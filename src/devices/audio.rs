// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only BIN/CUE CD-DA and a pull-driven, bounded PCM queue (std only).
//!
//! Format references (not an emulator dependency):
//! * cdrdao's manual: AUDIO sectors are 2352 bytes, 588 stereo frames,
//!   signed 16-bit stereo at 44100 Hz, with 75 sectors/second:
//!   <https://github.com/cdrdao/cdrdao/blob/master/dao/cdrdao.man>.
//! * BIN/CUE **BINARY** images use little-endian samples, unlike cdrdao TOC
//!   raw audio's big-endian convention. DOSBox Staging's BinaryFile::getEndian
//!   explicitly returns S16LE, and BinaryFile::getRate/getChannels give 44100/2:
//!   <https://github.com/dosbox-staging/dosbox-staging/blob/main/src/dos/cdrom_image.cpp>
//!   <https://github.com/dosbox-staging/dosbox-staging/blob/main/src/dos/cdrom.h>
//!   cdrdao's toc2cue conversion also documents the audio byte-swap distinction:
//!   <https://github.com/cdrdao/cdrdao/blob/master/utils/toc2cue.cc>.
//!
//! CUE INDEX positions are file-relative sectors; do NOT add the 150-sector
//! MSCDEX absolute-MSF bias here. This deliberately supports only a single
//! BINARY file, MODE1/2352 or AUDIO tracks, and INDEX 01. Explicit pregaps,
//! INDEX 00, compressed tracks and pre-emphasis are rejected, not guessed.
//! No device, GUI, clock, thread, guest execution, or host audio is opened.
//! `prefetch` performs explicit read-only I/O; `mix_into`/`render_into` do not.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
pub const SECTOR_BYTES: usize = 2352;
pub const FRAMES_PER_SECTOR: u64 = 588;
pub const SECTORS_PER_SECOND: u32 = 75;
pub type StereoFrame = [i16; CHANNELS];
const MAX_CUE_BYTES: u64 = 65_536;
const MAX_QUEUE_FRAMES: usize = SAMPLE_RATE as usize * 10;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Frames are the authoritative exact duration. Duration truncates fractional
/// nanoseconds (less than one ns); no millisecond or floating-point drift.
pub fn frames_duration(frames: u64) -> Duration {
    let rate = u64::from(SAMPLE_RATE);
    Duration::new(
        frames / rate,
        ((frames % rate) * 1_000_000_000 / rate) as u32,
    )
}

/// Decode whole raw sectors. Each four bytes are little-endian left, right.
/// Validation precedes writes, so malformed input leaves `output` unchanged.
pub fn decode_sectors(raw: &[u8], output: &mut [StereoFrame]) -> io::Result<()> {
    if !raw.len().is_multiple_of(SECTOR_BYTES) || output.len() != raw.len() / 4 {
        return Err(invalid("CD-DA input/output is not whole-sector aligned"));
    }
    for (bytes, frame) in raw.chunks_exact(4).zip(output) {
        *frame = [
            i16::from_le_bytes([bytes[0], bytes[1]]),
            i16::from_le_bytes([bytes[2], bytes[3]]),
        ];
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CdTrack {
    pub number: u8,
    pub audio: bool,
    pub start_sector: u64,
    pub end_sector: u64,
}
impl CdTrack {
    pub fn sectors(self) -> u64 {
        self.end_sector - self.start_sector
    }
    pub fn frames(self) -> u64 {
        self.sectors() * FRAMES_PER_SECTOR
    }
    pub fn duration(self) -> Duration {
        frames_duration(self.frames())
    }
}

fn cue_sector(text: &str) -> io::Result<u64> {
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|s| s.len() != 2 || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("expected CUE MM:SS:FF"));
    }
    let values: Vec<u64> = parts.iter().map(|s| s.parse().unwrap()).collect();
    if values[1] >= 60 || values[2] >= 75 {
        return Err(invalid("invalid CUE MSF"));
    }
    Ok(values[0] * 4500 + values[1] * 75 + values[2])
}

