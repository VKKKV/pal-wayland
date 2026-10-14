// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored disc fixtures only. No device output or guest execution.
use pal_wayland::{audio, audio_capture, cdrom};

use audio::{FRAMES_PER_SECTOR, PlaybackState, SECTOR_BYTES};
use audio_capture::{Capture, MAX_ADVANCE_FRAMES, MAX_WAV_FRAMES};
use cdrom::CdRom;
use pal_wayland::semantics::{A20, GuestMemory, SegmentedAddress};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const AT: SegmentedAddress = SegmentedAddress::new(0, 0x100);
struct Fixture {
    dir: PathBuf,
    bin: PathBuf,
    cue: PathBuf,
    iso: PathBuf,
    sectors: u32,
}
impl Fixture {
    fn new(sectors: u32) -> Self {
        let dir =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "pal-wav-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&dir).unwrap();
        let bin = dir.join("disc.bin");
        let cue = dir.join("disc.cue");
        let iso = dir.join("disc.iso");
        let mut cooked = vec![0; 18 * 2048];
        cooked[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        fs::write(&iso, &cooked).unwrap();
        let mut raw = Vec::new();
        for data in cooked.chunks_exact(2048) {
            let mut sector = [0; SECTOR_BYTES];
            sector[15] = 1;
            sector[16..2064].copy_from_slice(data);
            raw.extend(sector);
        }
        for i in 0..u64::from(sectors) * FRAMES_PER_SECTOR {
            // Distinct signed channels, wrap-free arithmetic even in debug.
            let frame = match i {
                0 => [i16::MIN, i16::MAX],
                1 => [0x1234, -0x1234],
                _ => [(i % 60_000) as i32 as i16, -((i % 30_000) as i16)],
            };
            for sample in frame {
                raw.extend(sample.to_le_bytes());
            }
        }
        fs::write(&bin, raw).unwrap();
        fs::write(&cue, "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
        Self {
            dir,
            bin,
            cue,
            iso,
            sectors,
        }
    }
    fn cd(&self) -> CdRom {
        CdRom::mount_bin_cue(&self.bin, &self.cue).unwrap()
    }
    fn source_bytes(&self) -> Vec<u8> {
        let mut file = File::open(&self.bin).unwrap();
        file.seek(SeekFrom::Start(18 * SECTOR_BYTES as u64))
            .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }
    fn play(&self, cd: &mut CdRom) {
        let mut m = GuestMemory::new(0x10000, A20::Enabled);
        m.write8(AT, 22).unwrap();
        m.write8(AT.wrapping_add(2), 0x84).unwrap();
        m.write_bytes(AT.wrapping_add(14), &18u32.to_le_bytes())
            .unwrap();
        m.write_bytes(AT.wrapping_add(18), &self.sectors.to_le_bytes())
            .unwrap();
        cd.request(&mut m, AT).unwrap();
        assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x300);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn transition(cd: &mut CdRom, command: u8) {
    let mut m = GuestMemory::new(0x10000, A20::Enabled);
    m.write8(AT, 13).unwrap();
    m.write8(AT.wrapping_add(2), command).unwrap();
    cd.request(&mut m, AT).unwrap();
    let expected = if command == 0x88 { 0x300 } else { 0x100 };
    assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), expected);
}
fn frame_ns(frame: u64) -> u64 {
    (frame * 1_000_000_000).div_ceil(44_100)
}
fn wave(path: &std::path::Path, frames: u64) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(bytes.len() as u64, 44 + frames * 4);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64,
        36 + frames * 4
    );
    assert_eq!(&bytes[8..16], b"WAVEfmt ");
    assert_eq!(&bytes[16..24], &[16, 0, 0, 0, 1, 0, 2, 0]);
    assert_eq!(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        44_100
    );
    assert_eq!(
        u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
        176_400
    );
    assert_eq!(&bytes[32..40], &[4, 0, 16, 0, b'd', b'a', b't', b'a']);
    assert_eq!(
        u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as u64,
        frames * 4
    );
    bytes[44..].to_vec()
}

#[test]
fn real_pcm_across_queue_refills_and_eof_matches_independent_bin_bytes() {
    let f = Fixture::new(90); // 52920 frames exceeds 8192 queue + one call budget
    let mut cd = f.cd();
    f.play(&mut cd);
    let path = std::env::var_os("PAL_CAPTURE_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| f.dir.join("refill.wav"));
    let mut capture = Capture::create(&path, 60_000).unwrap();
    let first = capture.advance_to(1_000_000_000, &mut cd).unwrap();
    assert_eq!(first.frames_written, MAX_ADVANCE_FRAMES);
    assert_eq!(first.source_frames, 44_100);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 44_100);
    let final_progress = capture.advance_to(frame_ns(60_000), &mut cd).unwrap();
    assert_eq!(final_progress.frames_written, 60_000);
    assert_eq!(final_progress.source_frames, 52_920);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 52_920);
    assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Finished);
    assert_eq!(capture.finalize().unwrap(), final_progress);
    let bytes = wave(&path, 60_000);
    let expected = f.source_bytes();
    assert_eq!(&bytes[..expected.len()], expected);
    assert!(bytes[expected.len()..].iter().all(|b| *b == 0));
}

