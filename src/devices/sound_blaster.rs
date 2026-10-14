// SPDX-License-Identifier: GPL-3.0-or-later
//! PAL's 8-bit Sound Blaster DSP and ISA DMA channel 1 boundary.
//! No VOC/MKF parser or CPU interpreter: the original driver owns those layers.
//! Call advance_to BEFORE a timestamped port operation; times are nanoseconds.
//! Output is an independent 44100-Hz stereo source, including elapsed silence.
//! The caller drains and mixes it with CD-DA; no audio device is opened here.
#![forbid(unsafe_code)]
use crate::{
    runtime::Fault,
    semantics::{A20, GuestMemory},
};
use std::collections::VecDeque;

pub const OUTPUT_RATE: u64 = 44_100;
pub const MAX_QUEUED_FRAMES: usize = 44_100;
const NS_PER_SECOND: u64 = 1_000_000_000;

/// Extended physical DMA backing. Validation must be non-reading and require
/// a locked allocation; reads must use that allocation, never a RAM mirror.
/// A trait keeps the standalone RAM-only device usable without an XMS module.
pub trait DmaPhysicalMemory {
    fn validate_dma_span(&self, start: u32, len: usize) -> Result<(), Fault>;
    fn read_dma_byte(&self, address: u32) -> Result<u8, Fault>;
}

#[derive(Clone, Copy)]
struct DmaBus<'a> {
    extended: Option<&'a dyn DmaPhysicalMemory>,
}
impl DmaBus<'_> {
    fn ram_address(&self, memory: &GuestMemory, address: u32) -> u32 {
        // Compatibility-only RAM wrappers retain the historic CPU/A20 model.
        // Runtime selects the physical provider: ISA's 24-bit address is NOT
        // CPU A20-gated, even for unallocated/unlocked extended addresses.
        if self.extended.is_none() && memory.a20() == A20::Disabled {
            address & !(1 << 20)
        } else {
            address
        }
    }
    fn preflight_span(
        &self,
        memory: &GuestMemory,
        start: u32,
        len: u32,
        write: bool,
    ) -> Result<(), Fault> {
        if let Some(extended) = self.extended
            && start >= 0x100000
        {
            if write {
                return Err(Fault(
                    "SB identification DMA supports conventional RAM only".into(),
                ));
            }
            return extended.validate_dma_span(start, len as usize);
        }
        let start = self.ram_address(memory, start);
        let end = start
            .checked_add(len)
            .ok_or_else(|| Fault("SB DMA span overflow".into()))?;
        if end as usize > memory.len() {
            return Err(Fault(format!(
                "SB DMA1 outside guest memory at {:#x}",
                end - 1
            )));
        }
        // RAM DMA must not bypass the runtime VGA device bus.
        if start < 0xc0000 && end > 0xa0000 {
            return Err(Fault(format!(
                "SB DMA1 touches video aperture at {start:#x}"
            )));
        }
        Ok(())
    }
    fn read_byte(&self, memory: &GuestMemory, address: u32) -> Result<u8, Fault> {
        if let Some(extended) = self.extended {
            if address >= 0x100000 {
                return extended.read_dma_byte(address);
            }
            return memory
                .as_bytes()
                .get(address as usize)
                .copied()
                .ok_or_else(|| Fault("SB physical RAM DMA outside memory".into()));
        }
        Ok(memory.read_linear(address, 1)?[0])
    }
    fn write_byte(&self, memory: &mut GuestMemory, address: u32, value: u8) -> Result<(), Fault> {
        self.preflight_span(memory, address, 1, true)?;
        // Physical-provider writes are restricted to <1 MiB, where CPU A20
        // cannot change the address. The legacy wrapper keeps its old mapping.
        memory.write_linear(address, &[value])?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Dma1 {
    address: u16,
    count: u16,
    page: u8,
    mode: u8,
    masked: bool,
    high_byte: bool,
    terminal: bool,
    terminal_latched: bool,
}
impl Default for Dma1 {
    fn default() -> Self {
        Self {
            address: 0,
            count: 0,
            page: 0,
            mode: 0x49,
            masked: true,
            high_byte: false,
            terminal: false,
            terminal_latched: false,
        }
    }
}
impl Dma1 {
    fn linear_at(&self, offset: u32) -> u32 {
        (u32::from(self.page) << 16) | u32::from(self.address.wrapping_add(offset as u16))
    }
    fn available(&self) -> u32 {
        if self.terminal {
            0
        } else {
            u32::from(self.count) + 1
        }
    }
    fn consumed(&mut self) {
        self.address = self.address.wrapping_add(1);
        if self.count == 0 {
            self.terminal = true;
            self.terminal_latched = true;
            self.masked = true;
        }
        self.count = self.count.wrapping_sub(1);
    }
    /// Validate without reading device-backed memory or changing any latches.
    fn preflight(
        &self,
        memory: &GuestMemory,
        bus: DmaBus<'_>,
        len: u32,
        mode: u8,
    ) -> Result<(), Fault> {
        if self.mode != mode || len > self.available() {
            return Err(Fault(format!(
                "SB DMA1 mode/count mismatch: mode={:#x}, available={}, requested={len}",
                self.mode,
                self.available()
            )));
        }
        // ISA DMA wraps its byte counter inside a fixed 64-KiB page.
        // Provider selection determines A20 policy consistently for validation,
        // reads and E2 writes; validate both spans before consuming either.
        let page = u32::from(self.page) << 16;
        let first_len = len.min(0x10000 - u32::from(self.address));
        for (start, span_len) in [
            (page + u32::from(self.address), first_len),
            (page, len - first_len),
        ] {
            if span_len == 0 {
                continue;
            }
            bus.preflight_span(memory, start, span_len, mode == 0x45)?;
        }
        Ok(())
    }
}

/// FNV-1a over bytes actually read by ISA DMA, before DAC conversion or mixing.
/// Fixed-size metadata only; no source samples are retained or exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaSourceDiagnostic {
    pub physical_start: u32,
    pub transfer_bytes: u32,
    pub consumed_bytes: u32,
    pub fnv1a: u32,
    pub time_constant: u8,
    pub initial_dma_count: u16,
    pub start_ns: u64,
    pub completion_ns: Option<u64>,
}