fn parse_cue(text: &str, bin: &Path, sectors: u64) -> io::Result<Vec<CdTrack>> {
    let mut found_file = false;
    let mut tracks: Vec<CdTrack> = Vec::new();
    let mut indexed = true;
    for line in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let words: Vec<_> = line.split_whitespace().collect();
        match words[0] {
            "REM" => {}
            "FILE" => {
                if found_file || !tracks.is_empty() {
                    return Err(invalid("only one BINARY CUE file is supported"));
                }
                let name = line.strip_prefix("FILE").unwrap().trim();
                let name = name
                    .strip_prefix('"')
                    .and_then(|s| s.split_once('"'))
                    .filter(|(_, rest)| rest.trim() == "BINARY")
                    .map(|(name, _)| name)
                    .ok_or_else(|| invalid("expected quoted FILE name and BINARY"))?;
                if bin.file_name().and_then(|s| s.to_str()) != Some(name) {
                    return Err(invalid("CUE FILE does not match explicit BIN basename"));
                }
                found_file = true;
            }
            "TRACK" => {
                if !found_file || !indexed || words.len() != 3 || tracks.len() >= 99 {
                    return Err(invalid("invalid CUE TRACK or missing INDEX 01"));
                }
                let number: u8 = words[1]
                    .parse()
                    .map_err(|_| invalid("invalid track number"))?;
                if number as usize != tracks.len() + 1 {
                    return Err(invalid("CUE tracks must be sequential from 1"));
                }
                let audio = match words[2] {
                    "AUDIO" => true,
                    "MODE1/2352" => false,
                    _ => return Err(invalid("unsupported CUE track encoding")),
                };
                tracks.push(CdTrack {
                    number,
                    audio,
                    start_sector: 0,
                    end_sector: 0,
                });
                indexed = false;
            }
            "INDEX" => {
                if indexed || tracks.is_empty() || words.len() != 3 || words[1] != "01" {
                    return Err(invalid("only one INDEX 01 per track is supported"));
                }
                let start = cue_sector(words[2])?;
                if start >= sectors
                    || (tracks.len() == 1 && start != 0)
                    || (tracks.len() > 1 && start <= tracks[tracks.len() - 2].start_sector)
                {
                    return Err(invalid("invalid CUE track range"));
                }
                let last = tracks.len() - 1;
                tracks[last].start_sector = start;
                if last > 0 {
                    tracks[last - 1].end_sector = start;
                }
                indexed = true;
            }
            _ => {
                return Err(invalid(
                    "unsupported CUE directive (pregap/flags/metadata not inferred)",
                ));
            }
        }
    }
    if !indexed || tracks.is_empty() {
        return Err(invalid("CUE has no complete tracks"));
    }
    tracks.last_mut().unwrap().end_sector = sectors;
    Ok(tracks)
}