#[test]
fn sub_sector_pause_resume_and_stopped_intervals_preserve_pcm_order() {
    let f = Fixture::new(3);
    let mut cd = f.cd();
    let path = f.dir.join("transitions.wav");
    let mut capture = Capture::create(&path, 2500).unwrap();
    capture.advance_to(frame_ns(11), &mut cd).unwrap(); // stopped silence
    f.play(&mut cd);
    capture.advance_to(frame_ns(1012), &mut cd).unwrap();
    assert_eq!(cd.audio_player().unwrap().position_frames(), 1001);
    transition(&mut cd, 0x85);
    let queued = cd.audio_player().unwrap().queued_frames();
    capture.advance_to(frame_ns(1113), &mut cd).unwrap();
    assert_eq!(cd.audio_player().unwrap().position_frames(), 1001);
    assert_eq!(cd.audio_player().unwrap().queued_frames(), queued);
    transition(&mut cd, 0x88);
    let progress = capture.advance_to(frame_ns(2500), &mut cd).unwrap();
    assert_eq!(progress.source_frames, 1764);
    assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Finished);
    capture.finalize().unwrap();
    let pcm = wave(&path, 2500);
    let source = f.source_bytes();
    let mut expected = vec![0; 11 * 4];
    expected.extend(&source[..1001 * 4]);
    expected.extend([0; 101 * 4]);
    expected.extend(&source[1001 * 4..]);
    expected.resize(2500 * 4, 0);
    assert_eq!(pcm, expected);
}

#[test]
fn nanosecond_fragmentation_has_zero_frame_drift_and_no_audio_is_silent() {
    let f = Fixture::new(1);
    let mut cd = CdRom::mount(&f.iso).unwrap();
    let path = f.dir.join("clock.wav");
    let mut capture = Capture::create(&path, 150_000).unwrap();
    for step in 0..=10_000u64 {
        let ns = step * 333_333;
        let result = capture.advance_to(ns, &mut cd).unwrap();
        assert_eq!(
            result.frames_written,
            (u128::from(ns) * 44_100 / 1_000_000_000) as u64
        );
        assert_eq!(result.source_frames, 0);
    }
    assert_eq!(capture.progress().frames_written, 146_999);
    capture.finalize().unwrap();
    assert!(wave(&path, 146_999).iter().all(|b| *b == 0));
}

#[test]
fn backwards_clock_even_inside_same_frame_is_atomic_and_recoverable() {
    let f = Fixture::new(1);
    let mut cd = f.cd();
    f.play(&mut cd);
    let path = f.dir.join("backwards.wav");
    let mut capture = Capture::create(&path, 100).unwrap();
    capture.advance_to(100, &mut cd).unwrap();
    assert!(
        capture
            .advance_to(99, &mut cd)
            .unwrap_err()
            .0
            .contains("backwards")
    );
    assert_eq!(capture.progress().frames_written, 0);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    capture.advance_to(frame_ns(100), &mut cd).unwrap();
    assert_eq!(capture.finalize().unwrap().source_frames, 100);
    assert_eq!(wave(&path, 100), f.source_bytes()[..400]);
}

#[test]
fn per_call_bound_keeps_all_debt_and_total_budget_never_overwrites() {
    let f = Fixture::new(1);
    let mut cd = f.cd();
    f.play(&mut cd);
    let path = f.dir.join("bounds.wav");
    let mut capture = Capture::create(&path, 88_200).unwrap();
    assert!(
        capture
            .advance_to(2_000_000_000, &mut cd)
            .unwrap_err()
            .0
            .contains("per-call")
    );
    assert_eq!(capture.progress().frames_written, 0);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(fs::metadata(&path).unwrap().len(), 44);
    capture.advance_to(1_000_000_000, &mut cd).unwrap();
    capture.advance_to(2_000_000_000, &mut cd).unwrap();
    assert!(
        capture
            .advance_to(frame_ns(88_201), &mut cd)
            .unwrap_err()
            .0
            .contains("total")
    );
    assert_eq!(capture.progress().frames_written, 88_200);
    capture.finalize().unwrap();
    wave(&path, 88_200);
}

#[test]
fn existing_path_missing_parent_and_invalid_limits_are_real_errors() {
    let f = Fixture::new(1);
    let path = f.dir.join("existing.wav");
    fs::write(&path, b"keep me").unwrap();
    assert!(Capture::create(&path, 1).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"keep me");
    let invalid = f.dir.join("invalid.wav");
    assert!(Capture::create(&invalid, 0).is_err());
    assert!(Capture::create(&invalid, MAX_WAV_FRAMES + 1).is_err());
    assert!(!invalid.exists());
    assert!(Capture::create(&f.dir.join("absent/output.wav"), 1).is_err());
}

#[test]
fn actual_truncated_source_fails_without_fabricated_silence_or_completion() {
    let f = Fixture::new(20);
    let mut cd = f.cd();
    f.play(&mut cd);
    OpenOptions::new()
        .write(true)
        .open(&f.bin)
        .unwrap()
        .set_len(18 * SECTOR_BYTES as u64)
        .unwrap();
    let path = f.dir.join("truncated.wav");
    let mut capture = Capture::create(&path, 100).unwrap();
    let error = capture.advance_to(frame_ns(100), &mut cd).unwrap_err();
    assert!(error.0.contains("prefetch"));
    assert_eq!(capture.progress().frames_written, 0);
    assert_eq!(capture.progress().source_frames, 0);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(fs::metadata(&path).unwrap().len(), 44);
    assert!(
        capture
            .advance_to(frame_ns(100), &mut cd)
            .unwrap_err()
            .0
            .contains("poisoned")
    );
    assert!(capture.finalize().unwrap_err().0.contains("poisoned"));
}

#[test]
fn explicit_empty_capture_finalizes_valid_zero_length_wave() {
    let f = Fixture::new(1);
    let path = f.dir.join("empty.wav");
    let capture = Capture::create(&path, 1).unwrap();
    assert_eq!(capture.finalize().unwrap().frames_written, 0);
    assert!(wave(&path, 0).is_empty());
}
