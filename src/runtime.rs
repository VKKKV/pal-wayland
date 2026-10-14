// SPDX-License-Identifier: GPL-3.0-or-later
//! Guest state, interrupt/device routing and services for statically generated code.
//! No instruction decoder or guest CPU fallback lives here; incomplete services fail explicitly.
// Private ABI implementation, not a separate state owner or public API.
#[path = "runtime/dos.rs"]
mod dos;

use crate::{mz::MzFile, semantics::*};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault(pub String);
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Fault {}
impl From<MemoryError> for Fault {
    fn from(value: MemoryError) -> Self {
        Self(value.to_string())
    }
}

/// All host resources are explicit. The asset mount is read-only; no host path
/// is taken directly from a DOS string. Save writes use a separate COW mount.
#[derive(Debug)]
pub struct Machine {
    pub regs: Registers,
    upper_registers: [u16; 8],
    pub memory: GuestMemory,
    pub steps: u64,
    /// Optional deterministic wall-clock fixture; otherwise UTC SystemTime.
    pub clock_centiseconds: Option<u32>,
    /// Explicit virtual hardware option; original software path stays default.
    pub x87_present: bool,
    pub waiting_for_key: bool,
    bios_keys: VecDeque<u16>,
    scan_codes: VecDeque<u8>,
    pub key_down: [bool; 128],
    pub exit_code: Option<u8>,
    pub console: Vec<u8>,
    pub service_trace: Vec<String>,
    pub psp: u16,
    pub load_segment: u16,
    pub conventional_top: u16,
    pub video_mode: u8,
    pub planar: std::cell::RefCell<crate::planar_video::PlanarVideo>,
    attribute_palette: [u8; 16],
    overscan: u8,
    cursor: u16,
    cursor_shape: u16,
    crtc_index: u8,
    crtc_cursor: u16,
    ctrl_break: bool,
    last_dos_error: u16,
    pub dac: crate::video::Dac,
    pic_masks: [u8; 2],
    pub pit_reload: u16,
    clock_started: std::time::Instant,
    // Explicit host execution scope; ordinary library callers keep fresh time.
    execution_clock: std::cell::Cell<Option<u64>>,
    execution_calls: std::cell::Cell<u8>,
    pub virtual_elapsed_ns: Option<u64>,
    presentation: std::cell::RefCell<crate::video::FramePublication>,
    pit_low: Option<u8>,
    pit_control: u8,
    timer_next_ns: u64,
    pub irq_shadow: u8,
    pub irq_deliveries: u64,
    pub irq_enabled: bool,
    pic_isr: [u8; 2],
    keyboard_control: u8,
    files: crate::dos_files::DosFiles,
    pub cdrom: Option<crate::cdrom::CdRom>,
    /// Explicit optional DSP/DMA device; never enabled by an audio backend.
    pub sound_blaster: Option<crate::sound_blaster::SoundBlaster>,
    /// Explicit absent OPL probe boundary, restricted to 388/389 only.
    pub opl_absent: bool,
    /// One capture/output consumer may retain PCM; otherwise discard it muted.
    pub retain_sfx_pcm: bool,
    pub sfx_discarded_frames: u64,
    pub xms: crate::xms::Xms,
    pub xms_entry_registered: bool,
    blocks: BTreeMap<u16, u16>,
}

impl Machine {
    pub fn from_mz(data: &[u8], load_segment: u16) -> Result<Self, Fault> {
        let mz = MzFile::parse(data).map_err(|e| Fault(e.to_string()))?;
        let loaded = mz.load(load_segment).map_err(|e| Fault(e.to_string()))?;
        // Reserve the low-memory host service area and environment block.
        let psp = load_segment
            .checked_sub(16)
            .filter(|&p| p >= 0x1000)
            .ok_or_else(|| Fault("runtime requires load segment >= 0x1010".into()))?;
        let top = 0x9f00_u16;
        let start = usize::from(load_segment) * 16;
        if start + loaded.memory.len() > usize::from(top) * 16 {
            return Err(Fault("image does not fit conventional guest RAM".into()));
        }
        let mut physical = vec![0; 0x10_0000];
        physical[start..start + loaded.memory.len()].copy_from_slice(&loaded.memory);
        let mut machine = Self {
            regs: Registers {
                cs: loaded.cs,
                ip: loaded.ip,
                ss: loaded.ss,
                sp: loaded.sp,
                ds: psp,
                es: psp,
                flags: Flags::from_bits(0x202),
                ..Registers::default()
            },
            upper_registers: [0; 8],
            memory: GuestMemory::from_bytes(physical, A20::Disabled),
            steps: 0,
            clock_centiseconds: None,
            x87_present: false,
            waiting_for_key: false,
            bios_keys: VecDeque::new(),
            scan_codes: VecDeque::new(),
            key_down: [false; 128],
            exit_code: None,
            console: Vec::new(),
            service_trace: Vec::new(),
            psp,
            load_segment,
            conventional_top: top,
            video_mode: 3,
            planar: std::cell::RefCell::new(crate::planar_video::PlanarVideo::new()),
            overscan: 0,
            attribute_palette: [
                0, 1, 2, 3, 4, 5, 0x14, 7, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
            ],
            cursor: 0,
            cursor_shape: 0x0607,
            crtc_index: 0,
            crtc_cursor: 0,
            ctrl_break: false,
            last_dos_error: 0,
            dac: crate::video::Dac::default(),
            pic_masks: [0xb8, 0xff], // BIOS-like IRQ0/1/2/6 enabled.
            pit_reload: 0,
            clock_started: std::time::Instant::now(),
            execution_clock: std::cell::Cell::new(None),
            execution_calls: std::cell::Cell::new(0),
            virtual_elapsed_ns: None,
            presentation: std::cell::RefCell::new(crate::video::FramePublication::default()),
            pit_low: None,
            pit_control: 0x36,
            timer_next_ns: 54_925_401,
            irq_shadow: 0,
            irq_deliveries: 0,
            irq_enabled: false,
            pic_isr: [0; 2],
            keyboard_control: 0,
            files: crate::dos_files::DosFiles::default(),
            cdrom: None,
            sound_blaster: None,
            opl_absent: false,
            retain_sfx_pcm: false,
            sfx_discarded_frames: 0,
            xms: crate::xms::Xms::default(),
            xms_entry_registered: false,
            blocks: BTreeMap::from([(psp, top - psp)]),
        };
        machine.write16(SegmentedAddress::new(psp, 0), 0x20cd)?;
        machine.write16(SegmentedAddress::new(psp, 2), top)?;
        machine.write16(SegmentedAddress::new(psp, 0x2c), 0x0800)?;
        machine.write8(SegmentedAddress::new(psp, 0x80), 0)?;
        machine.write8(SegmentedAddress::new(psp, 0x81), 13)?;
        machine.memory.write_bytes(
            SegmentedAddress::new(0x0800, 0),
            b"PATH=C:\\\0\0\x01\0C:\\PAL.EXE\0",
        )?;
        // BIOS conventional memory size, as a declared deterministic environment.
        machine.write16(SegmentedAddress::new(0, 0x413), top >> 6)?;
        crate::text_video::initialize(&mut machine.memory).map_err(Fault)?;
        machine.write8(SegmentedAddress::new(0x40, 0x49), 3)?;
        machine.write16(SegmentedAddress::new(0x40, 0x4a), 80)?;
        machine.write16(SegmentedAddress::new(0x40, 0x63), 0x3d4)?;
        Ok(machine)
    }

    /// Configure physical conventional RAM before startup. No EBDA is provided
    /// in this virtual environment; at most the actual 0xA0000 VGA boundary.
    pub fn configure_conventional_top(&mut self, top: u16) -> Result<(), Fault> {
        if self.steps != 0 || top < self.conventional_top || top > 0xa000 {
            return Err(self.fault("RAM top must be configured before startup, <=640 KiB"));
        }
        self.conventional_top = top;
        self.blocks.insert(self.psp, top - self.psp);
        self.write16(SegmentedAddress::new(self.psp, 2), top)?;
        self.write16(SegmentedAddress::new(0, 0x413), top >> 6)?;
        Ok(())
    }
    pub fn mount_assets(&mut self, root: &Path) -> Result<(), Fault> {
        self.files.mount_assets(root).map_err(Fault)
    }
    pub fn mount_saves(&mut self, root: &Path) -> Result<(), Fault> {
        self.files.mount_saves(root).map_err(Fault)
    }

