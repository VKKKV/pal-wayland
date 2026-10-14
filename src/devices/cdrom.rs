// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only MSCDEX data and CD-DA service. PCM is consumed explicitly, never
//! sent to a host audio device and never advanced by guest status polling.
use crate::{
    audio::{CdAudioImage, CdPlayer, FRAMES_PER_SECTOR, PlaybackState, StereoFrame},
    runtime::Fault,
    semantics::{GuestMemory, SegmentedAddress},
};
use std::{fs, ops::Range, path::Path};

/// Validate addresses only: do not read a RAM shadow or device read latches.
fn ram_span(
    m: &GuestMemory,
    at: SegmentedAddress,
    len: usize,
    forbidden: &Range<u32>,
) -> Result<(), Fault> {
    if len > 0x10000 - usize::from(at.offset) {
        return Err(Fault(format!(
            "CD-ROM {len}-byte span at {at} crosses segment boundary"
        )));
    }
    for i in 0..len {
        let physical = m.physical_address(at.wrapping_add(i as u16));
        if forbidden.contains(&(physical as u32)) {
            return Err(Fault(format!(
                "CD-ROM service span touches forbidden physical address {physical:#x}"
            )));
        }
        if physical >= m.len() {
            return Err(Fault(format!(
                "CD-ROM service span outside guest RAM at {physical:#x}"
            )));
        }
    }
    Ok(())
}

fn header_size(command: u8) -> u8 {
    match command {
        3 => 20,
        0x80 => 25,
        0x84 => 22,
        _ => 13,
    }
}