/// Keeps a read-only file handle, not a 237 MB duplicate of the disc image.
#[derive(Debug)]
pub struct CdAudioImage {
    file: File,
    tracks: Vec<CdTrack>,
    sectors: u64,
    // Private staging preserves output atomicity on short reads. Reuse the
    // high-water capacity instead of allocating on every player refill.
    raw: Vec<u8>,
}
impl CdAudioImage {
    pub fn mount_bin_cue(bin: &Path, cue: &Path) -> io::Result<Self> {
        let file = File::open(bin)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() % SECTOR_BYTES as u64 != 0 {
            return Err(invalid(
                "expected a nonempty sector-aligned regular BIN file",
            ));
        }
        let mut text = String::new();
        File::open(cue)?
            .take(MAX_CUE_BYTES + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_CUE_BYTES {
            return Err(invalid("CUE exceeds size limit"));
        }
        let sectors = metadata.len() / SECTOR_BYTES as u64;
        let tracks = parse_cue(&text, bin, sectors)?;
        Ok(Self {
            file,
            tracks,
            sectors,
            raw: Vec::new(),
        })
    }
    pub fn tracks(&self) -> &[CdTrack] {
        &self.tracks
    }
    pub fn sectors(&self) -> u64 {
        self.sectors
    }
    pub fn track(&self, number: u8) -> io::Result<CdTrack> {
        number
            .checked_sub(1)
            .and_then(|n| self.tracks.get(n as usize))
            .copied()
            .ok_or_else(|| invalid("track number out of range"))
    }
    fn validate_audio_frames(&self, start: u64, count: u64) -> io::Result<()> {
        let end = start
            .checked_add(count)
            .ok_or_else(|| invalid("audio range overflow"))?;
        if end > self.sectors * FRAMES_PER_SECTOR {
            return Err(invalid("audio range exceeds leadout"));
        }
        if count == 0 {
            return Ok(());
        }
        let mut cursor = start;
        for track in &self.tracks {
            let first = track.start_sector * FRAMES_PER_SECTOR;
            let last = track.end_sector * FRAMES_PER_SECTOR;
            if cursor >= last {
                continue;
            }
            if cursor < first || !track.audio {
                return Err(invalid("audio range intersects a data track"));
            }
            cursor = end.min(last);
            if cursor == end {
                return Ok(());
            }
        }
        Err(invalid("audio range not covered by tracks"))
    }
    /// Read arbitrary PCM-frame slices, never data-track bytes. On any read
    /// failure the caller's output is unchanged. File truncation returns EOF.
    pub fn read_frames(&mut self, start: u64, output: &mut [StereoFrame]) -> io::Result<()> {
        self.validate_audio_frames(start, output.len() as u64)?;
        let byte_count = output
            .len()
            .checked_mul(4)
            .ok_or_else(|| invalid("audio buffer overflow"))?;
        self.raw
            .try_reserve_exact(byte_count.saturating_sub(self.raw.len()))
            .map_err(|_| invalid("audio buffer allocation failed"))?;
        self.raw.resize(byte_count, 0);
        self.file.seek(SeekFrom::Start(
            start
                .checked_mul(4)
                .ok_or_else(|| invalid("audio offset overflow"))?,
        ))?;
        self.file.read_exact(&mut self.raw)?;
        for (bytes, frame) in self.raw.chunks_exact(4).zip(output) {
            *frame = [
                i16::from_le_bytes([bytes[0], bytes[1]]),
                i16::from_le_bytes([bytes[2], bytes[3]]),
            ];
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    Finished,
}

/// Per-channel gain: 0 = mute, 255 = unity; integer division truncates toward 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StereoVolume {
    pub left: u8,
    pub right: u8,
}
impl Default for StereoVolume {
    fn default() -> Self {
        Self {
            left: 255,
            right: 255,
        }
    }
}

/// Position advances only when the consumer pulls source frames, never during
/// prefetch or pause. Underrun outputs silence without advancing the position.
/// This does not report speaker/audible position: there is no device clock.
#[derive(Debug)]
pub struct CdPlayer {
    image: CdAudioImage,
    queue: VecDeque<StereoFrame>,
    // Bounded by queue capacity; a failed read never publishes staged frames.
    prefetch_frames: Vec<StereoFrame>,
    capacity: usize,
    start_frame: u64,
    end_frame: u64,
    position: u64,
    state: PlaybackState,
    volume: StereoVolume,
}
impl CdPlayer {
    pub fn new(image: CdAudioImage, capacity_frames: usize) -> io::Result<Self> {
        if capacity_frames == 0 || capacity_frames > MAX_QUEUE_FRAMES {
            return Err(invalid("queue capacity must be 1..=441000 stereo frames"));
        }
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(capacity_frames)
            .map_err(|_| invalid("queue allocation failed"))?;
        let mut prefetch_frames = Vec::new();
        prefetch_frames
            .try_reserve_exact(capacity_frames)
            .map_err(|_| invalid("prefetch allocation failed"))?;
        Ok(Self {
            image,
            queue,
            prefetch_frames,
            capacity: capacity_frames,
            start_frame: 0,
            end_frame: 0,
            position: 0,
            state: PlaybackState::Stopped,
            volume: StereoVolume::default(),
        })
    }
    pub fn image(&self) -> &CdAudioImage {
        &self.image
    }
    pub fn state(&self) -> PlaybackState {
        self.state
    }
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }
    pub fn position_frames(&self) -> u64 {
        self.position
    }
    pub fn absolute_frame(&self) -> u64 {
        self.start_frame + self.position
    }
    pub fn duration_frames(&self) -> u64 {
        self.end_frame - self.start_frame
    }
    pub fn position_duration(&self) -> Duration {
        frames_duration(self.position)
    }
    pub fn duration(&self) -> Duration {
        frames_duration(self.duration_frames())
    }
    pub fn volume(&self) -> StereoVolume {
        self.volume
    }
    pub fn set_volume(&mut self, volume: StereoVolume) {
        self.volume = volume;
    }
    pub fn play_track(&mut self, number: u8) -> io::Result<()> {
        let track = self.image.track(number)?;
        if !track.audio {
            return Err(invalid("cannot play a data track"));
        }
        self.play_sectors(track.start_sector, track.sectors())
    }
    /// Replace the selection atomically after validating the entire range.
    /// Adjacent audio tracks can be selected together; data crossing is rejected.
    pub fn play_sectors(&mut self, start: u64, count: u64) -> io::Result<()> {
        if count == 0 {
            return Err(invalid("empty playback range"));
        }
        let first = start
            .checked_mul(FRAMES_PER_SECTOR)
            .ok_or_else(|| invalid("audio start overflow"))?;
        let frames = count
            .checked_mul(FRAMES_PER_SECTOR)
            .ok_or_else(|| invalid("audio length overflow"))?;
        self.image.validate_audio_frames(first, frames)?;
        self.queue.clear();
        self.start_frame = first;
        self.end_frame = first + frames;
        self.position = 0;
        self.state = PlaybackState::Playing;
        Ok(())
    }
    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            self.state = PlaybackState::Paused;
        }
    }
    pub fn resume(&mut self) {
        if self.state == PlaybackState::Paused {
            self.state = PlaybackState::Playing;
        }
    }
    /// Clears all pending frames and selection; resume cannot resurrect audio.
    pub fn stop(&mut self) {
        self.queue.clear();
        self.start_frame = 0;
        self.end_frame = 0;
        self.position = 0;
        self.state = PlaybackState::Stopped;
    }
    /// Explicit producer operation, bounded by queue capacity. May do file I/O
    /// with reusable staging buffers. Never call from a realtime device callback.
    /// Read errors leave queue, selection and position unchanged.
    pub fn prefetch(&mut self) -> io::Result<usize> {
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            return Ok(0);
        }
        let next = self.absolute_frame() + self.queue.len() as u64;
        let count = (self.end_frame - next).min((self.capacity - self.queue.len()) as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        self.prefetch_frames.resize(count, [0, 0]);
        self.image.read_frames(next, &mut self.prefetch_frames)?;
        self.queue.extend(self.prefetch_frames.iter().copied());
        Ok(count)
    }
    /// Add queued audio to existing stereo PCM with independent gains and final
    /// signed-16 saturation. No allocation, locking, I/O, or implicit prefetch.
    /// Returns source frames consumed. Leaves the remaining destination intact.
    pub fn mix_into(&mut self, output: &mut [StereoFrame]) -> usize {
        if self.state != PlaybackState::Playing {
            return 0;
        }
        let count = output.len().min(self.queue.len());
        for destination in &mut output[..count] {
            let source = self.queue.pop_front().unwrap();
            for (channel, gain) in [self.volume.left, self.volume.right]
                .into_iter()
                .enumerate()
            {
                let scaled = i32::from(source[channel]) * i32::from(gain) / 255;
                destination[channel] = (i32::from(destination[channel]) + scaled)
                    .clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                    as i16;
            }
        }
        self.position += count as u64;
        if self.absolute_frame() == self.end_frame {
            self.state = PlaybackState::Finished;
        }
        count
    }
    /// Overwrite a buffer; silence fills pause/stop/underrun and the EOF tail.
    pub fn render_into(&mut self, output: &mut [StereoFrame]) -> usize {
        output.fill([0, 0]);
        self.mix_into(output)
    }
    /// Device-adapter-friendly interleaved i16 stereo output at exactly 44100 Hz.
    /// Odd-length buffers are rejected before any output/position mutation.
    pub fn render_interleaved(&mut self, output: &mut [i16]) -> io::Result<usize> {
        if !output.len().is_multiple_of(2) {
            return Err(invalid("interleaved output needs complete stereo frames"));
        }
        let mut consumed = 0;
        for pair in output.chunks_exact_mut(2) {
            let mut frame = [[0, 0]];
            consumed += self.render_into(&mut frame);
            pair.copy_from_slice(&frame[0]);
        }
        Ok(consumed)
    }
}