#[derive(Clone, Debug)]
struct Playback {
    remaining: u32,
    period_ns: u64,
    next_ns: u64,
    frozen_delay_ns: u64,
    paused: bool,
    held_sample: i16,
}

/// Observable hardware position, not a queued/prefetched source position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub now_ns: u64,
    pub dma_address: u16,
    pub dma_count: u16,
    pub dma_page: u8,
    pub dma_masked: bool,
    pub dma_terminal: bool,
    pub playing: bool,
    pub paused: bool,
    pub speaker_on: bool,
    pub remaining_bytes: u32,
    pub consumed_bytes: u64,
    pub completed_transfers: u64,
    pub irq_pending: bool,
    pub last_completion_ns: Option<u64>,
    pub queued_frames: usize,
    pub last_playback_source: Option<DmaSourceDiagnostic>,
    pub first_completed_source: Option<DmaSourceDiagnostic>,
    pub last_completed_source: Option<DmaSourceDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct SoundBlaster {
    base: u16,
    dma: Dma1,
    reset_asserted: bool,
    command: Option<u8>,
    parameters: Vec<u8>,
    replies: VecDeque<u8>,
    test_register: u8,
    e2_value: u8,
    e2_phase: u8,
    pending_identification: Option<u8>,
    time_constant: u8,
    speaker_on: bool,
    playback: Option<Playback>,
    irq_pending: bool,
    now_ns: u64,
    output_frames: u64,
    pcm: VecDeque<[i16; 2]>,
    consumed_bytes: u64,
    completed_transfers: u64,
    last_completion_ns: Option<u64>,
    last_playback_source: Option<DmaSourceDiagnostic>,
    first_completed_source: Option<DmaSourceDiagnostic>,
    last_completed_source: Option<DmaSourceDiagnostic>,
}
impl Default for SoundBlaster {
    fn default() -> Self {
        Self::new(0x220).expect("valid default SB base")
    }
}
impl SoundBlaster {
    pub fn new(base: u16) -> Result<Self, Fault> {
        if !matches!(base, 0x210 | 0x220 | 0x230 | 0x240 | 0x250 | 0x260) {
            return Err(Fault(format!("unsupported SB base {base:#x}")));
        }
        Ok(Self {
            base,
            dma: Dma1::default(),
            reset_asserted: false,
            command: None,
            parameters: Vec::with_capacity(2),
            replies: VecDeque::new(),
            test_register: 0,
            e2_value: 0xaa,
            e2_phase: 0,
            pending_identification: None,
            time_constant: 45,
            speaker_on: false,
            playback: None,
            irq_pending: false,
            now_ns: 0,
            output_frames: 0,
            pcm: VecDeque::new(),
            consumed_bytes: 0,
            completed_transfers: 0,
            last_completion_ns: None,
            last_playback_source: None,
            first_completed_source: None,
            last_completed_source: None,
        })
    }
    pub fn base(&self) -> u16 {
        self.base
    }
    /// OPL (388/389) is deliberately not part of this device.
    pub fn handles_port(&self, port: u16) -> bool {
        matches!(port, 0x02 | 0x03 | 0x08 | 0x0a | 0x0b | 0x0c | 0x83)
            || [6, 10, 12, 14]
                .iter()
                .any(|&offset| port == self.base + offset)
    }
    /// Level IRQ7; the PIC must separately model request, in-service, and EOI.
    /// A PIC delivery does not acknowledge the DSP; IN(base+0e) does.
    pub fn irq_pending(&self) -> bool {
        self.irq_pending
    }
    pub fn irq(&self) -> u8 {
        7
    }
    pub fn status(&self) -> Status {
        Status {
            now_ns: self.now_ns,
            dma_address: self.dma.address,
            dma_count: self.dma.count,
            dma_page: self.dma.page,
            dma_masked: self.dma.masked,
            dma_terminal: self.dma.terminal,
            playing: self.playback.is_some(),
            paused: self
                .playback
                .as_ref()
                .is_some_and(|p| p.paused || (self.dma.masked && p.remaining > 0)),
            speaker_on: self.speaker_on,
            remaining_bytes: self.playback.as_ref().map_or(0, |p| p.remaining),
            consumed_bytes: self.consumed_bytes,
            completed_transfers: self.completed_transfers,
            irq_pending: self.irq_pending,
            last_completion_ns: self.last_completion_ns,
            queued_frames: self.pcm.len(),
            last_playback_source: self.last_playback_source,
            first_completed_source: self.first_completed_source,
            last_completed_source: self.last_completed_source,
        }
    }
    /// Absolute index of the first retained PCM frame. Independent IRQ/port
    /// advancement may render ahead of the output cursor, but must not move the
    /// consumer's FIFO start. A mismatch detects missing frames/another consumer.
    pub fn queued_frame_start(&self) -> u64 {
        self.output_frames - self.pcm.len() as u64
    }
    pub fn drain_frames(&mut self, destination: &mut [[i16; 2]]) -> usize {
        let n = destination.len().min(self.pcm.len());
        for frame in &mut destination[..n] {
            *frame = self.pcm.pop_front().unwrap();
        }
        n
    }
    pub fn read_port(&mut self, port: u16) -> Result<u8, Fault> {
        // Reset is write-only; PAL reads it four times solely for I/O delay.
        // Model the un-driven bus read, without consuming DSP replies or IRQ.
        if port == self.base + 6 {
            return Ok(0xff);
        }
        if port == self.base + 10 {
            return Ok(self.replies.pop_front().unwrap_or(0xff));
        }
        if port == self.base + 12 {
            return Ok(if self.reset_asserted { 0x80 } else { 0 });
        }
        if port == self.base + 14 {
            self.irq_pending = false;
            return Ok(if self.replies.is_empty() { 0 } else { 0x80 });
        }
        match port {
            0x02 | 0x03 => {
                let value = if port == 2 {
                    self.dma.address
                } else {
                    self.dma.count
                };
                let byte = if self.dma.high_byte {
                    (value >> 8) as u8
                } else {
                    value as u8
                };
                self.dma.high_byte = !self.dma.high_byte;
                Ok(byte)
            }
            0x08 => {
                let value = if self.dma.terminal_latched { 2 } else { 0 };
                self.dma.terminal_latched = false;
                // Terminal count masks the channel; reading status clears only TC indication.
                // Keep terminal separately to prevent an exhausted count from becoming 65536.
                Ok(value)
            }
            0x83 => Ok(self.dma.page),
            _ => Err(Fault(format!("unsupported SB/DMA IN {port:#x}"))),
        }
    }
    /// Legacy RAM-only caller: retains its historical A20-gated DMA policy.
    pub fn write_port(
        &mut self,
        port: u16,
        value: u8,
        memory: &mut GuestMemory,
    ) -> Result<(), Fault> {
        self.write_port_bus(port, value, memory, DmaBus { extended: None })
    }
    /// Physical ISA DMA caller, used by the production Machine with XMS.
    /// All 24 address bits are honored regardless of CPU A20. Addresses at or
    /// above 1 MiB MUST resolve to locked XMS; E2 writes stay in conventional
    /// RAM. Do not mix this provider with legacy wrappers during a transfer.
    pub fn write_port_with_xms(
        &mut self,
        port: u16,
        value: u8,
        memory: &mut GuestMemory,
        extended: &dyn DmaPhysicalMemory,
    ) -> Result<(), Fault> {
        self.write_port_bus(
            port,
            value,
            memory,
            DmaBus {
                extended: Some(extended),
            },
        )
    }
    fn preflight_sources(&self, memory: &GuestMemory, bus: DmaBus<'_>) -> Result<(), Fault> {
        if let Some(p) = &self.playback
            && p.remaining > 0
        {
            self.dma.preflight(memory, bus, p.remaining, 0x49)?;
        }
        if self.pending_identification.is_some() {
            self.dma.preflight(memory, bus, 1, 0x45)?;
        }
        Ok(())
    }
    /// Before a runtime's bounded advance/drain loop, reject a late unlock or
    /// invalid remaining source without partially advancing/draining its queue.
    pub fn preflight_with_xms(
        &self,
        memory: &GuestMemory,
        extended: &dyn DmaPhysicalMemory,
    ) -> Result<(), Fault> {
        self.preflight_sources(
            memory,
            DmaBus {
                extended: Some(extended),
            },
        )
    }
    fn write_port_bus(
        &mut self,
        port: u16,
        value: u8,
        memory: &mut GuestMemory,
        bus: DmaBus<'_>,
    ) -> Result<(), Fault> {
        self.preflight_sources(memory, bus)?;
        if port == self.base + 6 {
            match value {
                1 => {
                    self.reset_asserted = true;
                    self.command = None;
                    self.parameters.clear();
                    self.replies.clear();
                    self.playback = None;
                    self.pending_identification = None;
                    self.e2_value = 0xaa;
                    self.e2_phase = 0;
                    self.test_register = 0;
                    self.time_constant = 45;
                    self.speaker_on = false;
                    self.irq_pending = false;
                }
                0 => {
                    if self.reset_asserted {
                        self.replies.push_back(0xaa);
                    }
                    self.reset_asserted = false;
                }
                _ => return Err(Fault(format!("unsupported DSP reset value {value:#x}"))),
            }
            return Ok(());
        }
        if port == self.base + 12 {
            if self.reset_asserted {
                return Err(Fault("DSP command during reset".into()));
            }
            return self.write_dsp(value, memory, bus);
        }
        match port {
            0x0c => self.dma.high_byte = false,
            0x02 | 0x03 => {
                if !self.dma.masked {
                    return Err(Fault("DMA programming requires masked channel 1".into()));
                }
                let register = if port == 2 {
                    &mut self.dma.address
                } else {
                    &mut self.dma.count
                };
                *register = if self.dma.high_byte {
                    (*register & 0x00ff) | (u16::from(value) << 8)
                } else {
                    (*register & 0xff00) | u16::from(value)
                };
                self.dma.high_byte = !self.dma.high_byte;
                if port == 3 {
                    self.dma.terminal = false;
                    self.dma.terminal_latched = false;
                }
            }
            0x83 => {
                if !self.dma.masked {
                    return Err(Fault(
                        "DMA page programming requires masked channel 1".into(),
                    ));
                }
                self.dma.page = value;
            }
            0x0b => {
                if !matches!(value, 0x45 | 0x49) {
                    return Err(Fault(format!("unsupported DMA1 mode {value:#x}")));
                }
                if !self.dma.masked {
                    return Err(Fault(
                        "DMA mode programming requires masked channel 1".into(),
                    ));
                }
                self.dma.mode = value;
            }
            0x0a => {
                if !matches!(value, 1 | 5) {
                    return Err(Fault(format!("unsupported DMA mask command {value:#x}")));
                }
                let masked = value == 5;
                if !masked {
                    if self.pending_identification.is_some() {
                        self.dma.preflight(memory, bus, 1, 0x45)?;
                    }
                    if let Some(p) = &self.playback {
                        self.dma.preflight(memory, bus, p.remaining, 0x49)?;
                    }
                }
                let was_running = self.running();
                self.dma.masked = masked;
                self.update_gate(was_running)?;
                // PAL stops with DSP D0 followed by DMA mask (3b61:0330/06a7).
                // D0 alone remains resumable with D4; the two-part stop retires
                // the old DSP block so the guest may program its next block.
                if masked && self.playback.as_ref().is_some_and(|p| p.paused) {
                    self.playback = None;
                }
                self.flush_identification(memory, bus)?;
            }
            _ => return Err(Fault(format!("unsupported SB/DMA OUT {port:#x}"))),
        }
        Ok(())
    }
    fn running(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|p| !p.paused && (!self.dma.masked || p.remaining == 0))
    }
    fn update_gate(&mut self, was_running: bool) -> Result<(), Fault> {
        let running = self.running();
        if let Some(p) = &mut self.playback {
            if was_running && !running {
                p.frozen_delay_ns = p.next_ns.saturating_sub(self.now_ns);
            }
            if !was_running && running {
                p.next_ns = self
                    .now_ns
                    .checked_add(p.frozen_delay_ns)
                    .ok_or_else(|| Fault("SB resume time overflow".into()))?;
            }
        }
        Ok(())
    }
    fn flush_identification(
        &mut self,
        memory: &mut GuestMemory,
        bus: DmaBus<'_>,
    ) -> Result<(), Fault> {
        if !self.dma.masked
            && let Some(value) = self.pending_identification
        {
            self.dma.preflight(memory, bus, 1, 0x45)?;
            bus.write_byte(memory, self.dma.linear_at(0), value)?;
            self.dma.consumed();
            self.pending_identification = None;
        }
        Ok(())
    }
    fn write_dsp(
        &mut self,
        value: u8,
        memory: &mut GuestMemory,
        bus: DmaBus<'_>,
    ) -> Result<(), Fault> {
        if let Some(command) = self.command {
            let required = if command == 0x14 { 2 } else { 1 };
            if self.parameters.len() + 1 < required {
                self.parameters.push(value);
                return Ok(());
            }
            // Final parameter is committed only after the complete operation validates.
            match command {
                0x40 => self.time_constant = value,
                0xe0 => self.reply(&[!value])?,
                0xe4 => self.test_register = value,
                0xe2 => {
                    if self.pending_identification.is_some() || self.playback.is_some() {
                        return Err(Fault("overlapping SB DMA identification".into()));
                    }
                    self.dma.preflight(memory, bus, 1, 0x45)?;
                    // The four E2 phases add the input XOR 96/A5/69/5A modulo 256.
                    // Independently expressed hardware equation; format cross-check:
                    // DOSBox Staging v0.82.0 dsp_do_command/E2 (not a behavioral oracle).
                    let next = self
                        .e2_value
                        .wrapping_add(value ^ [0x96, 0xa5, 0x69, 0x5a][usize::from(self.e2_phase)]);
                    if !self.dma.masked {
                        bus.write_byte(memory, self.dma.linear_at(0), next)?;
                        self.dma.consumed();
                    } else {
                        self.pending_identification = Some(next);
                    }
                    self.e2_value = next;
                    self.e2_phase = (self.e2_phase + 1) & 3;
                }
                0x14 => {
                    if self
                        .playback
                        .as_ref()
                        .is_some_and(|p| !p.paused || !self.dma.masked)
                        || self.pending_identification.is_some()
                    {
                        return Err(Fault("overlapping SB single-cycle playback".into()));
                    }
                    let len = u32::from(u16::from_le_bytes([self.parameters[0], value])) + 1;
                    self.dma.preflight(memory, bus, len, 0x49)?;
                    let period_ns = (256 - u64::from(self.time_constant)) * 1000;
                    self.now_ns
                        .checked_add(u64::from(len) * period_ns)
                        .ok_or_else(|| Fault("SB playback end time overflow".into()))?;
                    self.last_playback_source = Some(DmaSourceDiagnostic {
                        physical_start: self.dma.linear_at(0),
                        transfer_bytes: len,
                        consumed_bytes: 0,
                        fnv1a: 0x811c9dc5,
                        time_constant: self.time_constant,
                        initial_dma_count: self.dma.count,
                        start_ns: self.now_ns,
                        completion_ns: None,
                    });
                    self.playback = Some(Playback {
                        remaining: len,
                        period_ns,
                        next_ns: self.now_ns,
                        frozen_delay_ns: 0,
                        paused: false,
                        held_sample: 0,
                    });
                }
                _ => unreachable!("only implemented parameterized commands are registered"),
            }
            self.command = None;
            self.parameters.clear();
            return Ok(());
        }
        match value {
            0x14 | 0x40 | 0xe0 | 0xe2 | 0xe4 => {
                self.command = Some(value);
                self.parameters.clear();
            }
            0xe1 => self.reply(&[2, 1])?, // SB 2.01: no OPL or SB16 capabilities advertised.
            0xe8 => self.reply(&[self.test_register])?,
            0xd1 => self.speaker_on = true,
            0xd3 => self.speaker_on = false,
            0xd0 | 0xd4 => {
                let was_running = self.running();
                if let Some(p) = &mut self.playback {
                    p.paused = value == 0xd0;
                }
                self.update_gate(was_running)?;
            }
            _ => return Err(Fault(format!("unsupported SB DSP command {value:#x}"))),
        }
        Ok(())
    }
    fn reply(&mut self, values: &[u8]) -> Result<(), Fault> {
        if self.replies.len() + values.len() > 16 {
            return Err(Fault("SB DSP response queue full".into()));
        }
        self.replies.extend(values);
        Ok(())
    }
    /// Advance DAC and DMA from elapsed time, never from host queue draining.
    /// First byte enters the DAC at start; the Nth byte finishes after N periods.
    /// A one-byte muted init still raises IRQ7 at exactly one source period.
    /// Full remaining DMA preflight and queue/time bounds precede ANY effects.
    /// Zero-order resampling uses an integer global 44100-Hz output timeline.
    /// Legacy RAM-only advance; see write_port for its compatibility A20 policy.
    pub fn advance_to(&mut self, target_ns: u64, memory: &mut GuestMemory) -> Result<(), Fault> {
        self.advance_to_bus(target_ns, memory, DmaBus { extended: None })
    }
    pub fn advance_to_with_xms(
        &mut self,
        target_ns: u64,
        memory: &mut GuestMemory,
        extended: &dyn DmaPhysicalMemory,
    ) -> Result<(), Fault> {
        self.advance_to_bus(
            target_ns,
            memory,
            DmaBus {
                extended: Some(extended),
            },
        )
    }
    fn advance_to_bus(
        &mut self,
        target_ns: u64,
        memory: &mut GuestMemory,
        bus: DmaBus<'_>,
    ) -> Result<(), Fault> {
        if target_ns < self.now_ns {
            return Err(Fault("SB clock moved backwards".into()));
        }
        let frames_u128 =
            u128::from(target_ns) * u128::from(OUTPUT_RATE) / u128::from(NS_PER_SECOND);
        let target_frames =
            u64::try_from(frames_u128).map_err(|_| Fault("SB output time overflow".into()))?;
        let count = target_frames - self.output_frames;
        if count > (MAX_QUEUED_FRAMES - self.pcm.len()) as u64 {
            return Err(Fault(
                "SB PCM queue/catch-up limit exceeded; advance in bounded drained slices".into(),
            ));
        }
        if let Some(p) = &self.playback {
            if p.remaining > 0 {
                self.dma.preflight(memory, bus, p.remaining, 0x49)?;
            }
            // Bound the last event even after pause/resume adjustments.
            p.next_ns
                .checked_add(u64::from(p.remaining) * p.period_ns)
                .ok_or_else(|| Fault("SB event time overflow".into()))?;
        }
        if self.pending_identification.is_some() {
            self.dma.preflight(memory, bus, 1, 0x45)?;
        }
        self.flush_identification(memory, bus)?;
        for index in self.output_frames..target_frames {
            // Sample at frame end, rounded upwards to ns. This timestamp is
            // strictly later than the preceding advance_to, even for tiny slices.
            let frame_ns = (u128::from(index + 1) * u128::from(NS_PER_SECOND))
                .div_ceil(u128::from(OUTPUT_RATE)) as u64;
            self.advance_events(frame_ns, memory, bus)?;
            let sample = if self.speaker_on && self.playback.as_ref().is_some_and(|p| !p.paused) {
                self.playback.as_ref().map_or(0, |p| p.held_sample)
            } else {
                0
            };
            self.pcm.push_back([sample, sample]);
        }
        self.advance_events(target_ns, memory, bus)?;
        self.output_frames = target_frames;
        self.now_ns = target_ns;
        Ok(())
    }
    fn advance_events(
        &mut self,
        target_ns: u64,
        memory: &mut GuestMemory,
        bus: DmaBus<'_>,
    ) -> Result<(), Fault> {
        while let Some(p) = &self.playback {
            // DMA TC may have masked the channel after the final byte. The final
            // DAC interval still runs to completion; it must not freeze IRQ7.
            if p.paused || (self.dma.masked && p.remaining > 0) || p.next_ns > target_ns {
                break;
            }
            if p.remaining == 0 {
                self.last_completion_ns = Some(p.next_ns);
                let source = self
                    .last_playback_source
                    .as_mut()
                    .expect("playback diagnostic");
                source.completion_ns = Some(p.next_ns);
                self.last_completed_source = Some(*source);
                if self.first_completed_source.is_none() {
                    self.first_completed_source = Some(*source);
                }
                self.completed_transfers += 1;
                self.irq_pending = true;
                self.playback = None;
                break;
            }
            let byte = bus.read_byte(memory, self.dma.linear_at(0))?;
            let source = self
                .last_playback_source
                .as_mut()
                .expect("playback diagnostic");
            source.fnv1a = (source.fnv1a ^ u32::from(byte)).wrapping_mul(0x01000193);
            source.consumed_bytes += 1;
            self.dma.consumed();
            self.consumed_bytes += 1;
            let p = self.playback.as_mut().unwrap();
            p.remaining -= 1;
            p.held_sample = (i16::from(byte) - 128) * 256;
            p.next_ns += p.period_ns;
        }
        Ok(())
    }
}