fn ioctl_size(function: u8) -> Result<usize, Fault> {
    match function {
        6 | 8 => Ok(5),
        7 => Ok(4),
        9 => Ok(2),
        10 | 11 => Ok(7),
        12 | 15 => Ok(11),
        _ => Err(Fault(format!("unsupported CD-ROM ioctl {function:02x}"))),
    }
}
#[derive(Debug)]
pub struct CdRom {
    iso: Vec<u8>,
    tracks: Vec<(u32, bool)>,
    leadout: u32,
    // Virtual head starts at data track 1 and survives final STOP.
    head_sector: u32,
    audio: Option<CdAudioService>,
    legacy_header13: bool,
}
#[derive(Debug)]
struct CdAudioService {
    player: CdPlayer,
    start: u32,
    end: u32,
}
impl CdRom {
    pub fn mount(path: &Path) -> Result<Self, Fault> {
        let iso = fs::read(path).map_err(|e| Fault(e.to_string()))?;
        if iso.len() % 2048 != 0
            || iso.len() < 17 * 2048
            || &iso[16 * 2048 + 1..16 * 2048 + 6] != b"CD001"
        {
            return Err(Fault("expected ISO9660 cooked data track".into()));
        }
        let leadout = (iso.len() / 2048) as u32;
        Ok(Self {
            iso,
            tracks: vec![(0, false)],
            leadout,
            head_sector: 0,
            audio: None,
            legacy_header13: false,
        })
    }
    pub fn mount_bin_cue(bin: &Path, cue: &Path) -> Result<Self, Fault> {
        let raw = fs::read(bin).map_err(|e| Fault(e.to_string()))?;
        if raw.len() % 2352 != 0 {
            return Err(Fault("raw CD length is not sector aligned".into()));
        }
        let text = fs::read_to_string(cue).map_err(|e| Fault(e.to_string()))?;
        let mut tracks = Vec::new();
        let mut audio = false;
        for line in text.lines() {
            let words: Vec<_> = line.split_whitespace().collect();
            if words.first() == Some(&"TRACK") {
                audio = words.get(2) == Some(&"AUDIO");
            }
            if words.len() == 3 && words[0] == "INDEX" && words[1] == "01" {
                let parts: Vec<u32> = words[2]
                    .split(':')
                    .map(|s| s.parse::<u32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| Fault("invalid CUE time".into()))?;
                if parts.len() != 3 || parts[1] >= 60 || parts[2] >= 75 {
                    return Err(Fault("invalid CUE MSF".into()));
                }
                let sector = parts[0]
                    .checked_mul(4500)
                    .and_then(|n| n.checked_add(parts[1] * 75))
                    .and_then(|n| n.checked_add(parts[2]))
                    .ok_or_else(|| Fault("CUE time overflow".into()))?;
                if sector as usize >= raw.len() / 2352
                    || tracks.last().is_some_and(|&(last, _)| sector <= last)
                {
                    return Err(Fault("invalid CUE track range".into()));
                }
                tracks.push((sector, audio));
            }
        }
        if tracks.is_empty() || tracks[0] != (0, false) {
            return Err(Fault("expected data track 1".into()));
        }
        let data_end = tracks
            .get(1)
            .map(|t| t.0 as usize)
            .unwrap_or(raw.len() / 2352);
        let mut iso = Vec::with_capacity(data_end * 2048);
        for sector in raw[..data_end * 2352].chunks_exact(2352) {
            // This fixed image ends track 1 with 150 all-zero pregap sectors.
            // Preserve their zero cooked payload; reject any other non-MODE1 data.
            if sector[15] != 1 && sector.iter().any(|&b| b != 0) {
                return Err(Fault("unsupported raw data sector mode".into()));
            }
            iso.extend_from_slice(&sector[16..2064]);
        }
        if iso.len() < 17 * 2048 || &iso[16 * 2048 + 1..16 * 2048 + 6] != b"CD001" {
            return Err(Fault("missing ISO9660 volume".into()));
        }
        // Validate FILE identity, sequential track numbers and encodings too;
        // the legacy TOC parser alone is not sufficient to advertise CD-DA.
        let image = CdAudioImage::mount_bin_cue(bin, cue).map_err(|e| Fault(e.to_string()))?;
        let audio = if image.tracks().iter().any(|track| track.audio) {
            Some(CdAudioService {
                player: CdPlayer::new(image, 8192).map_err(|e| Fault(e.to_string()))?,
                start: 0,
                end: 0,
            })
        } else {
            None
        };
        Ok(Self {
            iso,
            tracks,
            leadout: (raw.len() / 2352) as u32,
            head_sector: 0,
            audio,
            legacy_header13: false,
        })
    }
    /// Opt-in PAL quirk: its header length is the fixed common 13-byte prefix,
    /// not the full command structure. Actual memory reads remain checked.
    pub fn enable_legacy_header13(&mut self) {
        self.legacy_header13 = true;
    }
    pub fn sectors(&self) -> u32 {
        (self.iso.len() / 2048) as u32
    }
    pub fn audio_player(&self) -> Option<&CdPlayer> {
        self.audio.as_ref().map(|audio| &audio.player)
    }
    /// Explicit bounded file-I/O producer; never advances playback position.
    pub fn prefetch_audio(&mut self) -> Result<usize, Fault> {
        match self.audio.as_mut() {
            Some(audio) => audio.player.prefetch().map_err(|e| Fault(e.to_string())),
            None => Ok(0),
        }
    }
    /// Pure 44100 Hz stereo i16 PCM consumer. No I/O, allocation or devices.
    /// Silence on pause/underrun; returns only actual source frames consumed.
    pub fn render_audio(&mut self, output: &mut [StereoFrame]) -> usize {
        match self.audio.as_mut() {
            Some(audio) => {
                let consumed = audio.player.render_into(output);
                if consumed != 0 {
                    self.head_sector = (audio.player.absolute_frame() / FRAMES_PER_SECTOR) as u32;
                }
                consumed
            }
            None => {
                output.fill([0, 0]);
                0
            }
        }
    }
    fn playing(&self) -> bool {
        self.audio_player()
            .is_some_and(|p| p.state() == PlaybackState::Playing)
    }
    fn finish_request(
        &self,
        m: &mut GuestMemory,
        at: SegmentedAddress,
        status: u16,
    ) -> Result<(), Fault> {
        m.write8(at.wrapping_add(1), 0)?;
        m.write16(
            at.wrapping_add(3),
            status | if self.playing() { 0x200 } else { 0 },
        )?;
        Ok(())
    }
    fn sector(mode: u8, raw: [u8; 4]) -> Option<u32> {
        match mode {
            0 => Some(u32::from_le_bytes(raw)),
            1 if raw[0] < 75 && raw[1] < 60 && raw[3] == 0 => {
                (u32::from(raw[2]) * 4500 + u32::from(raw[1]) * 75 + u32::from(raw[0]))
                    .checked_sub(150)
            }
            _ => None,
        }
    }
    fn msf(sector: u32) -> [u8; 4] {
        let f = sector + 150;
        [(f % 75) as u8, ((f / 75) % 60) as u8, (f / 4500) as u8, 0]
    }
    /// MSCDEX QInfo (RBIL 01449): binary M/S/F times, unlike the F/S/M/0
    /// DWORD used by PLAY/TOC. TNO remains BCD; CONTROL/ADR has ADR=1.
    /// The supported CUE subset has INDEX 01 only and no track flags.
    /// MSCDEX requires valid stationary-head information too: mount starts
    /// at HSG 0 (data track 1), not at an invented unplayed audio position.
    fn qchannel(&self) -> [u8; 10] {
        let sector = self.head_sector;
        let (track, start, audio) = if sector >= self.leadout {
            // EOF at lead-out keeps its real location with Red Book TNO=AA.
            (0xaa, self.leadout, self.tracks.last().unwrap().1)
        } else {
            let i = self.tracks.partition_point(|&(start, _)| start <= sector) - 1;
            let (start, audio) = self.tracks[i];
            let number = (i + 1) as u8;
            (((number / 10) << 4) | (number % 10), start, audio)
        };
        let relative = sector - start; // no 150-sector bias within a track
        let absolute = Self::msf(sector);
        [
            if audio { 0x01 } else { 0x41 },
            track,
            1,
            (relative / 4500) as u8,
            ((relative / 75) % 60) as u8,
            (relative % 75) as u8,
            0,
            absolute[2],
            absolute[1],
            absolute[0],
        ]
    }
    /// Read the common prefix only after proving all five bytes, including
    /// unit/status stores. A declared short header needs no command fields.
    fn request_header(
        &self,
        m: &GuestMemory,
        at: SegmentedAddress,
        forbidden: &Range<u32>,
    ) -> Result<(u8, Option<[u8; 25]>), Fault> {
        ram_span(m, at, 5, forbidden)?;
        let mut header = [0; 25];
        m.read_bytes(at, &mut header[..5])?;
        let command = header[2];
        let length = header[0];
        let required = header_size(command);
        if length != 0 && length < required && !(self.legacy_header13 && length == 13) {
            return Ok((command, None));
        }
        ram_span(m, at, usize::from(required), forbidden)?;
        m.read_bytes(at, &mut header[..usize::from(required)])?;
        Ok((command, Some(header)))
    }

