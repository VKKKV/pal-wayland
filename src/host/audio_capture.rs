// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit, silent CD-DA capture on the main thread, never an audio callback.
//!
//! The caller authorizes the exact output path and supplies elapsed nanoseconds
//! (e.g. Instant::elapsed, or an explicit fixture clock). No clock, device,
//! thread, directory creation or guest execution is hidden here. Advance before
//! applying a playback transition to assign the preceding interval correctly.
//! Finalization is mandatory: dropping a capture does not claim a valid WAV.
use crate::{
    audio::{PlaybackState, SAMPLE_RATE},
    cdrom::CdRom,
    runtime::Fault,
};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::Path,
};

/// Hard CPU/I/O budget per advance. Larger intervals are rejected BEFORE any
/// clock/player/file mutation, never truncated or silently skipped. Call more
/// often (or explicitly catch up with intermediate timestamps).
pub const MAX_ADVANCE_FRAMES: u64 = SAMPLE_RATE as u64;
/// RIFF size is 36 + 4 * stereo frames and must fit an unsigned DWORD.
pub const MAX_WAV_FRAMES: u64 = (u32::MAX as u64 - 36) / 4;
const CHUNK_FRAMES: usize = 1024;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureProgress {
    /// Total timeline frames (including silence), not just CD source frames.
    pub frames_written: u64,
    /// Total real source frames pulled from CdRom, including muted PCM.
    pub source_frames: u64,
    /// Independent SB timeline frames drained, including silence; not CD PCM.
    pub sfx_frames: u64,
}

/// Bounded file sink: only exclusive creation, no default output path.
pub struct Capture {
    sink: CaptureSink<File>,
}
impl Capture {
    /// max_frames is an explicit total output budget, in 44100-Hz stereo frames.
    /// Validate limits before creating anything; existing paths are untouched.
    pub fn create(path: &Path, max_frames: u64) -> Result<Self, Fault> {
        validate_limit(max_frames)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| io_fault("create", e))?;
        Ok(Self {
            sink: CaptureSink::new(file, max_frames)?,
        })
    }
    /// Emits floor(elapsed_ns * 44100 / 1e9) total frames. Absolute integer
    /// conversion carries fractional frames across every call without drift.
    /// While Playing, refill and drain consecutive queue slices until the
    /// interval is complete. Only pause/stop/EOF/no player produce silence.
    /// Any source/output I/O failure permanently poisons this capture; a
    /// partially written file is evidence of failure, not a successful WAV.
    pub fn advance_to(
        &mut self,
        elapsed_ns: u64,
        cd: &mut CdRom,
    ) -> Result<CaptureProgress, Fault> {
        self.sink.advance_to(elapsed_ns, cd)
    }
    /// Mixed CD/SFX consumer, using the same absolute timeline as the Machine.
    /// Output budget checks happen before hardware advance or source draining.
    pub fn advance_machine(
        &mut self,
        ns: u64,
        m: &mut crate::runtime::Machine,
    ) -> Result<CaptureProgress, Fault> {
        self.sink.preflight(ns)?;
        m.advance_sound_to(ns)?;
        self.sink.advance_with(ns, |pcm| m.render_mixed_audio(pcm))
    }
    pub fn progress(&self) -> CaptureProgress {
        self.sink.progress()
    }
    /// Checks and writes RIFF lengths, flushes and syncs the actual file.
    /// No implicit finalization on Drop: the caller must handle this result.
    pub fn finalize(mut self) -> Result<CaptureProgress, Fault> {
        let progress = self.sink.finalize()?;
        self.sink
            .writer
            .sync_all()
            .map_err(|e| io_fault("sync", e))?;
        Ok(progress)
    }
}

