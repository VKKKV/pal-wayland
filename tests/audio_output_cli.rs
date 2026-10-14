// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Silent final-entry verification: authored guest PLAY, real wall clock,
//! trusted recorder helper, no PipeWire socket or desktop connection.
mod common;
use common::FixtureCargo;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const PACKET: usize = 0x100;
const AUDIO_START: u32 = 18;
const AUDIO_SECTORS: u32 = 8;

// Wait for N actual DOS centisecond edges (wrap-safe). No host sleep or injected
// virtual clock in the CLI. This makes the sample count independent of CPU speed.
fn delay(code: &mut Vec<u8>, edges: u16) {
    code.extend([0xbe]); // MOV SI,edges
    code.extend(edges.to_le_bytes());
    code.extend([0xb4, 0x2c, 0xcd, 0x21, 0x88, 0xd3]); // time; MOV BL,DL
    code.extend([
        0xb4, 0x2c, 0xcd, 0x21, 0x38, 0xda, 0x74, 0xf8, // wait DL != BL
        0x88, 0xd3, 0x4e, 0x75, 0xf3, // remember edge; DEC SI; JNE wait
    ]);
}
fn guest() -> Vec<u8> {
    let mut code = Vec::new();
    delay(&mut code, 2); // >=10 ms stopped, independently observable silence
    code.extend([
        0x0e, 0x1f, // PUSH CS; POP DS (packet status check)
        0x0e, 0x07, // PUSH CS; POP ES
        0xbb, 0, 1, // MOV BX,packet
        0xb9, 3, 0, // drive D:
        0xb8, 0x10, 0x15, 0xcd, 0x2f, // MSCDEX PLAY
        0x81, 0x7f, 3, 0, 3, 0x75, 0, // check done+busy
    ]);
    let branch = code.len() - 1;
    delay(&mut code, 3); // >=20 ms actual audio
    code.extend([0xb8, 0, 0x4c, 0xcd, 0x21]);
    let failure = code.len();
    code.extend([0xb8, 1, 0x4c, 0xcd, 0x21]);
    code[branch] = i8::try_from(failure - branch - 1).unwrap() as u8;
    assert!(code.len() < PACKET);
    let mut mz = vec![0; 544];
    for (i, word) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        mz[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    mz[32..32 + code.len()].copy_from_slice(&code);
    let packet = &mut mz[32 + PACKET..32 + PACKET + 22];
    packet[0] = 22;
    packet[2] = 0x84;
    packet[14..18].copy_from_slice(&AUDIO_START.to_le_bytes());
    packet[18..22].copy_from_slice(&AUDIO_SECTORS.to_le_bytes());
    mz
}
fn successful(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}
struct Fixture {
    dir: PathBuf,
    exe: PathBuf,
    bin: PathBuf,
    helper: PathBuf,
    generated: PathBuf,
    target: PathBuf,
    binary: PathBuf,
    source: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let dir = common::unique_dir("pal-pcm-cli");
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("play.exe");
        fs::write(&exe, guest()).unwrap();
        let bin = dir.join("disc.bin");
        let mut raw = vec![0; AUDIO_START as usize * 2352];
        for sector in raw.chunks_exact_mut(2352) {
            sector[15] = 1;
        }
        raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
        let mut source = Vec::new();
        for i in 0..AUDIO_SECTORS * 588 {
            for sample in [i as i16 + 1, -(i as i16) - 1] {
                raw.extend(sample.to_le_bytes());
                source.extend(sample.to_ne_bytes());
            }
        }
        fs::write(&bin, raw).unwrap();
        fs::write(bin.with_extension("cue"), "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
        // Shell metacharacters/spaces must remain part of this one filename.
        let helper = dir.join("silent helper; $exact.py");
        fs::write(&helper, "#!/usr/bin/python3\nimport os, pathlib, sys\nassert len(sys.argv) == 1\nassert {k: os.environ[k] for k in ('PAL_PCM_FORMAT', 'PAL_PCM_RATE', 'PAL_PCM_CHANNELS', 'PAL_PCM_INTERLEAVED')} == {'PAL_PCM_FORMAT':'s16ne', 'PAL_PCM_RATE':'44100', 'PAL_PCM_CHANNELS':'2', 'PAL_PCM_INTERLEAVED':'1'}\npath = pathlib.Path(__file__).with_name('pcm.raw')\nwith path.open('xb') as f:\n    while True:\n        data = sys.stdin.buffer.read(16384)\n        if not data: break\n        f.write(data)\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let generated = dir.join("generated");
        successful(
            &Command::new(env!("CARGO_BIN_EXE_pal-wayland-lift"))
                .arg(&exe)
                .arg(&generated)
                .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
                .output()
                .unwrap(),
        );
        let target = dir.join("target");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let f = Self {
            binary: target.join(profile).join("pal-wayland"),
            dir,
            exe,
            bin,
            helper,
            generated,
            target,
            source,
        };
        f.build(false);
        f
    }
    fn build(&self, window: bool) {
        let mut command = Command::new(env!("CARGO"));
        command
            .args(["build", "--offline"])
            .current_dir(&self.generated);
        if !cfg!(debug_assertions) {
            command.arg("--release");
        }
        if window {
            command.args(["--features", "window"]);
        }
        successful(&command.fixture_output_to(&self.target).unwrap());
    }
    fn run(&self, mount: bool, args: &[&str]) -> Output {
        let mut command = Command::new(&self.binary);
        command
            .arg(&self.exe)
            .args(["--max-steps", "1000000"])
            .args(args)
            .current_dir(&self.dir)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("PIPEWIRE_RUNTIME_DIR")
            .env("PIPEWIRE_REMOTE", self.dir.join("no-pipewire-socket"));
        if mount {
            command.arg("--cd-bin").arg(&self.bin);
        }
        command.output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn counter(stderr: &str, field: &str) -> usize {
    let line = stderr
        .lines()
        .find(|line| line.starts_with("audio-output finished "))
        .expect(stderr);
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{field}=")))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn generated_cli_silent_pcm_and_shared_validation_with_window_build() {
    let f = Fixture::new();
    let raw = f.dir.join("pcm.raw");
    let helper = f.helper.to_str().unwrap();
    // Default/off never launches the helper or requires a CD.
    for args in [
        vec!["--stop", "1010:0000"],
        vec!["--stop", "1010:0000", "--audio-output", "off"],
    ] {
        let output = f.run(false, &args);
        successful(&output);
        assert!(String::from_utf8_lossy(&output.stderr).contains("audio-output backend=off"));
        assert!(!raw.exists());
    }
    let rejected = f.dir.join("rejected.state");
    let state = rejected.to_str().unwrap();
    let wav = f.dir.join("rejected.wav");
    let wav_path = wav.to_str().unwrap();
    for window in ["no", "yes"] {
        for (mount, mut args, message) in [
            (
                true,
                vec!["--audio-helper", helper],
                "audio-helper requires",
            ),
            (
                true,
                vec!["--audio-output", "invalid"],
                "audio-output must be",
            ),
            (
                true,
                vec!["--audio-output", "pipewire", "--virtual-step-ns", "1000"],
                "requires wall time",
            ),
            (
                true,
                vec![
                    "--audio-output",
                    "pipewire",
                    "--audio-wav",
                    wav_path,
                    "--audio-max-frames",
                    "1000",
                ],
                "cannot combine with audio-wav",
            ),
            (
                false,
                vec!["--audio-output", "pipewire"],
                "audio-output requires CD mount",
            ),
        ] {
            args.extend(["--window", window, "--state", state]);
            let output = f.run(mount, &args);
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(message),
                "{output:?}"
            );
            assert!(output.stdout.is_empty());
            assert!(!rejected.exists() && !wav.exists() && !raw.exists());
        }
    }
    let missing = f.dir.join("missing helper");
    let output = f.run(
        true,
        &[
            "--audio-output",
            "pipewire",
            "--audio-helper",
            missing.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success() && output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("PCM helper spawn"),
        "{output:?}"
    );
    assert!(!raw.exists());
    // Budget failure still closes stdin and delivers the queued tail, but must
    // retain the original fault rather than publishing a successful guest exit.
    let output = f.run(
        true,
        &[
            "--audio-output",
            "pipewire",
            "--audio-helper",
            helper,
            "--max-steps",
            "1",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("static execution budget exceeded")
            && stderr.contains("audio-output finished"),
        "{output:?}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("guest_exit_code"));
    assert_eq!(
        fs::read(&raw).unwrap().len(),
        counter(&stderr, "pipe_written_frames") * 4
    );
    fs::remove_file(&raw).unwrap();
    let output = f.run(
        true,
        &["--audio-output", "pipewire", "--audio-helper", helper],
    );
    successful(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(String::from_utf8_lossy(&output.stdout).contains("guest_exit_code: 0"));
    assert!(
        stderr.contains("transport=explicit-helper") && stderr.contains("device_played=unknown")
    );
    let submitted = counter(&stderr, "submitted_frames");
    let consumed = counter(&stderr, "source_frames");
    assert_eq!(counter(&stderr, "pipe_written_frames"), submitted);
    assert_eq!(counter(&stderr, "buffered_frames"), 0);
    let bytes = fs::read(&raw).unwrap();
    assert_eq!(bytes.len(), submitted * 4);
    let start = bytes
        .chunks_exact(4)
        .position(|frame| frame != [0, 0, 0, 0])
        .expect("no source PCM");
    assert!(start > 0 && consumed > 0);
    assert!(consumed <= f.source.len() / 4);
    assert_eq!(
        &bytes[start * 4..(start + consumed) * 4],
        &f.source[..consumed * 4]
    );
    assert!(bytes[..start * 4].iter().all(|&byte| byte == 0));
    assert!(
        bytes[(start + consumed) * 4..]
            .iter()
            .all(|&byte| byte == 0)
    );
    eprintln!(
        "silent final CLI: submitted={submitted} source={consumed} pre_play_silence={start}; exact BIN PCM verified"
    );

    // A real generated window-feature build, followed only by isolated startup
    // failure: no compositor, no GUI/device. The lazy transport must not spawn.
    fs::remove_file(&raw).unwrap();
    f.build(true);
    let output = f.run(
        false,
        &[
            "--window",
            "yes",
            "--audio-output",
            "pipewire",
            "--audio-helper",
            helper,
            "--state",
            state,
        ],
    );
    assert!(!output.status.success() && output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("audio-output requires CD mount"));
    assert!(!rejected.exists() && !raw.exists());
    let output = f.run(
        true,
        &[
            "--window",
            "yes",
            "--audio-output",
            "pipewire",
            "--audio-helper",
            helper,
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Wayland"),
        "{output:?}"
    );
    assert!(output.stdout.is_empty() && !raw.exists());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("transport=explicit-helper"));
    eprintln!("generated window build passed; no-display initialization precedes helper spawn");
}