    /// Preflight a RAM-only service request before calling [`Self::request`].
    /// `forbidden` is a physical half-open range (mode12: 0xa0000..0xb0000).
    /// This checks actual schema spans, not the caller's declared capacity;
    /// legacy zero/13-byte lengths retain their normal protocol behavior.
    /// No reads occur until the containing span is proven ordinary RAM. No
    /// guest bytes, CD state, planes or latches are changed on rejection.
    pub fn preflight_ram_request(
        &self,
        m: &GuestMemory,
        at: SegmentedAddress,
        forbidden: Range<u32>,
    ) -> Result<(), Fault> {
        let (command, header) = self.request_header(m, at, &forbidden)?;
        let Some(header) = header else {
            return Ok(()); // request only writes protocol-error status
        };
        let word = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
        match command {
            3 => {
                let dst = SegmentedAddress::new(word(16), word(14));
                let count = usize::from(word(18));
                // The function itself is guest-controlled: never consult an
                // aperture byte to discover the required IOCTL buffer length.
                ram_span(m, dst, 1, &forbidden)?;
                let function = m.read8(dst)?;
                let required = ioctl_size(function)?;
                if count != 0 && count < required {
                    return Ok(());
                }
                if matches!(function, 7 | 11) {
                    ram_span(m, dst, 2, &forbidden)?;
                    let argument = m.read8(dst.wrapping_add(1))?;
                    if (function == 7 && argument != 0)
                        || (function == 11
                            && (argument == 0 || usize::from(argument) > self.tracks.len()))
                    {
                        return Ok(()); // invalid argument: no transfer write
                    }
                }
                ram_span(m, dst, required, &forbidden)?;
            }
            0x80 => {
                let raw = header[20..24].try_into().unwrap();
                if header[24] == 0
                    && let Some(start) = Self::sector(header[13], raw)
                {
                    let count = usize::from(word(18));
                    let len = count
                        .checked_mul(2048)
                        .ok_or_else(|| Fault("CD read overflow".into()))?;
                    let begin = (start as usize)
                        .checked_mul(2048)
                        .ok_or_else(|| Fault("CD read overflow".into()))?;
                    let end = begin
                        .checked_add(len)
                        .ok_or_else(|| Fault("CD read overflow".into()))?;
                    if end <= self.iso.len() {
                        ram_span(
                            m,
                            SegmentedAddress::new(word(16), word(14)),
                            len,
                            &forbidden,
                        )?;
                    }
                }
            }
            _ => {} // audio controls and unknown commands have no nested spans
        }
        Ok(())
    }

