// SPDX-License-Identifier: GPL-3.0-or-later
//! Silent authored transports only: no PipeWire connection or audio device.
use audio_output::{AudioOutput, MAX_ADVANCE_FRAMES, MAX_BUFFER_FRAMES};
use cdrom::CdRom;
use pal_wayland::semantics::{A20, GuestMemory, SegmentedAddress};
use pal_wayland::{audio, audio_output, cdrom};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "pal-pcm-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&dir).unwrap();
        let mut cooked = vec![0; 18 * 2048];
        cooked[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        fs::write(dir.join("disc.iso"), &cooked).unwrap();
        let mut bin = Vec::new();
        for data in cooked.chunks_exact(2048) {
            let mut sector = [0; audio::SECTOR_BYTES];
            sector[15] = 1;
            sector[16..2064].copy_from_slice(data);
            bin.extend(sector);
        }
        for _ in 0..4 * audio::FRAMES_PER_SECTOR {
            bin.extend([0x1234i16, -0x1234].into_iter().flat_map(i16::to_le_bytes));
        }
        fs::write(dir.join("disc.bin"), bin).unwrap();
        fs::write(dir.join("disc.cue"), "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
        Self { dir }
    }
    fn cd(&self) -> CdRom {
        CdRom::mount_bin_cue(&self.dir.join("disc.bin"), &self.dir.join("disc.cue")).unwrap()
    }
    fn transport(&self, body: &str) -> AudioOutput {
        let path = self.dir.join("transport.py");
        fs::write(&path, body).unwrap();
        let mut command = Command::new("python3");
        command.arg(path).arg(self.dir.join("pcm.raw"));
        AudioOutput::spawn_transport(command).unwrap()
    }
    fn play(&self, cd: &mut CdRom) {
        let at = SegmentedAddress::new(0, 0x100);
        let mut memory = GuestMemory::new(0x10000, A20::Enabled);
        memory.write8(at, 22).unwrap();
        memory.write8(at.wrapping_add(2), 0x84).unwrap();
        memory
            .write_bytes(at.wrapping_add(14), &18u32.to_le_bytes())
            .unwrap();
        memory
            .write_bytes(at.wrapping_add(18), &4u32.to_le_bytes())
            .unwrap();
        cd.request(&mut memory, at).unwrap();
        assert_eq!(memory.read16(at.wrapping_add(3)).unwrap(), 0x300);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::env::var_os("PAL_PCM_TEST_KEEP").as_deref() == Some(std::ffi::OsStr::new("1")) {
            eprintln!("PCM fixture retained: {}", self.dir.display());
        } else {
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }
}
const RECORDER: &str = "import sys\ndata = sys.stdin.buffer.read()\nwith open(sys.argv[1], 'xb') as f: f.write(data)\n";
fn frame_ns(frame: u64) -> u64 {
    (frame * 1_000_000_000).div_ceil(44_100)
}

#[test]
fn startup_baseline_pre_play_drain_and_exact_stereo_transport() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    let mut output = fixture.transport(RECORDER);
    let baseline = 999_000_000_000;
    assert_eq!(
        output
            .advance_to(baseline, &mut cd)
            .unwrap()
            .submitted_frames,
        0
    );
    // Drain old stopped state BEFORE applying PLAY; do not retroactively assign
    // this preceding interval to the newly playing source.
    assert_eq!(
        output
            .advance_to(baseline + frame_ns(441), &mut cd)
            .unwrap()
            .source_frames,
        0
    );
    fixture.play(&mut cd);
    let progress = output
        .advance_to(baseline + frame_ns(882), &mut cd)
        .unwrap();
    assert_eq!(
        (progress.submitted_frames, progress.source_frames),
        (882, 441)
    );
    assert_eq!(cd.audio_player().unwrap().position_frames(), 441);
    let final_progress = output.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(final_progress.pipe_written_frames, 882);
    assert_eq!(final_progress.buffered_frames, 0);
    let bytes = fs::read(fixture.dir.join("pcm.raw")).unwrap();
    let mut expected = vec![0; 441 * 4];
    for _ in 0..441 {
        for sample in [0x1234i16, -0x1234] {
            expected.extend(sample.to_ne_bytes());
        }
    }
    assert_eq!(bytes, expected);
}