    pub fn fault(&self, message: impl Into<String>) -> Fault {
        Fault(format!(
            "{:04x}:{:04x} step={} AX={:04x} BX={:04x} CX={:04x} DX={:04x} DS={:04x} ES={:04x} SS:SP={:04x}:{:04x}: {}",
            self.regs.cs,
            self.regs.ip,
            self.steps,
            self.regs.ax,
            self.regs.bx,
            self.regs.cx,
            self.regs.dx,
            self.regs.ds,
            self.regs.es,
            self.regs.ss,
            self.regs.sp,
            message.into()
        ))
    }
    /// Compare bytes, never decode them. Modified code cannot silently execute
    /// its stale translation. A mismatch is a hard diagnostic, not a JIT request.
    pub fn guard_code(&self, expected: &[u8]) -> Result<(), Fault> {
        self.validate_ram_span(
            SegmentedAddress::new(self.regs.cs, self.regs.ip),
            expected.len(),
        )?;
        // Most instructions fit the architectural 15-byte ceiling. Keep the
        // uncommon longer macro guard intact without allocating per CPU step.
        let mut stack = [0u8; 15];
        let mut longer = Vec::new();
        let actual: &mut [u8] = if expected.len() <= stack.len() {
            &mut stack[..expected.len()]
        } else {
            longer.resize(expected.len(), 0);
            &mut longer
        };
        self.memory
            .read_bytes(SegmentedAddress::new(self.regs.cs, self.regs.ip), actual)?;
        if actual != expected {
            return Err(self.fault("code changed since static translation"));
        }
        Ok(())
    }
    /// 386 operand-size extension. Legacy 16-bit writes preserve upper halves.
    /// Set-1 input from the explicit host adapter. Repeats are separate key
    /// presses; queue capacity is bounded and oldest input is retained.
    pub fn key_event(&mut self, scan: u8, ascii: u8, pressed: bool) {
        if scan >= 128 {
            return;
        }
        self.key_down[usize::from(scan)] = pressed;
        if self.scan_codes.len() < 256 {
            self.scan_codes
                .push_back(scan | if pressed { 0 } else { 0x80 });
        }
        if pressed && self.bios_keys.len() < 32 {
            self.bios_keys
                .push_back((u16::from(scan) << 8) | u16::from(ascii));
            self.waiting_for_key = false;
        }
    }
    pub fn get32(&self, index: u8) -> u32 {
        let low = match index {
            0 => self.regs.ax,
            1 => self.regs.cx,
            2 => self.regs.dx,
            3 => self.regs.bx,
            4 => self.regs.sp,
            5 => self.regs.bp,
            6 => self.regs.si,
            7 => self.regs.di,
            _ => panic!("invalid statically generated register index"),
        };
        (u32::from(self.upper_registers[usize::from(index)]) << 16) | u32::from(low)
    }
    pub fn set32(&mut self, index: u8, value: u32) {
        let low = value as u16;
        match index {
            0 => self.regs.ax = low,
            1 => self.regs.cx = low,
            2 => self.regs.dx = low,
            3 => self.regs.bx = low,
            4 => self.regs.sp = low,
            5 => self.regs.bp = low,
            6 => self.regs.si = low,
            7 => self.regs.di = low,
            _ => panic!("invalid statically generated register index"),
        }
        self.upper_registers[usize::from(index)] = (value >> 16) as u16;
    }
    /// Side-effect-free bus preflight. Never loads VGA latches.
    pub fn validate_span(&self, address: SegmentedAddress, len: usize) -> Result<(), Fault> {
        self.planar_address(address, len)?;
        self.validate_memory_span(address, len)
    }
    fn validate_memory_span(&self, address: SegmentedAddress, len: usize) -> Result<(), Fault> {
        if usize::from(address.offset)
            .checked_add(len)
            .is_none_or(|n| n > 65536)
        {
            return Err(self.fault("access crosses segment boundary"));
        }
        for i in 0..len {
            let at = SegmentedAddress::new(address.segment, address.offset + i as u16);
            if self.memory.physical_address(at) >= self.memory.len() {
                return Err(self.fault("memory access out of bounds"));
            }
        }
        Ok(())
    }
    pub fn validate_ram_span(&self, address: SegmentedAddress, len: usize) -> Result<(), Fault> {
        self.validate_memory_span(address, len)?;
        if self.planar_address(address, len)?.is_some() {
            return Err(self.fault("RAM-only access intersects planar aperture"));
        }
        Ok(())
    }
    fn planar_address(&self, address: SegmentedAddress, len: usize) -> Result<Option<u32>, Fault> {
        if self.video_mode != 0x12 {
            return Ok(None);
        }
        self.validate_memory_span(address, len)?;
        let mut planes = 0usize;
        let start = self.memory.physical_address(address);
        for i in 0..len {
            let p = self.memory.physical_address(SegmentedAddress::new(
                address.segment,
                address.offset + i as u16,
            ));
            if (0xa0000..0xb0000).contains(&p) {
                planes += 1;
            }
        }
        if planes == 0 {
            return Ok(None);
        }
        if planes != len {
            return Err(self.fault("mixed RAM/planar access unsupported"));
        }
        Ok(Some(start as u32))
    }
    pub fn pop16(&mut self) -> Result<u16, Fault> {
        self.validate_ram_span(SegmentedAddress::new(self.regs.ss, self.regs.sp), 2)?;
        Ok(self.regs.pop16(&self.memory)?)
    }
    pub fn far_return(&mut self, cleanup: u16) -> Result<(), Fault> {
        for i in [0, 2] {
            self.validate_ram_span(
                SegmentedAddress::new(self.regs.ss, self.regs.sp.wrapping_add(i)),
                2,
            )?;
        }
        self.regs.far_return_with_cleanup(&self.memory, cleanup)?;
        Ok(())
    }
    pub fn leave16(&mut self) -> Result<(), Fault> {
        let at = SegmentedAddress::new(self.regs.ss, self.regs.bp);
        self.validate_ram_span(at, 2)?;
        let bp = self.memory.read16(at)?;
        self.regs.sp = self.regs.bp.wrapping_add(2);
        self.regs.bp = bp;
        Ok(())
    }
    pub fn xms_call(&mut self) -> Result<(), Fault> {
        if self.video_mode == 0x12 {
            self.xms
                .preflight_ram_call(&self.regs, &self.memory, 0xa0000..0xb0000)?;
        }
        self.xms.call(&mut self.regs, &mut self.memory)
    }
    pub fn read32(&self, address: SegmentedAddress) -> Result<u32, Fault> {
        if let Some(p) = self.planar_address(address, 4)? {
            return self.planar.borrow_mut().read32(p).map_err(Fault);
        }
        let mut b = [0; 4];
        self.memory.read_bytes(address, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    pub fn write32(&mut self, address: SegmentedAddress, value: u32) -> Result<(), Fault> {
        if let Some(p) = self.planar_address(address, 4)? {
            return self.planar.borrow_mut().write32(p, value).map_err(Fault);
        }
        self.memory.write_bytes(address, &value.to_le_bytes())?;
        Ok(())
    }
    pub fn read8(&self, address: SegmentedAddress) -> Result<u8, Fault> {
        if let Some(p) = self.planar_address(address, 1)? {
            return self.planar.borrow_mut().read8(p).map_err(Fault);
        }
        Ok(self.memory.read8(address)?)
    }
    pub fn read16(&self, address: SegmentedAddress) -> Result<u16, Fault> {
        if let Some(p) = self.planar_address(address, 2)? {
            return self.planar.borrow_mut().read16(p).map_err(Fault);
        }
        Ok(self.memory.read16(address)?)
    }
    pub fn write8(&mut self, address: SegmentedAddress, value: u8) -> Result<(), Fault> {
        if let Some(p) = self.planar_address(address, 1)? {
            return self.planar.borrow_mut().write8(p, value).map_err(Fault);
        }
        Ok(self.memory.write8(address, value)?)
    }
    pub fn write16(&mut self, address: SegmentedAddress, value: u16) -> Result<(), Fault> {
        if let Some(p) = self.planar_address(address, 2)? {
            return self.planar.borrow_mut().write16(p, value).map_err(Fault);
        }
        Ok(self.memory.write16(address, value)?)
    }
    pub fn read_bytes(&self, address: SegmentedAddress, out: &mut [u8]) -> Result<(), Fault> {
        if let Some(p) = self.planar_address(address, out.len())? {
            return self.planar.borrow_mut().read_linear(p, out).map_err(Fault);
        }
        self.memory.read_bytes(address, out)?;
        Ok(())
    }
    pub fn write_bytes(&mut self, address: SegmentedAddress, data: &[u8]) -> Result<(), Fault> {
        if let Some(p) = self.planar_address(address, data.len())? {
            return self
                .planar
                .borrow_mut()
                .write_linear(p, data)
                .map_err(Fault);
        }
        self.memory.write_bytes(address, data)?;
        Ok(())
    }
    /// Emitted only for a verified OUT DX,AL paired with a 16-bit CX LOOP.
    pub fn palette_stream_out(&mut self, loop_ip: u16) -> Result<(), Fault> {
        let owner = crate::video::PaletteOwner {
            cs: self.regs.cs,
            out_ip: self.regs.ip,
            loop_ip,
        };
        self.port_out_owned(self.regs.dx, self.regs.al().into(), 8, Some(owner))
    }
    pub fn finish_palette_stream(&mut self, cs: u16, out_ip: u16, loop_ip: u16, target_ip: u16) {
        self.dac.finish_palette_stream(
            crate::video::PaletteOwner {
                cs,
                out_ip,
                loop_ip,
            },
            self.regs.cx,
            self.regs.cs == cs && self.regs.ip == target_ip,
        );
    }
    /// Called only after the selected AOT instruction's exact guard succeeds.
    /// Recognition uses operand effect, never a game-specific address/opcode.
    pub fn begin_vram_rep(&self, width: usize) -> Result<(), Fault> {
        if self.video_mode != 0x13 || self.presentation.borrow().active() {
            return Ok(());
        }
        let count = usize::from(self.regs.cx);
        let dest = usize::from(self.regs.es) * 16 + usize::from(self.regs.di);
        let full = count.checked_mul(width) == Some(crate::video::FRAME_BYTES)
            && if self.regs.flags.contains(Flags::DF) {
                dest + width == crate::video::VGA_BASE + crate::video::FRAME_BYTES
                    && usize::from(self.regs.di) + width >= crate::video::FRAME_BYTES
            } else {
                dest == crate::video::VGA_BASE
                    && usize::from(self.regs.di) + crate::video::FRAME_BYTES <= 65536
            };
        if full {
            self.presentation.borrow_mut().begin(
                (self.regs.cs, self.regs.ip),
                self.dac
                    .presentation_indexed_frame(&self.memory)
                    .map_err(Fault)?,
            );
            self.refresh_execution_clock();
        }
        Ok(())
    }
    pub fn finish_vram_rep(&self, cs: u16, ip: u16) -> Result<(), Fault> {
        if self.presentation.borrow().active() && self.regs.cx == 0 {
            self.presentation.borrow_mut().finish((cs, ip));
            self.refresh_execution_clock();
        }
        Ok(())
    }
    /// Complete mode-13h publication; `frame` remains a raw diagnostic view.
    pub fn presentation_indexed_frame(&self) -> Result<crate::video::IndexedFrame, Fault> {
        if self.video_mode != 0x13 {
            return Err(self.fault("indexed presentation requires mode13"));
        }
        self.presentation
            .borrow_mut()
            .indexed_frame(|| self.dac.presentation_indexed_frame(&self.memory))
            .map_err(Fault)
    }
    /// CPU compatibility expands the same immutable indexed publication.
    pub fn presentation_frame(&self) -> Result<Vec<u32>, Fault> {
        self.presentation_indexed_frame()?
            .rgb_pixels()
            .map_err(Fault)
    }
    pub fn frame(&self) -> Result<(usize, usize, Vec<u32>), Fault> {
        match self.video_mode {
            0x13 => Ok((320, 200, self.dac.frame(&self.memory).map_err(Fault)?)),
            0x12 => Ok((
                640,
                480,
                self.planar
                    .borrow()
                    .frame_with_palette(&std::array::from_fn(|i| {
                        let c = self.attribute_palette[i];
                        let r = (u32::from(c & 4 != 0) * 170) + (u32::from(c & 0x20 != 0) * 85);
                        let g = (u32::from(c & 2 != 0) * 170) + (u32::from(c & 0x10 != 0) * 85);
                        let b = (u32::from(c & 1 != 0) * 170) + (u32::from(c & 8 != 0) * 85);
                        (r << 16) | (g << 8) | b
                    })),
            )),
            _ => Err(self.fault("no supported graphics framebuffer selected")),
        }
    }
    /// 80386 real-mode 16-bit FLAGS image: bit 1 set, bits 3/5/15 clear.
    /// IOPL/NT are stored; virtual-8086/protected mode is not implemented.
    pub fn normalize_flags(value: u16) -> u16 {
        (value & 0x7fd5) | 2
    }

    /// Execute a statically decoded INT. A non-null guest vector always wins
    /// over the default host shim. The generated dispatcher must subsequently
    /// contain that target; otherwise it fails, never silently takes host code.
    pub fn software_interrupt(&mut self, number: u8, next_ip: u16) -> Result<(), Fault> {
        let vector = SegmentedAddress::new(0, u16::from(number) * 4);
        let ip = self.read16(vector)?;
        let cs = self.read16(vector.wrapping_add(2))?;
        if (cs, ip) == (0, 0) {
            self.interrupt(number)?;
            if self.waiting_for_key {
                return Ok(());
            }
            self.regs.ip = next_ip;
            return Ok(());
        }
        let old = self.regs;
        let values = [Self::normalize_flags(old.flags.bits()), old.cs, next_ip];
        // Preflight all stores before changing memory or registers.
        for i in 1..=3 {
            self.validate_ram_span(SegmentedAddress::new(old.ss, old.sp.wrapping_sub(i * 2)), 2)?;
        }
        for (i, value) in values.into_iter().enumerate() {
            self.write16(
                SegmentedAddress::new(old.ss, old.sp.wrapping_sub((i as u16 + 1) * 2)),
                value,
            )?;
        }
        self.regs.sp = old.sp.wrapping_sub(6);
        self.regs.flags =
            Flags::from_bits(Self::normalize_flags(old.flags.bits()) & !(Flags::IF | Flags::TF));
        self.regs.cs = cs;
        self.regs.ip = ip;
        Ok(())
    }
    pub fn interrupt_return(&mut self) -> Result<(), Fault> {
        let at = SegmentedAddress::new(self.regs.ss, self.regs.sp);
        for i in [0, 2, 4] {
            self.validate_ram_span(at.wrapping_add(i), 2)?;
        }
        let ip = self.read16(at)?;
        let cs = self.read16(at.wrapping_add(2))?;
        let flags = self.read16(at.wrapping_add(4))?;
        self.regs.ip = ip;
        self.regs.cs = cs;
        self.regs.flags = Flags::from_bits(Self::normalize_flags(flags));
        self.regs.sp = self.regs.sp.wrapping_add(6);
        Ok(())
    }
    /// Invoke a default platform service directly (test/host boundary only).
    /// Translated INT instructions must call software_interrupt instead.
    pub fn interrupt(&mut self, number: u8) -> Result<(), Fault> {
        let vector = SegmentedAddress::new(0, u16::from(number) * 4);
        if self.read16(vector)? != 0 || self.read16(vector.wrapping_add(2))? != 0 {
            return Err(self.fault("guest interrupt vector installed; host bypass forbidden"));
        }
        if self.service_trace.len() < 4096 {
            self.service_trace.push(format!(
                "{:04x}:{:04x} INT {number:02x} AX={:04x} BX={:04x} CX={:04x} DX={:04x}",
                self.regs.cs, self.regs.ip, self.regs.ax, self.regs.bx, self.regs.cx, self.regs.dx
            ));
        }
        match number {
            0x20 => {
                self.exit_code = Some(0);
                Ok(())
            }
            0x16 => {
                match self.regs.ah() {
                    0x00 | 0x10 => {
                        if let Some(value) = self.bios_keys.pop_front() {
                            self.regs.ax = value;
                            self.waiting_for_key = false;
                        } else {
                            self.waiting_for_key = true;
                        }
                    }
                    0x01 | 0x11 => {
                        let next = self.bios_keys.front().copied();
                        self.regs.flags.set(Flags::ZF, next.is_none());
                        if let Some(value) = next {
                            self.regs.ax = value;
                        }
                    }
                    0x02 => {
                        self.regs.set_al(
                            u8::from(self.key_down[0x36])
                                | u8::from(self.key_down[0x2a]) << 1
                                | u8::from(self.key_down[0x1d]) << 2
                                | u8::from(self.key_down[0x38]) << 3,
                        );
                    }
                    _ => return Err(self.fault("unsupported BIOS keyboard service")),
                }
                Ok(())
            }
            0x2f if self.regs.ax == 0x4300 => {
                self.regs.set_al(0x80);
                Ok(())
            }
            0x2f if self.regs.ax == 0x4310 => {
                self.regs.es = 0xf000;
                self.regs.bx = 0x100;
                self.xms_entry_registered = true;
                Ok(())
            }
            0x2f if self.regs.ax == 0x1500 => {
                self.regs.bx = u16::from(self.cdrom.is_some());
                if self.cdrom.is_some() {
                    self.regs.cx = 3;
                }
                Ok(())
            }
            0x2f if self.regs.ax == 0x1510 => {
                if self.regs.cx != 3 || self.cdrom.is_none() {
                    self.dos_error(15);
                    return Ok(());
                }
                let at = SegmentedAddress::new(self.regs.es, self.regs.bx);
                if self.video_mode == 0x12 {
                    self.cdrom.as_ref().unwrap().preflight_ram_request(
                        &self.memory,
                        at,
                        0xa0000..0xb0000,
                    )?;
                }
                self.cdrom.as_mut().unwrap().request(
                    &mut self.memory,
                    SegmentedAddress::new(self.regs.es, self.regs.bx),
                )?;
                self.success();
                Ok(())
            }
            0x21 => self.dos(),
            // Explicitly offline guest: no network redirector is installed.
            0x2a if self.regs.ah() == 0 => Ok(()),
            0x11 => {
                // Declared virtual equipment: 80-column color, one floppy,
                // no x87. This is a host model, not detected physical hardware.
                self.regs.ax = 0x0021 | if self.x87_present { 2 } else { 0 };
                Ok(())
            }
            0x10 if self.regs.ax == 0x0500 => Ok(()), // Only page 0 exists.
            0x10 if self.regs.ah() == 1 => {
                self.cursor_shape = self.regs.cx;
                Ok(())
            }
            0x10 if self.regs.ah() == 2 && self.regs.bx >> 8 == 0 => {
                if self.video_mode == 3 {
                    crate::text_video::validate_cursor(self.regs.dx).map_err(Fault)?;
                }
                self.read16(SegmentedAddress::new(0x40, 0x50))?;
                self.cursor = self.regs.dx;
                self.memory
                    .write16(SegmentedAddress::new(0x40, 0x50), self.cursor)?;
                Ok(())
            }
            0x10 if self.regs.ah() == 3 && self.regs.bx >> 8 == 0 => {
                self.regs.cx = self.cursor_shape;
                self.regs.dx = self.cursor;
                Ok(())
            }
            0x10 if self.regs.ax == 0x1002 => {
                let at = SegmentedAddress::new(self.regs.es, self.regs.dx);
                let mut colors = [0; 17];
                self.read_bytes(at, &mut colors)?;
                if colors.iter().any(|c| *c > 63) {
                    return Err(self.fault("invalid attribute palette table"));
                }
                self.attribute_palette.copy_from_slice(&colors[..16]);
                self.overscan = colors[16];
                Ok(())
            }
            0x10 if self.regs.ax == 0x1000 => {
                let index = self.regs.bx as u8;
                let value = (self.regs.bx >> 8) as u8;
                if index >= 16 || value > 63 {
                    return Err(self.fault("invalid attribute palette register"));
                }
                self.attribute_palette[index as usize] = value;
                Ok(())
            }
            0x10 if self.regs.ah() == 8 => {
                self.regs.ax = crate::text_video::read_cell(
                    &self.memory,
                    self.video_mode,
                    (self.regs.bx >> 8) as u8,
                    self.cursor,
                )
                .map_err(Fault)?;
                Ok(())
            }
            0x10 if matches!(self.regs.ah(), 9 | 10) => {
                let attr = if self.regs.ah() == 9 {
                    Some(self.regs.bx as u8)
                } else {
                    None
                };
                crate::text_video::write_repeat(
                    &mut self.memory,
                    self.video_mode,
                    (self.regs.bx >> 8) as u8,
                    self.cursor,
                    self.regs.al(),
                    attr,
                    self.regs.cx,
                )
                .map_err(Fault)?;
                Ok(())
            }
            0x10 if matches!(self.regs.ah(), 6 | 7) => {
                let rect = crate::text_video::Rect {
                    top: (self.regs.cx >> 8) as u8,
                    left: self.regs.cx as u8,
                    bottom: (self.regs.dx >> 8) as u8,
                    right: self.regs.dx as u8,
                };
                let direction = if self.regs.ah() == 6 {
                    crate::text_video::ScrollDirection::Up
                } else {
                    crate::text_video::ScrollDirection::Down
                };
                crate::text_video::scroll(
                    &mut self.memory,
                    self.video_mode,
                    0,
                    rect,
                    self.regs.al(),
                    (self.regs.bx >> 8) as u8,
                    direction,
                )
                .map_err(Fault)?;
                Ok(())
            }
            0x10 if self.regs.ah() == 0x0e => {
                self.read16(SegmentedAddress::new(0x40, 0x50))?;
                let mut cursor = self.cursor;
                crate::text_video::teletype(
                    &mut self.memory,
                    self.video_mode,
                    (self.regs.bx >> 8) as u8,
                    &mut cursor,
                    self.regs.al(),
                )
                .map_err(Fault)?;
                self.write16(SegmentedAddress::new(0x40, 0x50), cursor)?;
                self.cursor = cursor;
                Ok(())
            }
            0x10 if self.regs.ax == 0x1130 && self.regs.bx >> 8 <= 1 => {
                // BH=0/1 requests the current vector, not a built-in ROM font.
                // No ROM font is synthesized. A null vector remains null until
                // explicitly installed by guest/platform font initialization.
                let vector = if self.regs.bx >> 8 == 0 { 0x1f } else { 0x43 };
                self.regs.bp = self.read16(SegmentedAddress::new(0, vector * 4))?;
                self.regs.es = self.read16(SegmentedAddress::new(0, vector * 4 + 2))?;
                self.regs.cx = if self.video_mode == 0x13 { 8 } else { 16 };
                self.regs.dx = (self.regs.dx & 0xff00) | 24;
                Ok(())
            }
            0x10 if self.regs.ah() == 0xef => {
                self.regs.dx = 0xffff; // No Hercules adapter/driver.
                Ok(())
            }
            0x10 if self.regs.ah() == 0x1b && self.regs.bx == 0 => {
                // RBIL tables 00040/00045. Advertise only our two implemented
                // modes and no font/save-state extensions. Tables are in guest RAM.
                let table = SegmentedAddress::new(0x0800, 0x0100);
                let mut capabilities = [0u8; 16];
                capabilities[0] = 1 << 3;
                capabilities[2] = 1 << 3;
                capabilities[7] = 5; // 200 and 400 scan lines.
                capabilities[8] = 1;
                capabilities[9] = 1;
                let mut state = [0u8; 64];
                state[0..2].copy_from_slice(&table.offset.to_le_bytes());
                state[2..4].copy_from_slice(&table.segment.to_le_bytes());
                state[4] = self.video_mode;
                let graphics = self.video_mode == 0x13;
                state[5..7].copy_from_slice(&(if graphics { 40u16 } else { 80u16 }).to_le_bytes());
                state[7..9]
                    .copy_from_slice(&(if graphics { 64000u16 } else { 4096u16 }).to_le_bytes());
                state[0x0b..0x0d].copy_from_slice(&self.cursor.to_le_bytes());
                state[0x1b..0x1d].copy_from_slice(&self.cursor_shape.to_le_bytes());
                state[0x1e..0x20].copy_from_slice(&0x03d4u16.to_le_bytes());
                state[0x22] = 24;
                state[0x23] = if graphics { 8 } else { 16 };
                state[0x25] = 8; // VGA color.
                state[0x27..0x29]
                    .copy_from_slice(&(if graphics { 256u16 } else { 16u16 }).to_le_bytes());
                state[0x29] = 1;
                state[0x2a] = if graphics { 0 } else { 2 };
                state[0x33] = 4;
                // Prevalidate caller buffer before either memory write.
                let destination = SegmentedAddress::new(self.regs.es, self.regs.di);
                self.validate_ram_span(destination, 64)?;
                self.memory.write_bytes(table, &capabilities)?;
                self.memory.write_bytes(destination, &state)?;
                self.regs.set_al(0x1b);
                Ok(())
            }
            0x10 if self.regs.ah() == 0x12 && self.regs.bx as u8 == 0x10 => {
                // Declared color VGA with 256KiB (four64KiB planes). Feature/
                // switch values describe EGA-compatible color enhanced display.
                self.regs.bx = 3;
                self.regs.cx = 9;
                Ok(())
            }
            0x10 if self.regs.ax == 0x1a00 => {
                self.regs.set_al(0x1a);
                self.regs.bx = 8;
                Ok(())
            }
            0x10 if self.regs.ah() == 0 => {
                let mode = self.regs.al() & 0x7f;
                if ![3, 0x12, 0x13].contains(&mode) {
                    return Err(self.fault(format!("unsupported BIOS video mode {mode:02x}")));
                }
                self.read16(SegmentedAddress::new(0x40, 0x50))?;
                self.read8(SegmentedAddress::new(0x40, 0x49))?;
                if mode == 0x12 {
                    self.planar
                        .borrow_mut()
                        .set_mode(self.regs.al())
                        .map_err(Fault)?;
                    self.attribute_palette = [
                        0, 1, 2, 3, 4, 5, 0x14, 7, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
                    ];
                    self.overscan = 0;
                }
                if self.regs.al() & 0x80 == 0 && mode == 3 {
                    crate::text_video::initialize(&mut self.memory).map_err(Fault)?;
                }
                if self.regs.al() & 0x80 == 0 && mode == 0x13 {
                    self.memory
                        .write_bytes(SegmentedAddress::new(0xa000, 0), &vec![0; 64000])?;
                }
                self.memory
                    .write8(SegmentedAddress::new(0x0040, 0x0049), mode)?;
                self.video_mode = mode;
                *self.presentation.borrow_mut() = crate::video::FramePublication::default();
                self.cursor = 0;
                self.write16(SegmentedAddress::new(0x40, 0x50), 0)?;
                Ok(())
            }
            0x10 if self.regs.ah() == 0x0f => {
                self.regs.ax = (if self.video_mode == 0x13 {
                    0x2800
                } else {
                    0x5000
                }) | u16::from(self.video_mode);
                self.regs.bx &= 0xff;
                Ok(())
            }
            0x12 => {
                self.regs.ax = self.conventional_top >> 6;
                Ok(())
            }
            _ => Err(self.fault(format!("unsupported interrupt {number:02x}"))),
        }
    }
    /// Schedule at most one real PIT IRQ between translated instructions. IRQ
    /// coalesces while masked/IF clear (8259 edge latch), never stacks unbounded.
    pub fn enable_irqs(&mut self) -> Result<(), Fault> {
        self.irq_enabled = true;
        if self.read32(SegmentedAddress::new(0, 32))? == 0 {
            self.write16(SegmentedAddress::new(0, 32), 0x200)?;
            self.write16(SegmentedAddress::new(0, 34), 0xf000)?;
        }
        Ok(())
    }
    pub fn bios_timer_return(&mut self) -> Result<(), Fault> {
        let at = SegmentedAddress::new(0x40, 0x6c);
        let ticks = self.read32(at)?.wrapping_add(1);
        self.write32(at, ticks)?;
        self.port_out(0x20, 0x20, 8)?;
        self.interrupt_return()
    }
    pub fn poll_timer_irq(&mut self) -> Result<bool, Fault> {
        // Hardware time advances even with IF clear, shadow or delivery disabled.
        if self.execution_clock.get().is_some() {
            let calls = self.execution_calls.get();
            if calls == 32 {
                self.refresh_execution_clock();
                self.execution_calls.set(1);
            } else {
                self.execution_calls.set(calls + 1);
            }
        }
        let now = self.elapsed_ns();
        self.advance_sound_to(now)?;
        if !self.irq_enabled {
            return Ok(false);
        }

        if self.irq_shadow != 0 {
            self.irq_shadow -= 1;
            return Ok(false);
        }
        if !self.regs.flags.contains(Flags::IF) {
            return Ok(false);
        }
        // Fixed fully nested priority: only requests above the highest active
        // ISR bit may enter. Timer IRQ0 outranks keyboard IRQ1.
        let priority = self.pic_isr[0].trailing_zeros();
        let irq = if now >= self.timer_next_ns && self.pic_masks[0] & 1 == 0 && priority > 0 {
            0
        } else if !self.scan_codes.is_empty() && self.pic_masks[0] & 2 == 0 && priority > 1 {
            1
        } else if self.sound_blaster.as_ref().is_some_and(|s| s.irq_pending())
            && self.pic_masks[0] & 0x80 == 0
            && priority > 7
        {
            7
        } else {
            return Ok(false);
        };
        let vector = SegmentedAddress::new(0, (8 + irq) * 4);
        if self.read32(vector)? == 0 {
            return Ok(false);
        }
        self.software_interrupt((8 + irq) as u8, self.regs.ip)?;
        self.pic_isr[0] |= 1 << irq;
        if irq == 0 {
            let divisor = if self.pit_reload == 0 {
                65536
            } else {
                u64::from(self.pit_reload)
            };
            // Coalesce overdue edges without rebasing the PIT phase on the
            // host's polling/delivery latency. At most one IRQ is delivered.
            let period = divisor * 1_000_000_000 / 1_193_182;
            let periods = (now - self.timer_next_ns) / period + 1;
            self.timer_next_ns = self
                .timer_next_ns
                .saturating_add(periods.saturating_mul(period));
        }
        self.irq_deliveries += 1;
        Ok(true)
    }
    /// Rebase wall time before guest execution, not after a playback transition.
    pub fn start_audio_timeline(&mut self) {
        self.clock_started = std::time::Instant::now();
        if self.execution_clock.get().is_some() {
            self.execution_clock.set(Some(0));
        }
    }
    /// Share one real wall sample for at most 32 translated calls. Hosts must
    /// refresh at that bound, around slow services and before playback changes.
    pub fn begin_execution_batch(&self) -> u64 {
        let now = self.refresh_execution_clock();
        self.execution_clock.set(Some(now));
        self.execution_calls.set(0);
        now
    }
    pub fn end_execution_batch(&self) {
        self.execution_clock.set(None);
    }
    pub fn refresh_execution_clock(&self) -> u64 {
        let now = self.virtual_elapsed_ns.unwrap_or_else(|| {
            self.clock_started
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64
        });
        if self.execution_clock.get().is_some() {
            self.execution_clock.set(Some(now));
        }
        now
    }
    pub fn advance_sound_to(&mut self, ns: u64) -> Result<(), Fault> {
        let Some(sb) = &mut self.sound_blaster else {
            return Ok(());
        };
        // One authoritative physical bus: CPU A20 does not gate ISA DMA.
        // Reject all remaining-source errors BEFORE a muted loop can drain PCM.
        sb.preflight_with_xms(&self.memory, &self.xms)?;
        if self.retain_sfx_pcm {
            return sb.advance_to_with_xms(ns, &mut self.memory, &self.xms);
        }
        // Muted runs still consume DMA and produce IRQ. Drain bounded slices so
        // idle elapsed silence never overflows the independent source queue.
        let mut scratch = [[0; 2]; 1024];
        loop {
            let end = ns.min(sb.status().now_ns.saturating_add(20_000_000));
            sb.advance_to_with_xms(end, &mut self.memory, &self.xms)?;
            loop {
                let n = sb.drain_frames(&mut scratch);
                self.sfx_discarded_frames += n as u64;
                if n == 0 {
                    break;
                }
            }
            if end == ns {
                break;
            }
        }
        Ok(())
    }
    /// Prepare a realtime slice AFTER sink reservation, BEFORE either source
    /// advances/drains. FIFO indices reconcile hardware already rendered by IRQ
    /// polling/port I/O; never send a historic slice timestamp to that hardware.
    pub fn prepare_realtime_audio(
        &mut self,
        start_frame: u64,
        end_ns: u64,
        frames: usize,
    ) -> Result<(), Fault> {
        if self.cdrom.is_none() && self.sound_blaster.is_none() {
            return Err(Fault("mixed PCM has no source".into()));
        }
        if let Some(sb) = &self.sound_blaster {
            if !self.retain_sfx_pcm || sb.queued_frame_start() != start_frame {
                return Err(Fault("SB mixed PCM FIFO/output cursor mismatch".into()));
            }
            sb.preflight_with_xms(&self.memory, &self.xms)?;
        }
        // Mounted CD queues hold 8192 frames, exceeding a realtime slice. Read
        // failure must precede new SB time/DMA effects, not just its FIFO drain.
        if let Some(cd) = &mut self.cdrom
            && let Some(p) = cd.audio_player()
            && p.state() == crate::audio::PlaybackState::Playing
        {
            let required = frames.min((p.duration_frames() - p.position_frames()) as usize);
            cd.prefetch_audio()?;
            if cd.audio_player().unwrap().queued_frames() < required {
                return Err(Fault("playing CD has insufficient queued PCM".into()));
            }
        }
        if let Some(sb) = &mut self.sound_blaster {
            if end_ns > sb.status().now_ns {
                sb.advance_to_with_xms(end_ns, &mut self.memory, &self.xms)?;
            }
            if sb.status().queued_frames < frames {
                return Err(Fault(
                    "SB mixed timeline has insufficient queued frames".into(),
                ));
            }
        }
        Ok(())
    }
    /// Exact timeline slice; CD counters remain source consumption, while SFX
    /// counts drained 44100-Hz timeline frames (including speaker-muted silence).
    pub fn render_mixed_audio(&mut self, pcm: &mut [[i16; 2]]) -> Result<(usize, usize), Fault> {
        if self
            .sound_blaster
            .as_ref()
            .is_some_and(|s| s.status().queued_frames < pcm.len())
        {
            return Err(Fault(
                "SB mixed timeline has insufficient queued frames".into(),
            ));
        }
        let mut cd_frames = 0;
        let mut offset = 0;
        pcm.fill([0; 2]);
        while offset < pcm.len() {
            let mut count = pcm.len() - offset;
            if let Some(cd) = &mut self.cdrom {
                if cd
                    .audio_player()
                    .is_some_and(|p| p.state() == crate::audio::PlaybackState::Playing)
                {
                    cd.prefetch_audio()?;
                    let queued = cd.audio_player().unwrap().queued_frames();
                    if queued == 0 {
                        return Err(Fault("playing CD has no queued PCM".into()));
                    }
                    count = count.min(queued);
                }
                cd_frames += cd.render_audio(&mut pcm[offset..offset + count]);
            }
            offset += count;
        }
        let mut sfx_frames = 0;
        if let Some(sb) = &mut self.sound_blaster {
            if sb.status().queued_frames < pcm.len() {
                return Err(Fault(
                    "SB mixed timeline has insufficient queued frames".into(),
                ));
            }
            for chunk in pcm.chunks_mut(1024) {
                let mut effects = [[0; 2]; 1024];
                let n = sb.drain_frames(&mut effects[..chunk.len()]);
                sfx_frames += n;
                for (dst, src) in chunk.iter_mut().zip(effects) {
                    for c in 0..2 {
                        dst[c] = (i32::from(dst[c]) + i32::from(src[c]))
                            .clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                            as i16;
                    }
                }
            }
        }
        Ok((cd_frames, sfx_frames))
    }
    pub fn elapsed_ns(&self) -> u64 {
        self.virtual_elapsed_ns
            .or(self.execution_clock.get())
            .unwrap_or_else(|| self.refresh_execution_clock())
    }
    pub fn port_in(&mut self, port: u16, width: u8) -> Result<u16, Fault> {
        let now = self.refresh_execution_clock();
        self.advance_sound_to(now)?;
        if width == 8 && self.opl_absent && matches!(port, 0x388 | 0x389) {
            return Ok(0); // No timer bits: an FM presence probe must fail.
        }
        if width == 8
            && self
                .sound_blaster
                .as_ref()
                .is_some_and(|s| s.handles_port(port))
        {
            return self
                .sound_blaster
                .as_mut()
                .unwrap()
                .read_port(port)
                .map(u16::from);
        }
        if width == 8 && matches!(port, 0x3ce | 0x3cf | 0x3c4 | 0x3c5) {
            if self.video_mode != 0x12 {
                return Err(self.fault("planar register input requires mode12"));
            }
            return self
                .planar
                .borrow()
                .read_port(port)
                .map(u16::from)
                .map_err(Fault);
        }
        if width == 8 && port == 0x3d4 {
            return Ok(u16::from(self.crtc_index));
        }
        if width == 8 && port == 0x3d5 {
            return match self.crtc_index {
                0x0e => Ok(self.crtc_cursor >> 8),
                0x0f => Ok(self.crtc_cursor & 255),
                0x0a => Ok(self.cursor_shape >> 8),
                0x0b => Ok(self.cursor_shape & 255),
                _ => Err(self.fault("unsupported CRTC register read")),
            };
        }
        if port == 0x61 && width == 8 {
            return Ok(u16::from(self.keyboard_control));
        }
        if port == 0x3da && width == 8 {
            // Declared 70 Hz VGA mode-13h timing; 1 ms vertical blank. Wall time
            // (or explicitly injected test time), never a port-read toggle.
            let phase = now % 14_285_714;
            return Ok(if phase >= 13_285_714 { 9 } else { 1 });
        }

        if width == 8 && port == 0x60 {
            return self
                .scan_codes
                .pop_front()
                .map(u16::from)
                .ok_or_else(|| self.fault("keyboard data port read with no queued scan code"));
        }
        if width == 8 && port == 0x64 {
            return Ok(u16::from(!self.scan_codes.is_empty()));
        }
        // PIC mask registers for the declared fixed-priority IRQ model.
        if width == 8 && matches!(port, 0x21 | 0xa1) {
            return Ok(u16::from(self.pic_masks[usize::from(port == 0xa1)]));
        }
        if width == 8 && (0x3c6..=0x3c9).contains(&port) {
            return self
                .dac
                .read_port(port)
                .map(u16::from)
                .map_err(|e| self.fault(e));
        }
        Err(self.fault(format!("unsupported IN port={port:04x} width={width}")))
    }
    pub fn port_out(&mut self, port: u16, value: u16, width: u8) -> Result<(), Fault> {
        self.port_out_owned(port, value, width, None)
    }
    fn port_out_owned(
        &mut self,
        port: u16,
        value: u16,
        width: u8,
        owner: Option<crate::video::PaletteOwner>,
    ) -> Result<(), Fault> {
        let now = self.refresh_execution_clock();
        self.advance_sound_to(now)?;
        if width == 8 && self.opl_absent && matches!(port, 0x388 | 0x389) {
            return Ok(()); // Explicit absent-device probe, not FM synthesis.
        }
        if width == 8
            && self
                .sound_blaster
                .as_ref()
                .is_some_and(|s| s.handles_port(port))
        {
            return self.sound_blaster.as_mut().unwrap().write_port_with_xms(
                port,
                value as u8,
                &mut self.memory,
                &self.xms,
            );
        }
        if matches!(port, 0x3ce | 0x3cf | 0x3c4 | 0x3c5) {
            if self.video_mode != 0x12 {
                return Err(self.fault("planar register output requires mode12"));
            }
            return match width {
                8 => self
                    .planar
                    .borrow_mut()
                    .write_port(port, value as u8)
                    .map_err(Fault),
                16 => self
                    .planar
                    .borrow_mut()
                    .write_port16(port, value)
                    .map_err(Fault),
                _ => Err(self.fault("unsupported planar port width")),
            };
        }
        if width == 8 && port == 0x3d4 {
            self.crtc_index = value as u8;
            return Ok(());
        }
        if width == 8 && port == 0x3d5 {
            let v = u16::from(value as u8);
            match self.crtc_index {
                0x0e => self.crtc_cursor = (self.crtc_cursor & 255) | (v << 8),
                0x0f => self.crtc_cursor = (self.crtc_cursor & 0xff00) | v,
                0x0a => self.cursor_shape = (self.cursor_shape & 255) | (v << 8),
                0x0b => self.cursor_shape = (self.cursor_shape & 0xff00) | v,
                _ => return Err(self.fault("unsupported CRTC register write")),
            };
            return Ok(());
        }
        if port == 0x61 && width == 8 {
            self.keyboard_control = value as u8;
            return Ok(());
        }
        if width == 8 && port == 0x43 {
            if value as u8 != 0x36 {
                return Err(self.fault("unsupported PIT control"));
            }
            self.pit_control = value as u8;
            self.pit_low = None;
            return Ok(());
        }
        if width == 8 && port == 0x40 {
            if let Some(lo) = self.pit_low.take() {
                self.pit_reload = u16::from_le_bytes([lo, value as u8]);
                let divisor = if self.pit_reload == 0 {
                    65536u64
                } else {
                    u64::from(self.pit_reload)
                };
                self.timer_next_ns = self
                    .elapsed_ns()
                    .saturating_add(divisor * 1_000_000_000 / 1_193_182);
            } else {
                self.pit_low = Some(value as u8);
            }
            return Ok(());
        }
        if width == 8 && matches!(port, 0x20 | 0xa0) && value == 0x20 {
            let isr = &mut self.pic_isr[usize::from(port == 0xa0)];
            if *isr != 0 {
                *isr &= !(1u8 << isr.trailing_zeros());
            }
            return Ok(());
        } // Non-specific EOI clears only the highest-priority ISR of this PIC.

        if width == 8 && matches!(port, 0x21 | 0xa1) {
            self.pic_masks[usize::from(port == 0xa1)] = value as u8;
            return Ok(());
        }
        if width == 8 && (0x3c6..=0x3c9).contains(&port) {
            let result =
                if let Some(owner) = owner.filter(|_| self.video_mode == 0x13 && port == 0x3c9) {
                    self.dac
                        .write_palette_stream(owner, self.regs.cx, value as u8)
                } else {
                    self.dac.write_port(port, value as u8)
                };
            return result.map_err(|e| self.fault(e));
        }
        Err(self.fault(format!(
            "unsupported OUT port={port:04x} value={value:04x} width={width}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    fn machine() -> Machine {
        let mut bytes = vec![0; 96];
        for (i, w) in [0x5a4d_u16, 96, 1, 0, 2, 0, 0xffff, 0, 64, 0, 0, 0, 28, 0]
            .into_iter()
            .enumerate()
        {
            bytes[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
        }
        Machine::from_mz(&bytes, 0x1010).unwrap()
    }
    #[test]
    fn psp_environment_and_relocation_load() {
        let m = machine();
        assert_eq!(m.regs.ds, 0x1000);
        assert_eq!(m.regs.cs, 0x1010);
        assert_eq!(m.read16(SegmentedAddress::new(m.psp, 0)).unwrap(), 0x20cd);
        assert_eq!(m.read16(SegmentedAddress::new(m.psp, 2)).unwrap(), 0x9f00);
        assert!(m.memory.len() >= 0xa0000);
    }
    #[test]
    fn operand32_register_aliases_and_dword_memory_are_bounded() {
        let mut m = machine();
        m.set32(0, 0x12345678);
        assert_eq!(m.regs.ax, 0x5678);
        m.regs.set_ah(0xab);
        assert_eq!(m.get32(0), 0x1234ab78);
        m.regs.ax = 9;
        assert_eq!(m.get32(0), 0x12340009);
        for index in 0..8 {
            m.set32(index, 0xfedc1234 + u32::from(index));
            assert_eq!(m.get32(index), 0xfedc1234 + u32::from(index));
        }
        let at = SegmentedAddress::new(0x7000, 0xfffc);
        m.write32(at, 0xdeadbeef).unwrap();
        assert_eq!(m.read32(at).unwrap(), 0xdeadbeef);
        let before = m.memory.as_bytes().to_vec();
        assert!(m.write32(at.wrapping_add(1), 1).is_err());
        assert_eq!(m.memory.as_bytes(), before);
    }
    #[test]
    fn timer_and_retrace_are_time_based_and_interrupt_frame_is_real() {
        let mut m = machine();
        m.irq_enabled = true;
        m.virtual_elapsed_ns = Some(1);
        assert_eq!(m.port_in(0x3da, 8).unwrap() & 8, 0);
        m.virtual_elapsed_ns = Some(13_500_000);
        assert_eq!(m.port_in(0x3da, 8).unwrap() & 8, 8);
        m.port_out(0x43, 0x36, 8).unwrap();
        m.port_out(0x40, 0x00, 8).unwrap();
        m.port_out(0x40, 0x10, 8).unwrap();
        assert_eq!(m.pit_reload, 4096);
        m.write16(SegmentedAddress::new(0, 32), 0x100).unwrap();
        m.write16(SegmentedAddress::new(0, 34), 0x6000).unwrap();
        m.regs.flags.set(Flags::IF, true);
        m.virtual_elapsed_ns = Some(30_000_000);
        m.port_out(0x21, 0xff, 8).unwrap();
        assert!(!m.poll_timer_irq().unwrap());
        m.port_out(0x21, 0xfe, 8).unwrap();
        let before = m.regs;
        m.irq_shadow = 1;
        assert!(!m.poll_timer_irq().unwrap());
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!((m.regs.cs, m.regs.ip, m.irq_deliveries), (0x6000, 0x100, 1));
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        assert_eq!(m.regs, before);
        assert!(!m.poll_timer_irq().unwrap());
        m.virtual_elapsed_ns = Some(40_000_000);
        assert!(m.poll_timer_irq().unwrap());
    }
    #[test]
    fn keyboard_irq_guest_frame_mask_if_shadow_and_make_break() {
        let mut m = machine();
        m.enable_irqs().unwrap();
        m.virtual_elapsed_ns = Some(0);
        m.write16(SegmentedAddress::new(0, 36), 0x100).unwrap();
        m.write16(SegmentedAddress::new(0, 38), 0x6000).unwrap();
        m.key_event(0x1c, 13, true);
        m.key_event(0x1c, 13, false);
        m.regs.flags.set(Flags::IF, false);
        assert!(!m.poll_timer_irq().unwrap());
        m.regs.flags.set(Flags::IF, true);
        m.port_out(0x21, 0xff, 8).unwrap();
        assert!(!m.poll_timer_irq().unwrap());
        m.port_out(0x21, 0xfd, 8).unwrap();
        m.irq_shadow = 1;
        assert!(!m.poll_timer_irq().unwrap());
        let before = m.regs;
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!((m.regs.cs, m.regs.ip), (0x6000, 0x100));
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x1c);
        m.port_out(0x61, 0x80, 8).unwrap();
        assert_eq!(m.port_in(0x61, 8).unwrap(), 0x80);
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        assert_eq!(m.regs, before);
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x9c);
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        assert_eq!(m.regs, before);
        assert!(!m.poll_timer_irq().unwrap());
        assert_eq!(m.irq_deliveries, 2);
    }
    #[test]
    fn pic_fixed_priority_nested_service_and_pic_specific_eoi() {
        let mut m = machine();
        m.enable_irqs().unwrap();
        m.virtual_elapsed_ns = Some(0);
        for (irq, cs) in [(0, 0x6000), (1, 0x6100)] {
            m.write16(SegmentedAddress::new(0, (8 + irq) * 4), 0x100)
                .unwrap();
            m.write16(SegmentedAddress::new(0, (8 + irq) * 4 + 2), cs)
                .unwrap();
        }
        m.regs.flags.set(Flags::IF, true);
        m.key_event(0x1c, 13, true);
        m.key_event(0x1c, 13, false);
        m.virtual_elapsed_ns = Some(60_000_000);
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!(m.regs.cs, 0x6000);
        m.regs.flags.set(Flags::IF, true);
        m.virtual_elapsed_ns = Some(120_000_000);
        assert!(!m.poll_timer_irq().unwrap()); // no self/lower priority reentry
        m.port_out(0xa0, 0x20, 8).unwrap();
        assert!(!m.poll_timer_irq().unwrap()); // slave cannot EOI master
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        m.virtual_elapsed_ns = Some(60_000_000);
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!(m.regs.cs, 0x6100);
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x1c);
        m.port_out(0xa0, 0x20, 8).unwrap();
        m.regs.flags.set(Flags::IF, true);
        assert!(!m.poll_timer_irq().unwrap());
        let outer_sp = m.regs.sp;
        m.virtual_elapsed_ns = Some(120_000_000);
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!(m.regs.cs, 0x6000);
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        assert_eq!(m.regs.sp, outer_sp);
        assert_eq!(m.regs.cs, 0x6100);
        assert!(!m.poll_timer_irq().unwrap()); // timer EOI did not release IRQ1
        m.port_out(0x20, 0x20, 8).unwrap();
        m.interrupt_return().unwrap();
        assert!(m.poll_timer_irq().unwrap());
        assert_eq!(m.regs.cs, 0x6100);
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x9c);
    }
    #[test]
    fn bounded_input_overflow_retains_oldest_and_can_drop_release() {
        let mut m = machine();
        for _ in 0..256 {
            m.key_event(0x1c, 13, true);
        }
        m.key_event(0x1c, 13, false);
        assert!(!m.key_down[0x1c]);
        for _ in 0..256 {
            assert_eq!(m.port_in(0x60, 8).unwrap(), 0x1c);
        }
        assert!(m.port_in(0x60, 8).is_err());
    }
    #[test]
    fn changed_code_fails_closed() {
        let mut m = machine();
        m.guard_code(&[0, 0]).unwrap();
        m.write8(SegmentedAddress::new(m.regs.cs, m.regs.ip), 1)
            .unwrap();
        assert!(
            m.guard_code(&[0, 0])
                .unwrap_err()
                .0
                .contains("code changed")
        );
    }
    #[test]
    fn interrupt_handler_frame_iret_and_no_host_bypass() {
        let mut m = machine();
        m.regs.ss = 0x7000;
        m.regs.sp = 0xfffe;
        m.regs.cs = 0x1010;
        m.regs.ip = 0x20;
        m.regs.flags = Flags::from_bits(Flags::TF | Flags::IF | Flags::CF);
        m.write16(SegmentedAddress::new(0, 0x12 * 4), 0x100)
            .unwrap();
        m.write16(SegmentedAddress::new(0, 0x12 * 4 + 2), 0x6000)
            .unwrap();
        assert!(
            m.interrupt(0x12)
                .unwrap_err()
                .0
                .contains("bypass forbidden")
        );
        m.software_interrupt(0x12, 0x22).unwrap();
        assert_eq!((m.regs.cs, m.regs.ip, m.regs.sp), (0x6000, 0x100, 0xfff8));
        assert_eq!(
            m.read16(SegmentedAddress::new(0x7000, 0xfff8)).unwrap(),
            0x22
        );
        assert_eq!(
            m.read16(SegmentedAddress::new(0x7000, 0xfffa)).unwrap(),
            0x1010
        );
        assert_eq!(
            m.read16(SegmentedAddress::new(0x7000, 0xfffc)).unwrap(),
            0x303
        );
        assert_eq!(m.regs.flags.bits(), 3);
        m.interrupt_return().unwrap();
        assert_eq!(
            (m.regs.cs, m.regs.ip, m.regs.sp, m.regs.flags.bits()),
            (0x1010, 0x22, 0xfffe, 0x303)
        );
        assert_eq!(Machine::normalize_flags(0), 2);
        assert_eq!(Machine::normalize_flags(0xffff), 0x7fd7);
    }
    #[test]
    fn bios_keyboard_wait_poll_read_and_scan_queue() {
        let mut m = machine();
        m.regs.ax = 0;
        m.regs.ip = 0x20;
        m.software_interrupt(0x16, 0x22).unwrap();
        assert!(m.waiting_for_key);
        assert_eq!(m.regs.ip, 0x20);
        m.key_event(0x1c, 13, true);
        m.regs.ax = 0x100;
        m.interrupt(0x16).unwrap();
        assert!(!m.regs.flags.zf());
        assert_eq!(m.regs.ax, 0x1c0d);
        m.regs.ax = 0;
        m.software_interrupt(0x16, 0x22).unwrap();
        assert_eq!(m.regs.ip, 0x22);
        assert_eq!(m.regs.ax, 0x1c0d);
        assert_eq!(m.port_in(0x64, 8).unwrap(), 1);
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x1c);
        m.key_event(0x1c, 13, false);
        assert_eq!(m.port_in(0x60, 8).unwrap(), 0x9c);
        assert_eq!(m.port_in(0x64, 8).unwrap(), 0);
        assert!(m.port_in(0x60, 8).is_err());
        m.regs.ax = 0x100;
        m.interrupt(0x16).unwrap();
        assert!(m.regs.flags.zf());
    }
    #[test]
    fn declared_equipment_offline_network_and_pic_masks_are_explicit() {
        let mut m = machine();
        m.interrupt(0x11).unwrap();
        assert_eq!(m.regs.ax, 0x21);
        m.regs.ax = 0;
        m.interrupt(0x2a).unwrap();
        assert_eq!(m.regs.ah(), 0);
        m.regs.ax = 0x0100;
        assert!(m.interrupt(0x2a).is_err());
        m.port_out(0x21, 0x5a, 8).unwrap();
        assert_eq!(m.port_in(0x21, 8).unwrap(), 0x5a);
        assert_eq!(m.port_in(0xa1, 8).unwrap(), 0xff);
        assert!(m.port_out(0x21, 0x5a, 16).is_err());
    }
    #[test]
    fn video_state_reports_only_implemented_modes_and_mode_set_clears_vram() {
        let mut m = machine();
        m.regs.ax = 0x1b02;
        m.regs.bx = 0;
        m.regs.es = 0x7000;
        m.regs.di = 0;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.regs.al(), 0x1b);
        assert_eq!(m.read8(SegmentedAddress::new(0x7000, 4)).unwrap(), 3);
        assert_eq!(m.read16(SegmentedAddress::new(0x7000, 5)).unwrap(), 80);
        m.write8(SegmentedAddress::new(0xa000, 0), 7).unwrap();
        m.regs.ax = 0x13;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.read8(SegmentedAddress::new(0xa000, 0)).unwrap(), 0);
        m.regs.ax = 0x0f00;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.regs.ax, 0x2813);
        m.regs.ax = 0x1b00;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.read16(SegmentedAddress::new(0x7000, 0x27)).unwrap(), 256);
    }
    #[test]
    fn clock_break_and_current_font_vector_are_explicit_state() {
        let mut m = machine();
        m.clock_centiseconds = Some(12 * 360_000 + 34 * 6000 + 56 * 100 + 78);
        m.regs.ax = 0x2c00;
        m.interrupt(0x21).unwrap();
        assert_eq!((m.regs.cx, m.regs.dx), ((12 << 8) | 34, (56 << 8) | 78));
        m.regs.ax = 0x3301;
        m.regs.dx = 1;
        m.interrupt(0x21).unwrap();
        m.regs.ax = 0x3300;
        m.regs.dx = 0;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.dx, 1);
        m.write16(SegmentedAddress::new(0, 0x1f * 4), 0x100)
            .unwrap();
        m.write16(SegmentedAddress::new(0, 0x1f * 4 + 2), 0x900)
            .unwrap();
        m.regs.ax = 0x1130;
        m.regs.bx = 0;
        m.interrupt(0x10).unwrap();
        assert_eq!((m.regs.es, m.regs.bp, m.regs.cx), (0x900, 0x100, 16));
        m.regs.ax = 0x1130;
        m.regs.bx = 0x0600;
        assert!(m.interrupt(0x10).is_err());
    }
    #[test]
    fn bios_text_reads_vram_repeats_scrolls_and_rejects_invalid_transactions() {
        let mut m = machine();
        let cell = SegmentedAddress::new(0xb800, 0);
        assert_eq!(m.read16(cell).unwrap(), 0x0720);
        m.regs.ax = 0x0241;
        m.regs.bx = 0;
        m.regs.dx = 0x0102;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.read16(SegmentedAddress::new(0x40, 0x50)).unwrap(), 0x0102);
        m.regs.ax = 0x0941;
        m.regs.bx = 0x1e;
        m.regs.cx = 2;
        m.interrupt(0x10).unwrap();
        m.regs.ax = 0x0800;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.regs.ax, 0x1e41);
        assert_eq!(m.cursor, 0x0102);
        m.write16(SegmentedAddress::new(0xb800, 164), 0x2f5a)
            .unwrap();
        m.regs.ax = 0x0800;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.regs.ax, 0x2f5a);
        m.regs.ax = 0x0600;
        m.regs.bx = 0x0700;
        m.regs.cx = 0;
        m.regs.dx = 0x184f;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.read16(cell).unwrap(), 0x0720);
        m.regs.ax = 0x0e42;
        m.regs.bx = 0;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.cursor, 0x0103);
        let before = m.memory.clone();
        let cursor = m.cursor;
        m.regs.ax = 0x0200;
        m.regs.dx = 0x1950;
        assert!(m.interrupt(0x10).is_err());
        assert_eq!(m.memory, before);
        assert_eq!(m.cursor, cursor);
        m.regs.ax = 0x0083;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.cursor, 0);
        assert_eq!(
            m.read16(SegmentedAddress::new(0xb800, 164)).unwrap(),
            0x0742
        );
    }
    #[test]
    fn text_crtc_cursor_registers_and_bda_base_are_not_fake_success() {
        let mut m = machine();
        assert_eq!(m.read16(SegmentedAddress::new(0x40, 0x63)).unwrap(), 0x3d4);
        m.port_out(0x3d4, 14, 8).unwrap();
        m.port_out(0x3d5, 7, 8).unwrap();
        assert_eq!(m.port_in(0x3d5, 8).unwrap(), 7);
        m.port_out(0x3d4, 15, 8).unwrap();
        m.port_out(0x3d5, 0xcf, 8).unwrap();
        assert_eq!(m.crtc_cursor, 1999);
        assert_eq!(m.port_in(0x3d5, 8).unwrap(), 0xcf);
        m.port_out(0x3d4, 0, 8).unwrap();
        let before = m.crtc_cursor;
        assert!(m.port_out(0x3d5, 0x10, 8).is_err());
        assert_eq!(before, m.crtc_cursor);
        assert!(m.port_in(0x3d5, 8).is_err());
    }
    #[test]
    fn bios_mode12_routes_cpu_and_bulk_to_planes_and_fails_mixed_ranges() {
        let mut m = machine();
        m.regs.ax = 0x12;
        m.interrupt(0x10).unwrap();
        let at = SegmentedAddress::new(0xa000, 0);
        m.port_out(0x3c4, 0x0102, 16).unwrap();
        m.write8(at, 0x80).unwrap();
        assert_eq!(m.planar.borrow().plane(0).unwrap()[0], 0x80);
        assert_eq!(m.memory.as_bytes()[0xa0000], 0);
        assert_eq!(m.read8(at).unwrap(), 0x80);
        assert_eq!(m.planar.borrow().latches(), [0x80, 0, 0, 0]);
        m.write_bytes(at.wrapping_add(2), &[0x10, 0x20]).unwrap();
        let mut bytes = [0; 2];
        m.read_bytes(at.wrapping_add(2), &mut bytes).unwrap();
        assert_eq!(bytes, [0x10, 0x20]);
        m.regs.es = 0x800;
        m.regs.dx = 0x200;
        m.memory
            .write_bytes(SegmentedAddress::new(0x800, 0x200), &[63; 17])
            .unwrap();
        m.regs.ax = 0x1002;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.frame().unwrap().2[0], 0xffffff);
        assert_eq!(m.overscan, 63);
        m.memory
            .write8(SegmentedAddress::new(0x800, 0x200), 64)
            .unwrap();
        let palette = m.attribute_palette;
        assert!(m.interrupt(0x10).is_err());
        assert_eq!(m.attribute_palette, palette);
        let before = m.planar.borrow().clone();
        assert!(
            m.write16(SegmentedAddress::new(0xafff, 15), 0xbeef)
                .is_err()
        );
        assert_eq!(*m.planar.borrow(), before);
        let (framew, frameh, pixels) = m.frame().unwrap();
        assert_eq!((framew, frameh, pixels.len()), (640, 480, 307200));
        assert_eq!(pixels[0], 0xffffff);
        m.regs.ax = 0x92;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.planar.borrow().plane(0).unwrap()[0], 0x80);
        m.regs.ax = 0x12;
        m.interrupt(0x10).unwrap();
        assert_eq!(m.read8(at).unwrap(), 0);
        m.regs.ax = 0x13;
        m.interrupt(0x10).unwrap();
        m.write8(at, 7).unwrap();
        assert_eq!(m.memory.as_bytes()[0xa0000], 7);
        assert_eq!(m.planar.borrow().plane(0).unwrap()[0], 0);
    }
    #[test]
    fn mode12_ram_only_code_stack_and_service_failures_leave_latches_unchanged() {
        let mut m = machine();
        m.regs.ax = 0x12;
        m.interrupt(0x10).unwrap();
        m.write8(SegmentedAddress::new(0xa000, 0), 0x80).unwrap();
        m.read8(SegmentedAddress::new(0xa000, 0)).unwrap();
        let before = m.planar.borrow().clone();
        m.regs.cs = 0x9fff;
        m.regs.ip = 0x10;
        assert!(m.guard_code(&[0]).is_err());
        assert_eq!(*m.planar.borrow(), before);
        m.regs.ss = 0x9fff;
        m.regs.sp = 0x10;
        let regs = m.regs;
        assert!(m.pop16().is_err());
        assert_eq!(m.regs, regs);
        m.regs.bp = 0x10;
        let regs = m.regs;
        assert!(m.leave16().is_err());
        assert_eq!(m.regs, regs);
        assert!(m.far_return(0).is_err());
        assert_eq!(m.regs, regs);
        assert!(m.interrupt_return().is_err());
        assert_eq!(m.regs, regs);
        m.write16(SegmentedAddress::new(0, 0x12 * 4), 0x100)
            .unwrap();
        m.write16(SegmentedAddress::new(0, 0x12 * 4 + 2), 0x6000)
            .unwrap();
        m.regs.sp = 0x14;
        let regs = m.regs;
        assert!(m.software_interrupt(0x12, 0x20).is_err());
        assert_eq!(m.regs, regs);
        assert_eq!(*m.planar.borrow(), before);
        m.regs.ax = 0x1b00;
        m.regs.bx = 0;
        m.regs.es = 0x9fff;
        m.regs.di = 0x10;
        assert!(m.interrupt(0x10).is_err());
        assert_eq!(*m.planar.borrow(), before);
        m.regs.ax = 0x4000;
        m.regs.bx = 1;
        m.regs.ds = 0xa000;
        m.regs.dx = 0;
        m.regs.cx = 1;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.console, [0x80]);
        m.regs.ax = 0x13;
        m.interrupt(0x10).unwrap();
        m.regs.ss = 0xa000;
        m.regs.sp = 0;
        m.memory
            .write16(SegmentedAddress::new(0xa000, 0), 0xbeef)
            .unwrap();
        assert_eq!(m.pop16().unwrap(), 0xbeef);
    }
    #[test]
    fn dos_flush_only_discards_typeahead_not_device_release() {
        let mut m = machine();
        m.key_event(0x50, 0, true);
        m.key_event(0x50, 0, false);
        m.regs.ax = 0x0c00;
        let before = m.regs;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs, before);
        assert!(m.bios_keys.is_empty());
        assert_eq!(
            m.scan_codes.iter().copied().collect::<Vec<_>>(),
            [0x50, 0xd0]
        );
        assert!(!m.key_down[0x50]);
        m.key_event(0x1c, 13, true);
        m.regs.ax = 0x0c0a;
        let queue = m.bios_keys.clone();
        assert!(m.interrupt(0x21).is_err());
        assert_eq!(m.bios_keys, queue);
        m.regs.ax = 0x0c06;
        m.regs.dx = 0xff;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.al(), 0);
        assert!(m.regs.flags.contains(Flags::ZF));
        assert!(m.bios_keys.is_empty());
        m.key_event(0x1c, 13, false);
        m.regs.ax = 0x0cff;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.ax, 0x0cff);
    }
    #[test]
    fn unsupported_services_are_not_success() {
        let mut m = machine();
        m.regs.ax = 0xffff;
        assert!(m.interrupt(0x21).is_err());
        assert!(m.interrupt(0x37).is_err());
        assert!(m.port_in(0x1234, 8).is_err());
    }
    #[test]
    fn dos_memory_resize_allocate_free() {
        let mut m = machine();
        m.regs.ax = 0x4a00;
        m.regs.bx = 0x100;
        m.interrupt(0x21).unwrap();
        assert!(!m.regs.flags.cf());
        m.regs.ax = 0x4800;
        m.regs.bx = 0x200;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.ax, 0x1100);
        m.regs.es = m.regs.ax;
        m.regs.ax = 0x4900;
        m.interrupt(0x21).unwrap();
        assert!(!m.regs.flags.cf());
        m.regs.ax = 0x4800;
        m.regs.bx = 0xffff;
        m.interrupt(0x21).unwrap();
        assert!(m.regs.flags.cf());
        assert_eq!(m.regs.ax, 8);
    }
    #[test]
    fn vectors_and_exit() {
        let mut m = machine();
        m.regs.ax = 0x2535;
        m.regs.ds = 0x1234;
        m.regs.dx = 0x5678;
        m.interrupt(0x21).unwrap();
        m.regs.ax = 0x3535;
        m.interrupt(0x21).unwrap();
        assert_eq!((m.regs.es, m.regs.bx), (0x1234, 0x5678));
        m.regs.ax = 0x4c17;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.exit_code, Some(0x17));
    }
    #[test]
    fn version_queries_console_returns_and_resize_failure_effects() {
        let mut m = machine();
        for (query, bx) in [(0, 0xff00), (1, 0)] {
            m.regs.ax = 0x3000 | query;
            m.interrupt(0x21).unwrap();
            assert_eq!((m.regs.ax, m.regs.bx, m.regs.cx), (5, bx, 0));
        }
        m.regs.ax = 0x0200;
        m.regs.dx = u16::from(b'A');
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.al(), b'A');
        assert_eq!(m.console, b"A");
        m.regs.ds = 0x1200;
        m.regs.dx = 0;
        m.memory
            .write_bytes(SegmentedAddress::new(m.regs.ds, 0), b"BC$")
            .unwrap();
        m.regs.ax = 0x0900;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.al(), b'$');
        assert_eq!(m.console, b"ABC");
        m.regs.es = m.psp;
        m.regs.ax = 0x4a00;
        m.regs.bx = 0x100;
        m.interrupt(0x21).unwrap();
        m.regs.ax = 0x4a00;
        m.regs.bx = 0xffff;
        m.interrupt(0x21).unwrap();
        assert!(m.regs.flags.cf());
        assert_eq!(m.blocks[&m.psp], m.conventional_top - m.psp);
    }
    #[test]
    fn readonly_file_read_seek_close_and_output_bounds() {
        let mut m = machine();
        let scratch = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .expect("TMPDIR for test fixture");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = scratch.join(format!(
            "pal-runtime-assets-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("WORD.DAT"))
            .unwrap()
            .write_all(b"abcdef")
            .unwrap();
        m.mount_assets(&dir).unwrap();
        m.regs.ds = 0x1200;
        m.regs.dx = 0;
        m.memory
            .write_bytes(SegmentedAddress::new(m.regs.ds, 0), b"word.dat\0")
            .unwrap();
        m.regs.ax = 0x3d00;
        m.interrupt(0x21).unwrap();
        assert!(!m.regs.flags.cf());
        let handle = m.regs.ax;
        m.regs.ax = 0x3f00;
        m.regs.bx = handle;
        m.regs.cx = 4;
        m.regs.dx = 0x100;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.ax, 4);
        assert_eq!(
            m.read16(SegmentedAddress::new(0x1200, 0x100)).unwrap(),
            0x6261
        );
        m.regs.ax = 0x3f00;
        m.regs.cx = 2;
        m.regs.dx = 0xffff;
        assert!(m.interrupt(0x21).is_err());
        assert_eq!(m.files.seek(handle, 0, 1).unwrap(), 4);
        m.regs.ax = 0x4202;
        m.regs.cx = 0xffff;
        m.regs.dx = 0xfffe;
        m.interrupt(0x21).unwrap();
        assert_eq!((m.regs.dx, m.regs.ax), (0, 4));
        m.regs.ax = 0x3f00;
        m.regs.cx = 5;
        m.regs.dx = 0x100;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs.ax, 2);
        m.regs.ax = 0x3e00;
        m.interrupt(0x21).unwrap();
        assert!(!m.regs.flags.cf());
        m.regs.ax = 0x3f00;
        m.interrupt(0x21).unwrap();
        assert!(m.regs.flags.cf());
        assert_eq!(m.regs.ax, 6);
        m.regs.ax = 0x3d01;
        m.regs.dx = 0;
        m.interrupt(0x21).unwrap();
        assert!(m.regs.flags.cf());
        assert_eq!(fs::read(dir.join("WORD.DAT")).unwrap(), b"abcdef");
    }
    #[test]
    fn paths_reject_traversal_and_absolute_names() {
        let mut m = machine();
        for name in [
            b"../secret".as_slice(),
            b"/secret",
            b"C:\\secret",
            b"..",
            b"a/b",
        ] {
            assert!(m.files.open(name, 0, false).is_err());
        }
    }
}