    /// MSCDEX request header layout RBIL 02597 / IOCTL 01449.
    pub fn request(&mut self, m: &mut GuestMemory, at: SegmentedAddress) -> Result<(), Fault> {
        // All modes prove the fixed prefix before reads or status writes.
        // Mode12 additionally preflights nested spans against the VGA aperture.
        let (command, header) = self.request_header(m, at, &(0..0))?;
        let Some(header) = header else {
            return self.finish_request(m, at, 0x8105);
        };
        let word = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
        let mut status = 0x100_u16;
        match command {
            3 => {
                let dst = SegmentedAddress::new(word(16), word(14));
                let count = usize::from(word(18));
                let function = m.read8(dst)?;
                let required = ioctl_size(function)?;
                if count != 0 && count < required {
                    return self.finish_request(m, at, 0x8105);
                }
                let mut out = vec![function];
                match function {
                    // RBIL 01450: only advertise implemented features.
                    // Producer prefetch is NOT MSCDEX command 82h capability.
                    6 => out.extend(
                        (0x0200_u32
                            | if self.audio.is_some() { 0x10 } else { 0 }
                            | if self.playing() { 0x400 } else { 0 })
                        .to_le_bytes(),
                    ),
                    7 => {
                        out.push(m.read8(dst.wrapping_add(1))?);
                        if out[1] == 0 {
                            out.extend(2048u16.to_le_bytes());
                        } else {
                            status = 0x8103;
                        }
                    }
                    8 => out.extend(self.sectors().to_le_bytes()),
                    9 => out.push(1),
                    10 => {
                        out.extend([1, self.tracks.len() as u8]);
                        out.extend(Self::msf(self.leadout));
                    }
                    11 => {
                        let track = m.read8(dst.wrapping_add(1))?;
                        out.push(track);
                        if let Some(&(sector, audio)) = track
                            .checked_sub(1)
                            .and_then(|i| self.tracks.get(i as usize))
                        {
                            out.extend(Self::msf(sector));
                            out.push(if audio { 0 } else { 0x40 });
                        } else {
                            status = 0x8108;
                        }
                    }
                    12 => out.extend(self.qchannel()),
                    15 => {
                        if let Some(audio) = self.audio.as_ref().filter(|a| {
                            matches!(
                                a.player.state(),
                                PlaybackState::Playing | PlaybackState::Paused
                            )
                        }) {
                            out.extend(
                                u16::from(audio.player.state() == PlaybackState::Paused)
                                    .to_le_bytes(),
                            );
                            // RBIL 01449 leaves +7 ambiguous. The MSCDEX spec
                            // names an END location, not a length. Use canonical
                            // Red Book F/S/M/0 (02597); DOSBox Staging IOCTL 0F
                            // writes M/S/F/0 instead. Do not copy that quirk.
                            out.extend(Self::msf(audio.start));
                            out.extend(Self::msf(audio.end));
                        } else {
                            out.extend([0; 10]);
                        }
                    }
                    _ => return Err(Fault(format!("unsupported CD-ROM ioctl {function:02x}"))),
                }
                if status == 0x100 {
                    // PAL's actual driver request leaves the byte count zero;
                    // use the fixed IOCTL schema in that legacy case. Nonzero
                    // undersized buffers are still rejected, guest writes checked.
                    if count != 0 && count < out.len() {
                        status = 0x8105;
                    } else {
                        m.write_bytes(dst, &out)?;
                        m.write16(at.wrapping_add(18), out.len() as u16)?;
                    }
                }
            }
            0x80 => {
                let dst = SegmentedAddress::new(word(16), word(14));
                let count = usize::from(word(18));
                let start = Self::sector(header[13], header[20..24].try_into().unwrap());
                if header[24] != 0 {
                    status = 0x8103;
                } else if let Some(start) = start {
                    let begin = start as usize * 2048;
                    let end = begin
                        .checked_add(count * 2048)
                        .ok_or_else(|| Fault("CD read overflow".into()))?;
                    if end > self.iso.len() {
                        status = 0x8108;
                    } else {
                        m.write_bytes(dst, &self.iso[begin..end])?;
                        if count != 0 {
                            self.head_sector = start + count as u32 - 1;
                        }
                    }
                } else {
                    status = 0x8108;
                }
            }
            0x84 => {
                // RBIL 02597: mode +0D, start DWORD +0E, count DWORD +12.
                if let Some(audio) = self.audio.as_mut() {
                    let start = header[14..18].try_into().unwrap();
                    let count = u32::from_le_bytes(header[18..22].try_into().unwrap());
                    if let Some(start) = Self::sector(header[13], start) {
                        if audio
                            .player
                            .play_sectors(u64::from(start), u64::from(count))
                            .is_ok()
                        {
                            audio.start = start;
                            audio.end = start + count; // validated within image
                            self.head_sector = start;
                        } else {
                            status = 0x8108;
                        }
                    } else {
                        status = 0x8108;
                    }
                } else {
                    status = 0x8108;
                }
            }
            0x85 => {
                if let Some(audio) = self.audio.as_mut() {
                    if audio.player.state() == PlaybackState::Playing {
                        audio.start = (audio.player.absolute_frame() / FRAMES_PER_SECTOR) as u32;
                        audio.player.pause();
                    } else {
                        // Second STOP clears resume state: MSCDEX state machine
                        // and DOSBox StopAudio both distinguish play from pause.
                        audio.player.stop();
                        audio.start = 0;
                        audio.end = 0;
                    }
                }
            }
            0x88 => {
                if let Some(audio) = self
                    .audio
                    .as_mut()
                    .filter(|a| a.player.state() == PlaybackState::Paused)
                {
                    audio.player.resume();
                } else {
                    status = 0x8108;
                }
            }
            _ => return Err(Fault(format!("unsupported CD-ROM request {command:02x}"))),
        }
        self.finish_request(m, at, status)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::A20;
    #[test]
    fn short_ioctl_does_not_read_outside_buffer_and_cue_overflow_is_rejected() {
        let mut cd = CdRom {
            iso: vec![0; 18 * 2048],
            tracks: vec![(0, false)],
            leadout: 18,
            head_sector: 0,
            audio: None,
            legacy_header13: false,
        };
        let mut m = GuestMemory::new(0x10000, A20::Enabled);
        let at = SegmentedAddress::new(0, 0x100);
        m.write8(at.wrapping_add(2), 3).unwrap();
        m.write16(at.wrapping_add(14), 0xffff).unwrap();
        m.write16(at.wrapping_add(18), 1).unwrap();
        m.write8(SegmentedAddress::new(0, 0xffff), 11).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x8105);
        let dir = std::env::temp_dir().join(format!("pal-cue-overflow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.bin"), vec![0; 30 * 2352]).unwrap();
        std::fs::write(
            dir.join("x.cue"),
            "TRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 954437:10:64\n",
        )
        .unwrap();
        let error = CdRom::mount_bin_cue(&dir.join("x.bin"), &dir.join("x.cue")).unwrap_err();
        assert!(error.0.contains("overflow"));
    }
    #[test]
    fn actual_multi_track_schema_does_not_invent_audio_playback() {
        let mut cd = CdRom {
            iso: vec![0; 18 * 2048],
            tracks: vec![(0, false), (18, true)],
            leadout: 30,
            head_sector: 0,
            audio: None,
            legacy_header13: false,
        };
        let mut m = GuestMemory::new(0x10000, A20::Enabled);
        let at = SegmentedAddress::new(0, 0x100);
        let dst = SegmentedAddress::new(0, 0x1000);
        m.write8(at.wrapping_add(2), 3).unwrap();
        m.write16(at.wrapping_add(14), dst.offset).unwrap();
        m.write8(dst, 10).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read8(dst.wrapping_add(2)).unwrap(), 2);
        m.write8(dst, 11).unwrap();
        m.write8(dst.wrapping_add(1), 2).unwrap();
        m.write16(at.wrapping_add(18), 0).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read8(dst.wrapping_add(6)).unwrap(), 0);
        m.write8(at.wrapping_add(2), 0x84).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x8108);
    }
    #[test]
    fn mounted_track_read_and_missing_audio_are_not_fabricated() {
        let mut iso = vec![0; 18 * 2048];
        iso[17 * 2048] = 42;
        let mut cd = CdRom {
            iso,
            tracks: vec![(0, false)],
            leadout: 18,
            head_sector: 0,
            audio: None,
            legacy_header13: false,
        };
        let mut m = GuestMemory::new(0x10000, A20::Enabled);
        let at = SegmentedAddress::new(0, 0x100);
        m.write8(at.wrapping_add(2), 0x80).unwrap();
        m.write16(at.wrapping_add(14), 0x1000).unwrap();
        m.write16(at.wrapping_add(18), 1).unwrap();
        m.write16(at.wrapping_add(20), 17).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read8(SegmentedAddress::new(0, 0x1000)).unwrap(), 42);
        assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x100);
        m.write8(at.wrapping_add(2), 0x84).unwrap();
        cd.request(&mut m, at).unwrap();
        assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x8108);
    }
}