/// Refill within each slice; source EOF yields silence without invented progress.
fn render_cd(cd: &mut CdRom, pcm: &mut [[i16; 2]]) -> Result<usize, Fault> {
    let mut offset = 0;
    let mut consumed = 0;
    while offset < pcm.len() {
        let mut count = pcm.len() - offset;
        if cd
            .audio_player()
            .is_some_and(|p| p.state() == PlaybackState::Playing)
        {
            cd.prefetch_audio()
                .map_err(|e| Fault(format!("WAV capture CD prefetch: {e}")))?;
            let queued = cd.audio_player().unwrap().queued_frames();
            if queued == 0 {
                return Err(Fault("WAV capture playing source has no queued PCM".into()));
            }
            count = count.min(queued);
        }
        consumed += cd.render_audio(&mut pcm[offset..offset + count]);
        offset += count;
    }
    Ok(consumed)
}
fn validate_limit(max_frames: u64) -> Result<(), Fault> {
    if max_frames == 0 || max_frames > MAX_WAV_FRAMES {
        return Err(Fault(format!(
            "WAV max_frames must be 1..={MAX_WAV_FRAMES}"
        )));
    }
    Ok(())
}
fn io_fault(operation: &str, error: std::io::Error) -> Fault {
    Fault(format!("WAV capture {operation}: {error}"))
}
fn header(frames: u64) -> Result<[u8; 44], Fault> {
    let data = frames
        .checked_mul(4)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| Fault("WAV data length overflow".into()))?;
    let riff = data
        .checked_add(36)
        .ok_or_else(|| Fault("WAV RIFF length overflow".into()))?;
    let mut bytes = [0; 44];
    bytes[0..4].copy_from_slice(b"RIFF");
    bytes[4..8].copy_from_slice(&riff.to_le_bytes());
    bytes[8..16].copy_from_slice(b"WAVEfmt ");
    bytes[16..20].copy_from_slice(&16u32.to_le_bytes());
    bytes[20..22].copy_from_slice(&1u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&2u16.to_le_bytes());
    bytes[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes[28..32].copy_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    bytes[32..34].copy_from_slice(&4u16.to_le_bytes());
    bytes[34..36].copy_from_slice(&16u16.to_le_bytes());
    bytes[36..40].copy_from_slice(b"data");
    bytes[40..44].copy_from_slice(&data.to_le_bytes());
    Ok(bytes)
}

