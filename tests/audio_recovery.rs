// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored sources and silent stdin helpers; no device/window/original assets.
use pal_wayland::{
    audio_output::{
        AudioOutput, AudioService, MAX_ADVANCE_FRAMES, MAX_BUFFER_FRAMES, RECOVERY_RETRY_INTERVAL,
    },
    runtime::Machine,
    sound_blaster::SoundBlaster,
};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(std::env::var_os("TMPDIR").unwrap())
            .join(format!("pal-recovery-{}-{unique}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn output(&self, blocked: bool) -> AudioOutput {
        let mut c = Command::new("python3");
        c.args([
            "-c",
            if blocked {
                "import time;time.sleep(30)"
            } else {
                "import sys;open(sys.argv[1],'xb').write(sys.stdin.buffer.read())"
            },
        ])
        .arg(self.0.join("pcm.raw"));
        AudioOutput::spawn_transport(c).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn machine() -> Machine {
    let mut b = vec![0; 544];
    for (i, w) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        b[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    let mut m = Machine::from_mz(&b, 0x1010).unwrap();
    m.virtual_elapsed_ns = Some(0);
    m.retain_sfx_pcm = true;
    m.sound_blaster = Some(SoundBlaster::default());
    // 2000 authored unsigned-8 samples at 10 kHz: 200 ms of non-silent SFX.
    // The independent oracle accounts for first byte entering DAC at time zero.
    let samples: Vec<u8> = (0..2000).map(|i| (i % 251) as u8).collect();
    m.memory.write_linear(0x5000, &samples).unwrap();
    for (p, v) in [
        (0xa, 5),
        (0xc, 0),
        (0xb, 0x49),
        (2, 0),
        (2, 0x50),
        (3, 0xcf),
        (3, 7),
        (0x83, 0),
        (0xa, 1),
        (0x22c, 0xd1),
        (0x22c, 0x40),
        (0x22c, 156),
        (0x22c, 0x14),
        (0x22c, 0xcf),
        (0x22c, 7),
    ] {
        m.port_out(p, v, 8).unwrap();
    }
    m
}
fn mount_cd(f: &Fixture, m: &mut Machine) {
    let mut bin = vec![0; 18 * 2352];
    for s in bin.chunks_exact_mut(2352) {
        s[15] = 1;
    }
    bin[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
    for i in 0..8 * 588 {
        for v in [(i + 1) as i16, -(i as i16) - 1] {
            bin.extend(v.to_le_bytes());
        }
    }
    let path = f.0.join("disc.bin");
    fs::write(&path, bin).unwrap();
    fs::write(path.with_extension("cue"), "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
    m.cdrom =
        Some(pal_wayland::cdrom::CdRom::mount_bin_cue(&path, &path.with_extension("cue")).unwrap());
}
fn cd_request(m: &mut Machine, command: u8) {
    use pal_wayland::semantics::SegmentedAddress;
    let at = SegmentedAddress::new(0, 0x100);
    m.memory
        .write8(at, if command == 0x84 { 22 } else { 13 })
        .unwrap();
    m.memory.write8(at.wrapping_add(2), command).unwrap();
    m.memory
        .write_bytes(at.wrapping_add(14), &18u32.to_le_bytes())
        .unwrap();
    m.memory
        .write_bytes(at.wrapping_add(18), &8u32.to_le_bytes())
        .unwrap();
    m.cdrom
        .as_mut()
        .unwrap()
        .request(&mut m.memory, at)
        .unwrap();
}
#[test]
fn cd_play_pause_resume_stop_wait_for_debt_and_preserve_exact_pcm() {
    let f = Fixture::new();
    let mut m = machine();
    m.sound_blaster = None;
    mount_cd(&f, &mut m);
    let mut o = f.output(false);
    o.advance_machine(0, &mut m).unwrap();
    recover(&mut o, &mut m, 60_000_000); // old stopped state, before PLAY
    cd_request(&mut m, 0x84);
    recover(&mut o, &mut m, 160_000_000); // whole 100-ms PLAY debt, before PAUSE
    assert_eq!(
        m.cdrom
            .as_ref()
            .unwrap()
            .audio_player()
            .unwrap()
            .position_frames(),
        4410
    );
    cd_request(&mut m, 0x85);
    recover(&mut o, &mut m, 220_000_000);
    cd_request(&mut m, 0x88);
    recover(&mut o, &mut m, 223_000_000);
    cd_request(&mut m, 0x85);
    cd_request(&mut m, 0x85); // pause then stop
    recover(&mut o, &mut m, 224_000_000);
    let p = o.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(p.source_frames, 4542);
    let mut expected = vec![0; 2646 * 4];
    for i in 0..4410i16 {
        for v in [i + 1, -i - 1] {
            expected.extend(v.to_ne_bytes());
        }
    }
    expected.extend(vec![0; 2646 * 4]);
    for i in 4410..4542i16 {
        for v in [i + 1, -i - 1] {
            expected.extend(v.to_ne_bytes());
        }
    }
    expected.resize(p.submitted_frames as usize * 4, 0);
    assert_eq!(fs::read(f.0.join("pcm.raw")).unwrap(), expected);
}
#[test]
fn missing_cd_bytes_fail_before_new_hardware_or_fifo_effects() {
    let f = Fixture::new();
    let mut m = machine();
    mount_cd(&f, &mut m);
    cd_request(&mut m, 0x84);
    fs::OpenOptions::new()
        .write(true)
        .open(f.0.join("disc.bin"))
        .unwrap()
        .set_len(18 * 2352)
        .unwrap();
    let mut o = f.output(false);
    o.advance_machine(0, &mut m).unwrap();
    let before = m.sound_blaster.as_ref().unwrap().status();
    assert!(
        o.service_machine(100_000_000, &mut m)
            .unwrap_err()
            .0
            .contains("PCM source")
    );
    assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
    assert_eq!(o.progress().submitted_frames, 0);
    assert_eq!(
        m.cdrom
            .as_ref()
            .unwrap()
            .audio_player()
            .unwrap()
            .position_frames(),
        0
    );
    assert!(o.poll().is_err());
}
fn frame_ns(n: u64) -> u64 {
    (n * 1_000_000_000).div_ceil(44100)
}
fn oracle(frames: u64) -> Vec<u8> {
    (1..=frames)
        .flat_map(|i| {
            let at = frame_ns(i);
            let value = if at < 200_000_000 {
                (((at / 100_000) % 251) as i16 - 128) * 256
            } else {
                0
            };
            [value, value].into_iter().flat_map(i16::to_ne_bytes)
        })
        .collect()
}
fn recover(o: &mut AudioOutput, m: &mut Machine, target: u64) -> usize {
    let steps = m.steps;
    let regs = m.regs;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut calls = 0;
    loop {
        let before = o.progress().submitted_frames;
        let s = o.service_machine(target, m).unwrap();
        calls += 1;
        assert!(o.progress().submitted_frames - before <= MAX_ADVANCE_FRAMES as u64);
        assert!(o.progress().buffered_frames <= MAX_BUFFER_FRAMES);
        assert_eq!(m.steps, steps);
        assert_eq!(m.regs, regs);
        if s.is_ready() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(RECOVERY_RETRY_INTERVAL);
    }
    calls
}
#[test]
fn real_60_and_100ms_gaps_recover_in_bounded_byte_exact_slices() {
    for gap in [60, 100] {
        let f = Fixture::new();
        let mut m = machine();
        let mut o = f.output(false);
        m.virtual_elapsed_ns = None;
        m.start_audio_timeline();
        o.advance_machine(0, &mut m).unwrap();
        std::thread::sleep(Duration::from_millis(gap));
        let target = m.elapsed_ns();
        let frames = (u128::from(target) * 44100 / 1_000_000_000) as u64;
        assert!(recover(&mut o, &mut m, target) >= 2);
        let p = o.finish(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (p.submitted_frames, p.sfx_frames, p.pipe_written_frames),
            (frames, frames, frames)
        );
        assert_eq!(p.buffered_frames, 0);
        assert_eq!(fs::read(f.0.join("pcm.raw")).unwrap(), oracle(frames));
        eprintln!("gap={gap}ms elapsed_ns={target} frames={frames} exact_sfx=true debt=0");
    }
}
#[test]
fn moving_wall_clock_catches_up_before_guest_execution_resumes() {
    let f = Fixture::new();
    let mut m = machine();
    let mut o = f.output(false);
    m.virtual_elapsed_ns = None;
    m.start_audio_timeline();
    o.advance_machine(0, &mut m).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut calls = 0;
    let target = loop {
        let target = m.elapsed_ns();
        let before = o.progress().submitted_frames;
        let service = o.service_machine(target, &mut m).unwrap();
        calls += 1;
        assert!(o.progress().submitted_frames - before <= MAX_ADVANCE_FRAMES as u64);
        assert_eq!(m.steps, 0);
        if service.is_ready() {
            break target;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(RECOVERY_RETRY_INTERVAL);
    };
    assert!(calls >= 3);
    let p = o.finish(Duration::from_secs(2)).unwrap();
    let frames = (u128::from(target) * 44100 / 1_000_000_000) as u64;
    assert_eq!(
        (p.submitted_frames, p.pipe_written_frames),
        (frames, frames)
    );
    assert_eq!(fs::read(f.0.join("pcm.raw")).unwrap(), oracle(frames));
    eprintln!(
        "moving_wall elapsed_ns={target} frames={frames} calls={calls} exact_sfx=true debt=0 guest_steps=0"
    );
}

#[test]
fn hardware_ahead_irq_and_ports_are_reconciled_without_backwards_advance() {
    for target in [60_000_000, 100_000_000] {
        let f = Fixture::new();
        let mut m = machine();
        let mut o = f.output(false);
        o.advance_machine(0, &mut m).unwrap();
        m.virtual_elapsed_ns = Some(target);
        assert!(!m.poll_timer_irq().unwrap());
        assert_eq!(m.port_in(0x226, 8).unwrap(), 0xff);
        let hardware = m.sound_blaster.as_ref().unwrap().status();
        assert!(hardware.queued_frames > MAX_ADVANCE_FRAMES);
        assert!(recover(&mut o, &mut m, target) >= 2);
        let end = m.sound_blaster.as_ref().unwrap().status();
        assert_eq!(end.now_ns, hardware.now_ns);
        assert_eq!(end.consumed_bytes, hardware.consumed_bytes);
        assert_eq!(end.queued_frames, 0);
        let p = o.finish(Duration::from_secs(2)).unwrap();
        assert_eq!(
            fs::read(f.0.join("pcm.raw")).unwrap(),
            oracle(p.submitted_frames)
        );
    }
}
#[test]
fn fractional_and_moving_targets_conserve_exact_frame_indices() {
    let f = Fixture::new();
    let mut m = machine();
    let mut o = f.output(false);
    o.advance_machine(0, &mut m).unwrap();
    assert!(matches!(
        o.service_machine(60_000_001, &mut m).unwrap(),
        AudioService::Pending {
            debt_frames: 441,
            ..
        }
    ));
    recover(&mut o, &mut m, 100_000_019);
    for t in [100_000_020, 100_010_000, 100_020_000, 100_030_000] {
        recover(&mut o, &mut m, t);
    }
    let p = o.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(p.submitted_frames, 4411);
    assert_eq!(fs::read(f.0.join("pcm.raw")).unwrap(), oracle(4411));
}
#[test]
fn backwards_observed_target_missing_source_and_stolen_fifo_are_fatal() {
    for case in 0..3 {
        let f = Fixture::new();
        let mut m = machine();
        let mut o = f.output(false);
        o.advance_machine(0, &mut m).unwrap();
        if case == 0 {
            assert!(!o.service_machine(100_000_000, &mut m).unwrap().is_ready());
            let before = m.sound_blaster.as_ref().unwrap().status();
            assert!(
                o.service_machine(99_000_000, &mut m)
                    .unwrap_err()
                    .0
                    .contains("backwards")
            );
            assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
        } else if case == 1 {
            m.sound_blaster = None;
            assert!(
                o.service_machine(60_000_000, &mut m)
                    .unwrap_err()
                    .0
                    .contains("no source")
            );
        } else {
            m.advance_sound_to(60_000_000).unwrap();
            m.sound_blaster
                .as_mut()
                .unwrap()
                .drain_frames(&mut [[0; 2]; 1]);
            let before = m.sound_blaster.as_ref().unwrap().status();
            assert!(
                o.service_machine(60_000_000, &mut m)
                    .unwrap_err()
                    .0
                    .contains("cursor mismatch")
            );
            assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
        }
        assert!(o.poll().is_err());
    }
}
#[test]
fn blocked_reader_times_out_without_source_advance_or_guest_progress() {
    let f = Fixture::new();
    let mut m = machine();
    let mut o = f.output(true);
    o.advance_machine(0, &mut m).unwrap();
    let start = Instant::now();
    let mut target = 0;
    let mut ready = true;
    let error = loop {
        if ready {
            target += 50_000_000;
        }
        let before = m.sound_blaster.as_ref().unwrap().status();
        let submitted = o.progress().submitted_frames;
        match o.service_machine(target, &mut m) {
            Err(e) => {
                assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
                assert_eq!(o.progress().submitted_frames, submitted);
                break e;
            }
            Ok(service) => {
                ready = service.is_ready();
                // Pending is not proof that the worker is stalled: it can still
                // write to the kernel socket and free capacity without a reader.
                // Assert no source effects only on this zero-admission call.
                if o.progress().submitted_frames == submitted {
                    assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
                }
            }
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(RECOVERY_RETRY_INTERVAL);
    };
    assert!(error.0.contains("no progress"), "{error}");
    assert_eq!(m.steps, 0);
    assert!(o.progress().buffered_frames <= MAX_BUFFER_FRAMES);
    let drop_start = Instant::now();
    drop(o);
    assert!(drop_start.elapsed() < Duration::from_secs(2));
}
