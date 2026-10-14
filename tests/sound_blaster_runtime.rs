// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored ports, real IRQ frames and silent final-entry tests only.
mod common;
use common::FixtureCargo;
use pal_wayland::{
    audio_capture::Capture,
    audio_output::AudioOutput,
    cdrom::CdRom,
    runtime::Machine,
    semantics::{Flags, SegmentedAddress},
    sound_blaster::SoundBlaster,
};
use std::{fs, path::PathBuf, process::Command, time::Duration};
fn mz(code: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; 544];
    for (i, w) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    bytes[32..32 + code.len()].copy_from_slice(code);
    bytes
}
fn machine() -> Machine {
    let mut m = Machine::from_mz(&mz(&[]), 0x1010).unwrap();
    m.virtual_elapsed_ns = Some(0);
    m.sound_blaster = Some(SoundBlaster::default());
    m
}
fn out(m: &mut Machine, p: u16, v: u8) {
    m.port_out(p, v.into(), 8).unwrap();
}
fn dsp(m: &mut Machine, values: &[u8]) {
    for &v in values {
        out(m, 0x22c, v);
    }
}
fn dma(m: &mut Machine, count: u16, mode: u8) {
    for (p, v) in [
        (0xa, 5),
        (0xc, 0),
        (0xb, mode),
        (2, 0),
        (2, 0x50),
        (3, count as u8),
        (3, (count >> 8) as u8),
        (0x83, 0),
        (0xa, 1),
    ] {
        out(m, p, v);
    }
}
struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Self {
        let p = common::unique_dir(&format!("pal-sb-{label}"));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn machine_init_irq7_frame_ack_and_eoi_are_independent() {
    let mut m = machine();
    assert!(
        Machine::from_mz(&mz(&[]), 0x1010)
            .unwrap()
            .sound_blaster
            .is_none()
    );
    assert!(m.port_in(0x388, 8).is_err());
    m.opl_absent = true;
    for p in [0x388, 0x389] {
        out(&mut m, p, 0xff);
        assert_eq!(m.port_in(p, 8).unwrap(), 0);
    }
    assert!(m.port_in(0x387, 8).is_err());
    assert!(m.port_in(0x22a, 16).is_err());
    dsp(&mut m, &[0xd3]);
    out(&mut m, 0x226, 1);
    out(&mut m, 0x226, 0);
    assert_eq!(m.port_in(0x22e, 8).unwrap(), 0x80);
    assert_eq!(m.port_in(0x22a, 8).unwrap(), 0xaa);
    dsp(&mut m, &[0xe0, 0xaa]);
    assert_eq!(m.port_in(0x22a, 8).unwrap(), 0x55);
    dma(&mut m, 1, 0x45);
    dsp(&mut m, &[0xe2, 0x34, 0xe2, 0x12]);
    assert_eq!(m.memory.read_linear(0x5000, 2).unwrap(), [0x4c, 3]);
    dsp(&mut m, &[0xe4, 0xaa, 0xe8]);
    assert_eq!(m.port_in(0x22a, 8).unwrap(), 0xaa);
    dma(&mut m, 0, 0x49);
    dsp(&mut m, &[0x40, 0x64, 0x14, 0, 0]);
    m.virtual_elapsed_ns = Some(156_000);
    assert!(!m.poll_timer_irq().unwrap()); // delivery disabled, DMA still completes
    assert_eq!(
        m.sound_blaster
            .as_ref()
            .unwrap()
            .status()
            .completed_transfers,
        1
    );
    m.enable_irqs().unwrap();
    m.regs.flags.set(Flags::IF, false);
    assert!(!m.poll_timer_irq().unwrap());
    m.write16(SegmentedAddress::new(0, 0x3c), 0x123).unwrap();
    m.write16(SegmentedAddress::new(0, 0x3e), 0x6000).unwrap();
    m.regs.flags.set(Flags::IF, true);
    out(&mut m, 0x21, 0xff);
    assert!(!m.poll_timer_irq().unwrap());
    out(&mut m, 0x21, 0x7f);
    let before = m.regs;
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!((m.regs.cs, m.regs.ip), (0x6000, 0x123));
    assert_eq!(
        m.read16(SegmentedAddress::new(m.regs.ss, m.regs.sp))
            .unwrap(),
        before.ip
    );
    m.interrupt_return().unwrap();
    assert!(!m.poll_timer_irq().unwrap()); // ISR7 blocks reentry
    out(&mut m, 0x20, 0x20);
    assert!(m.sound_blaster.as_ref().unwrap().irq_pending()); // EOI is not DSP ACK
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!(m.port_in(0x22e, 8).unwrap(), 0);
    m.interrupt_return().unwrap();
    assert!(!m.poll_timer_irq().unwrap());
    out(&mut m, 0x20, 0x20);
    assert!(!m.poll_timer_irq().unwrap());
    assert_eq!(m.regs, before);
    m.virtual_elapsed_ns = Some(2_000_000_000);
    m.irq_enabled = false;
    assert!(!m.poll_timer_irq().unwrap());
    assert_eq!(m.sound_blaster.as_ref().unwrap().status().queued_frames, 0);
    assert_eq!(m.sfx_discarded_frames, 88200);
}
#[test]
fn irq7_remains_below_timer_and_keyboard() {
    let mut m = machine();
    dma(&mut m, 0, 0x49);
    dsp(&mut m, &[0x40, 156, 0x14, 0, 0]);
    m.enable_irqs().unwrap();
    for (vector, cs) in [(8, 0x6000), (9, 0x6100), (15, 0x6200)] {
        m.write16(SegmentedAddress::new(0, vector * 4), 0x100)
            .unwrap();
        m.write16(SegmentedAddress::new(0, vector * 4 + 2), cs)
            .unwrap();
    }
    m.key_event(0x1c, 13, true);
    m.virtual_elapsed_ns = Some(60_000_000);
    out(&mut m, 0x21, 0x7c);
    m.regs.flags.set(Flags::IF, true);
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!(m.regs.cs, 0x6000);
    m.regs.flags.set(Flags::IF, true);
    assert!(!m.poll_timer_irq().unwrap());
    out(&mut m, 0x20, 0x20);
    m.interrupt_return().unwrap();
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!(m.regs.cs, 0x6100);
    m.port_in(0x60, 8).unwrap();
    out(&mut m, 0x20, 0x20);
    m.interrupt_return().unwrap();
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!(m.regs.cs, 0x6200);
    m.port_in(0x22e, 8).unwrap();
    out(&mut m, 0x20, 0x20);
    m.interrupt_return().unwrap();
    assert!(!m.poll_timer_irq().unwrap());
}
fn mixed_machine(f: &Fixture) -> Machine {
    let mut raw = vec![0; 18 * 2352];
    for s in raw.chunks_exact_mut(2352) {
        s[15] = 1;
    }
    raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
    for _ in 0..588 {
        for v in [10000i16, -10000] {
            raw.extend(v.to_le_bytes());
        }
    }
    let bin = f.0.join("disc.bin");
    let cue = f.0.join("disc.cue");
    fs::write(&bin, raw).unwrap();
    fs::write(&cue,"FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
    let mut m = machine();
    m.retain_sfx_pcm = true;
    m.cdrom = Some(CdRom::mount_bin_cue(&bin, &cue).unwrap());
    let at = SegmentedAddress::new(0, 0x100);
    m.memory.write8(at, 22).unwrap();
    m.memory.write8(at.wrapping_add(2), 0x84).unwrap();
    m.memory
        .write_bytes(at.wrapping_add(14), &18u32.to_le_bytes())
        .unwrap();
    m.memory
        .write_bytes(at.wrapping_add(18), &1u32.to_le_bytes())
        .unwrap();
    m.cdrom
        .as_mut()
        .unwrap()
        .request(&mut m.memory, at)
        .unwrap();
    m.memory.write_linear(0x5000, &[255; 10]).unwrap();
    dma(&mut m, 9, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 9, 0]);
    m
}
fn ns(frames: u64) -> u64 {
    (frames * 1_000_000_000).div_ceil(44100)
}
#[test]
fn capture_mixes_clamps_and_preserves_source_counters() {
    let f = Fixture::new("capture");
    let mut m = mixed_machine(&f);
    let wav = f.0.join("mix.wav");
    let mut c = Capture::create(&wav, 100).unwrap();
    let before = m.sound_blaster.as_ref().unwrap().status();
    assert!(c.advance_machine(ns(101), &mut m).is_err());
    assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
    assert_eq!(
        m.cdrom
            .as_ref()
            .unwrap()
            .audio_player()
            .unwrap()
            .position_frames(),
        0
    );
    let p = c.advance_machine(ns(44), &mut m).unwrap();
    assert_eq!(
        (p.frames_written, p.source_frames, p.sfx_frames),
        (44, 44, 44)
    );
    assert_eq!(
        m.sound_blaster.as_ref().unwrap().status().consumed_bytes,
        10
    );
    c.finalize().unwrap();
    let bytes = fs::read(wav).unwrap();
    assert_eq!(
        &bytes[44..48],
        &[32767i16.to_le_bytes(), 22512i16.to_le_bytes()].concat()
    );
}
#[test]
fn realtime_mixed_transport_budget_precedes_source_drain() {
    let f = Fixture::new("output");
    let mut m = mixed_machine(&f);
    let raw = f.0.join("out.raw");
    let mut cmd = Command::new("python3");
    cmd.args([
        "-c",
        "import sys;open(sys.argv[1],'xb').write(sys.stdin.buffer.read())",
    ])
    .arg(&raw);
    let mut o = AudioOutput::spawn_transport(cmd).unwrap();
    o.advance_machine(0, &mut m).unwrap();
    let before = m.sound_blaster.as_ref().unwrap().status();
    assert!(o.advance_machine(ns(2206), &mut m).is_err());
    assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
    assert_eq!(
        m.cdrom
            .as_ref()
            .unwrap()
            .audio_player()
            .unwrap()
            .position_frames(),
        0
    );
    let p = o.advance_machine(ns(44), &mut m).unwrap();
    assert_eq!(
        (p.submitted_frames, p.source_frames, p.sfx_frames),
        (44, 44, 44)
    );
    let p = o.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(p.pipe_written_frames, 44);
    let bytes = fs::read(raw).unwrap();
    assert_eq!(
        &bytes[..4],
        &[32767i16.to_ne_bytes(), 22512i16.to_ne_bytes()].concat()
    );
}
#[cfg(feature = "analysis")]
#[test]
fn generated_cli_explicit_sb_flags_and_sfx_capture() {
    // Reset/probe, DMA single-cycle playback and a bounded delay: no decoder at runtime.
    let f = Fixture::new("cli");
    let mut code = Vec::new();
    fn emit_out(c: &mut Vec<u8>, p: u16, v: u8) {
        c.push(0xba);
        c.extend(p.to_le_bytes());
        c.extend([0xb0, v, 0xee]);
    }
    emit_out(&mut code, 0x388, 4);
    emit_out(&mut code, 0x389, 0x60);
    emit_out(&mut code, 0x226, 1);
    emit_out(&mut code, 0x226, 0);
    code.extend([0xba, 0x2a, 2, 0xec]);
    for (p, v) in [
        (0xa, 5),
        (0xc, 0),
        (0xb, 0x49),
        (2, 0),
        (2, 0x50),
        (3, 9),
        (3, 0),
        (0x83, 0),
        (0xa, 1),
    ] {
        emit_out(&mut code, p, v);
    }
    // DMA bytes are explicitly authored RAM initialized by guest instructions.
    code.extend([0x31, 0xc0, 0x8e, 0xc0, 0x26, 0xc6, 0x06, 0, 0x50, 255]);
    for v in [0xd1, 0x40, 156, 0x14, 9, 0] {
        emit_out(&mut code, 0x22c, v);
    }
    code.extend([0xb9, 0, 1, 0xe2, 0xfe, 0xb8, 0, 0x4c, 0xcd, 0x21]);
    let exe = f.0.join("sb.exe");
    fs::write(&exe, mz(&code)).unwrap();
    let generated = f.0.join("generated");
    let lift = Command::new(env!("CARGO_BIN_EXE_pal-wayland-lift"))
        .arg(&exe)
        .arg(&generated)
        .output()
        .unwrap();
    assert!(lift.status.success(), "{lift:?}");
    let target = f.0.join("target");
    let build = Command::new(env!("CARGO"))
        .args(["build", "--offline"])
        .current_dir(&generated)
        .fixture_output_to(&target)
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    let binary = target.join("debug/pal-wayland");
    for flag in [None, Some("no")] {
        let mut cmd = Command::new(&binary);
        cmd.arg(&exe).args(["--stop", "1010:0000"]);
        if let Some(v) = flag {
            cmd.args(["--sound-blaster", v]);
        }
        let o = cmd.output().unwrap();
        assert!(o.status.success(), "{o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("sound-blaster device=off"));
    }
    let invalid = Command::new(&binary)
        .arg(&exe)
        .args(["--sound-blaster", "invalid"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("sound-blaster must be yes or no"));
    let wav = f.0.join("cli.wav");
    let o = Command::new(&binary)
        .arg(&exe)
        .args([
            "--sound-blaster",
            "yes",
            "--opl",
            "absent",
            "--virtual-step-ns",
            "10000",
            "--audio-wav",
        ])
        .arg(&wav)
        .args(["--audio-max-frames", "1000", "--max-steps", "2000"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(stderr.contains("device=enabled base=0x220 irq=7 dma=1 opl=absent"));
    assert!(stderr.contains("source_frames=0 sfx_frames="));
    assert!(stderr.contains("consumed_bytes=10 completed_transfers=1"));
    let bytes = fs::read(wav).unwrap();
    assert!(
        bytes[44..]
            .chunks_exact(2)
            .any(|s| i16::from_le_bytes(s.try_into().unwrap()) != 0)
    );
    eprintln!("CLI verified: {}", stderr);
    // The same final entry also drives the real-time mixer, with no sound device.
    use std::os::unix::fs::PermissionsExt;
    let helper = f.0.join("record.py");
    let raw = f.0.join("record.raw");
    fs::write(&helper,"#!/usr/bin/python3\nimport sys,pathlib\npathlib.Path(__file__).with_name('record.raw').write_bytes(sys.stdin.buffer.read())\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let o = Command::new(&binary)
        .arg(&exe)
        .args([
            "--sound-blaster",
            "yes",
            "--audio-output",
            "pipewire",
            "--audio-helper",
        ])
        .arg(helper)
        .args(["--max-steps", "2000"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("audio-output finished") && stderr.contains("source_frames=0 sfx_frames=")
    );
    let recorded = fs::read(raw).unwrap();
    assert!(!recorded.is_empty());
    assert!(
        recorded
            .chunks_exact(2)
            .any(|s| i16::from_ne_bytes(s.try_into().unwrap()) != 0)
    );
    eprintln!("Realtime CLI verified: {stderr}");
}