// Generic only internally, to exercise genuine write/seek/flush failures.
struct CaptureSink<W> {
    writer: W,
    max_frames: u64,
    last_ns: u64,
    frames_written: u64,
    source_frames: u64,
    sfx_frames: u64,
    poisoned: bool,
}
impl<W: Write + Seek> CaptureSink<W> {
    fn new(mut writer: W, max_frames: u64) -> Result<Self, Fault> {
        validate_limit(max_frames)?;
        writer
            .write_all(&header(0)?)
            .map_err(|e| io_fault("header", e))?;
        Ok(Self {
            writer,
            max_frames,
            last_ns: 0,
            frames_written: 0,
            source_frames: 0,
            sfx_frames: 0,
            poisoned: false,
        })
    }
    fn progress(&self) -> CaptureProgress {
        CaptureProgress {
            frames_written: self.frames_written,
            source_frames: self.source_frames,
            sfx_frames: self.sfx_frames,
        }
    }
    fn healthy(&self) -> Result<(), Fault> {
        if self.poisoned {
            Err(Fault(
                "WAV capture is poisoned after an I/O/source failure".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn advance_to(&mut self, ns: u64, cd: &mut CdRom) -> Result<CaptureProgress, Fault> {
        self.advance_with(ns, |pcm| render_cd(cd, pcm).map(|n| (n, 0)))
    }
    fn preflight(&self, ns: u64) -> Result<u64, Fault> {
        self.healthy()?;
        if ns < self.last_ns {
            return Err(Fault("WAV capture timestamp moved backwards".into()));
        }
        let target = (u128::from(ns) * u128::from(SAMPLE_RATE) / NANOS_PER_SECOND) as u64;
        if target > self.max_frames {
            return Err(Fault("WAV capture total frame budget exceeded".into()));
        }
        let pending = target - self.frames_written;
        if pending > MAX_ADVANCE_FRAMES {
            return Err(Fault(
                "WAV capture per-call frame budget exceeded; elapsed debt retained".into(),
            ));
        }
        Ok(target)
    }
    fn advance_with(
        &mut self,
        ns: u64,
        mut render: impl FnMut(&mut [[i16; 2]]) -> Result<(usize, usize), Fault>,
    ) -> Result<CaptureProgress, Fault> {
        let target = self.preflight(ns)?;
        let mut pcm = [[0; 2]; CHUNK_FRAMES];
        let mut raw = [0; CHUNK_FRAMES * 4];
        while self.frames_written < target {
            let count = (target - self.frames_written).min(CHUNK_FRAMES as u64) as usize;
            let (consumed, sfx) = match render(&mut pcm[..count]) {
                Ok(n) => n,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            self.source_frames += consumed as u64;
            self.sfx_frames += sfx as u64;
            for (frame, bytes) in pcm[..count].iter().zip(raw.chunks_exact_mut(4)) {
                bytes[..2].copy_from_slice(&frame[0].to_le_bytes());
                bytes[2..].copy_from_slice(&frame[1].to_le_bytes());
            }
            if let Err(error) = self.writer.write_all(&raw[..count * 4]) {
                self.poisoned = true;
                return Err(io_fault("PCM write", error));
            }
            self.frames_written += count as u64;
        }
        self.last_ns = ns;
        Ok(self.progress())
    }
    fn finalize(&mut self) -> Result<CaptureProgress, Fault> {
        self.healthy()?;
        let result = (|| {
            let bytes = header(self.frames_written)?;
            self.writer
                .seek(SeekFrom::Start(0))
                .map_err(|e| io_fault("header seek", e))?;
            self.writer
                .write_all(&bytes)
                .map_err(|e| io_fault("final header", e))?;
            self.writer.flush().map_err(|e| io_fault("flush", e))?;
            Ok(self.progress())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct FailingWriter {
        bytes: Cursor<Vec<u8>>,
        fail_write: bool,
        fail_seek: bool,
        fail_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::other("injected write failure"));
            }
            self.bytes.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::other("injected flush failure"));
            }
            Ok(())
        }
    }
    impl Seek for FailingWriter {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if self.fail_seek {
                return Err(io::Error::other("injected seek failure"));
            }
            self.bytes.seek(position)
        }
    }
    fn writer() -> FailingWriter {
        FailingWriter {
            bytes: Cursor::new(Vec::new()),
            fail_write: false,
            fail_seek: false,
            fail_flush: false,
        }
    }
    #[test]
    fn header_rejects_riff_overflow_and_accepts_exact_boundary() {
        assert!(header(MAX_WAV_FRAMES).is_ok());
        assert!(header(MAX_WAV_FRAMES + 1).is_err());
        assert!(header(u64::MAX).is_err());
    }
    #[test]
    fn initial_header_failure_is_propagated() {
        let mut output = writer();
        output.fail_write = true;
        assert!(CaptureSink::new(output, 1).is_err());
    }
    #[test]
    fn pcm_write_failure_is_terminal_and_never_counts_unwritten_frames() {
        let directory = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap())
            .join(format!("pal-wav-write-failure-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("disc.iso");
        let mut iso = vec![0; 18 * 2048];
        iso[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        std::fs::write(&path, iso).unwrap();
        let mut cd = CdRom::mount(&path).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let mut sink = CaptureSink::new(writer(), 1).unwrap();
        sink.writer.fail_write = true;
        assert!(
            sink.advance_to(22_676, &mut cd)
                .unwrap_err()
                .0
                .contains("injected")
        );
        assert_eq!(sink.progress().frames_written, 0);
        assert!(
            sink.advance_to(22_676, &mut cd)
                .unwrap_err()
                .0
                .contains("poisoned")
        );
        assert!(sink.finalize().unwrap_err().0.contains("poisoned"));
    }
    #[test]
    fn final_header_seek_write_and_flush_failures_poison_sink() {
        for failure in 0..3 {
            let mut sink = CaptureSink::new(writer(), 1).unwrap();
            sink.writer.fail_seek = failure == 0;
            sink.writer.fail_write = failure == 1;
            sink.writer.fail_flush = failure == 2;
            let error = sink.finalize().unwrap_err();
            assert!(error.0.contains("injected"));
            assert!(sink.finalize().unwrap_err().0.contains("poisoned"));
        }
    }
}