#[cfg(test)]
mod dma_source_tests {
    use super::*;

    #[test]
    fn authored_source_checksum_wrap_reset_and_completion() {
        let mut ram = GuestMemory::new(0x100000, A20::Enabled);
        ram.write_linear(0x1ffff, &[0]).unwrap();
        ram.write_linear(0x10000, &[128, 255]).unwrap();
        let mut sb = SoundBlaster::default();
        let program = |sb: &mut SoundBlaster, ram: &mut GuestMemory, at: u32, count: u16| {
            for (port, value) in [
                (0x0a, 5),
                (0x0c, 0),
                (0x0b, 0x49),
                (0x02, at as u8),
                (0x02, (at >> 8) as u8),
                (0x03, count as u8),
                (0x03, (count >> 8) as u8),
                (0x83, (at >> 16) as u8),
                (0x0a, 1),
                (0x22c, 0x40),
                (0x22c, 156),
                (0x22c, 0x14),
                (0x22c, count as u8),
                (0x22c, (count >> 8) as u8),
            ] {
                sb.write_port(port, value, ram).unwrap();
            }
        };
        program(&mut sb, &mut ram, 0x1ffff, 2);
        let start = sb.status().last_playback_source.unwrap();
        assert_eq!(
            (
                start.physical_start,
                start.transfer_bytes,
                start.consumed_bytes,
                start.fnv1a,
                start.completion_ns
            ),
            (0x1ffff, 3, 0, 0x811c9dc5, None)
        );
        sb.advance_to(0, &mut ram).unwrap();
        assert_eq!(sb.status().last_playback_source.unwrap().consumed_bytes, 1);
        assert!(sb.status().last_completed_source.is_none());
        sb.advance_to(300_000, &mut ram).unwrap();
        let completed = sb.status().last_completed_source.unwrap();
        // Independent fixed FNV-1a oracle for the authored bytes 00 80 FF.
        assert_eq!(
            (
                completed.fnv1a,
                completed.consumed_bytes,
                completed.completion_ns
            ),
            (0xb0eecb36, 3, Some(300_000))
        );
        assert_eq!(sb.status().first_completed_source, Some(completed));
        program(&mut sb, &mut ram, 0x10000, 0);
        assert_eq!(sb.status().last_playback_source.unwrap().fnv1a, 0x811c9dc5);
        assert_eq!(sb.status().last_completed_source, Some(completed));
        sb.advance_to(400_000, &mut ram).unwrap();
        assert_eq!(sb.status().last_completed_source.unwrap().consumed_bytes, 1);
        assert_eq!(sb.status().first_completed_source, Some(completed));
        assert_eq!(sb.status().completed_transfers, 2);
    }
}