#[test]
fn clock_budget_and_fractional_accumulation_do_not_skip_source() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    fixture.play(&mut cd);
    let mut output = fixture.transport(RECORDER);
    output.advance_to(1_000_000, &mut cd).unwrap();
    assert!(
        output
            .advance_to(999_999, &mut cd)
            .unwrap_err()
            .0
            .contains("backwards")
    );
    assert!(
        output
            .advance_to(1_000_000 + frame_ns(MAX_ADVANCE_FRAMES as u64 + 1), &mut cd)
            .unwrap_err()
            .0
            .contains("catch-up")
    );
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(output.progress().submitted_frames, 0);
    for delta in [10_000, 20_000, 30_000, 40_000, 50_000] {
        output.advance_to(1_000_000 + delta, &mut cd).unwrap();
    }
    assert_eq!(output.progress().submitted_frames, 2);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 2);
    assert_eq!(
        output
            .finish(Duration::from_secs(2))
            .unwrap()
            .pipe_written_frames,
        2
    );
}

#[test]
fn missing_helper_and_unsolicited_exit_fail_closed() {
    let fixture = Fixture::new();
    assert!(
        AudioOutput::spawn_transport(Command::new(fixture.dir.join("missing-helper"))).is_err()
    );
    let mut command = Command::new("python3");
    command.args(["-c", "import os; os._exit(7)"]);
    let result = AudioOutput::spawn_transport(command);
    if let Ok(mut output) = result {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(error) = output.poll() {
                assert!(error.0.contains("exited"));
                assert!(output.poll().is_err());
                break;
            }
            assert!(Instant::now() < deadline, "did not detect helper exit");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn blocked_transport_backpressure_precedes_source_drain_and_drop_reaps() {
    let fixture = Fixture::new();
    let mut output = fixture.transport("import time\ntime.sleep(30)\n");
    let mut cd = fixture.cd();
    output.advance_to(0, &mut cd).unwrap();
    fixture.play(&mut cd);
    let mut timestamp = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let next = timestamp + 50_000_000;
        let before = output.progress();
        let source_before = cd.audio_player().unwrap().position_frames();
        match output.advance_to(next, &mut cd) {
            Ok(progress) => {
                assert!(progress.buffered_frames <= MAX_BUFFER_FRAMES);
                timestamp = next;
                assert!(Instant::now() < deadline, "socket never saturated");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                assert!(error.0.contains("backpressure"), "{error}");
                assert_eq!(output.progress().submitted_frames, before.submitted_frames);
                // Admission failure is retryable: the worker can free capacity
                // immediately afterwards. Inspect this rejected call, not a
                // second call racing the worker's socket write.
                assert_eq!(cd.audio_player().unwrap().position_frames(), source_before);
                break;
            }
        }
    }
    let start = Instant::now();
    drop(output); // No wait for the 30-second sleeper or blocked stdin worker.
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn closed_helper_stdin_reports_real_worker_write_failure() {
    let fixture = Fixture::new();
    let mut output = fixture.transport(
        "import os, sys, time\nos.close(0)\nwith open(sys.argv[1], 'xb') as f: f.write(b'ready')\ntime.sleep(30)\n",
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !fixture.dir.join("pcm.raw").exists() {
        assert!(Instant::now() < deadline, "helper did not close stdin");
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut cd = fixture.cd();
    output.advance_to(0, &mut cd).unwrap();
    output.advance_to(frame_ns(1), &mut cd).unwrap();
    loop {
        if let Err(error) = output.poll() {
            assert!(error.0.contains("helper write"), "{error}");
            assert!(output.poll().is_err());
            assert_eq!(output.progress().pipe_written_frames, 0);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not report failed write"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn finish_timeout_is_error_and_cancels_blocked_helper() {
    let fixture = Fixture::new();
    let output = fixture.transport("import time\ntime.sleep(30)\n");
    let start = Instant::now();
    assert!(
        output
            .finish(Duration::from_millis(20))
            .unwrap_err()
            .0
            .contains("timed out")
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}
