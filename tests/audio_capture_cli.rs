// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Authored MZ and BIN/CUE: execute the final lifted CLI, never an emulator or
//! audio device. The runtime-only generated crate must remain decoder-free.
mod common;
use common::FixtureCargo;
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const STEP_NS: u64 = 10_000;
const PRE_PLAY_ITERATIONS: u16 = 100;
const POST_PLAY_ITERATIONS: u16 = 1500;
const AUDIO_START: u32 = 18;
const AUDIO_SECTORS: u32 = 3;
const PACKET: usize = 0x80;

fn delay(code: &mut Vec<u8>, iterations: u16) {
    code.push(0xba); // MOV DX,iterations
    code.extend(iterations.to_le_bytes());
    // DEC DX; CMP DX,0; JNE back. Each iteration is three translated calls.
    code.extend([0x4a, 0x83, 0xfa, 0, 0x75, 0xfa]);
}

fn guest() -> Vec<u8> {
    let mut code = Vec::new();
    delay(&mut code, PRE_PLAY_ITERATIONS);
    code.extend([
        0x0e,
        0x1f, // PUSH CS; POP DS
        0x0e,
        0x07, // PUSH CS; POP ES
        0xbb,
        PACKET as u8,
        0, // MOV BX,packet
        0xb9,
        3,
        0, // MOV CX,3 (drive D:)
        0xb8,
        0x10,
        0x15, // MOV AX,1510h
        0xcd,
        0x2f, // MSCDEX device request at ES:BX
        0x81,
        0x7f,
        3,
        0,
        3, // CMP WORD [BX+3],0300h (done + busy)
        0x75,
        0, // JNE failure
    ]);
    let branch = code.len() - 1;
    delay(&mut code, POST_PLAY_ITERATIONS);
    code.extend([0xb8, 0, 0x4c, 0xcd, 0x21]); // DOS exit 0
    let failure = code.len();
    code.extend([0xb8, 1, 0x4c, 0xcd, 0x21]); // DOS exit 1
    code[branch] = i8::try_from(failure - branch - 1).unwrap() as u8;
    assert!(code.len() < PACKET);

    let mut mz = vec![0; 288];
    for (i, word) in [
        0x5a4d_u16, 288, 1, 0, 2, 0x20, 0xffff, 0, 0x300, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        mz[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    mz[32..32 + code.len()].copy_from_slice(&code);
    // RBIL 02597 PLAY 84h: mode +0Dh, DWORD HSG start +0Eh,
    // DWORD sector count +12h. Strict full request length, no legacy opt-in.
    let packet = &mut mz[32 + PACKET..32 + PACKET + 22];
    packet[0] = 22;
    packet[2] = 0x84;
    packet[14..18].copy_from_slice(&AUDIO_START.to_le_bytes());
    packet[18..22].copy_from_slice(&AUDIO_SECTORS.to_le_bytes());
    mz
}

fn successful(output: &Output) {
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    dir: PathBuf,
    exe: PathBuf,
    bin: PathBuf,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let scratch =
            std::env::var_os("TMPDIR").expect("TMPDIR required for generated CLI fixtures");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(scratch).join(format!(
            "pal-audio-cli-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("play.exe");
        fs::write(&exe, guest()).unwrap();
        let bin = dir.join("disc.bin");
        let mut raw = vec![0; AUDIO_START as usize * 2352];
        for sector in raw.chunks_exact_mut(2352) {
            sector[15] = 1;
        }
        raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
        for i in 0..AUDIO_SECTORS * 588 {
            // Signed, distinct channels including endian-sensitive first samples.
            let frame = match i {
                0 => [i16::MIN, i16::MAX],
                1 => [0x1234, -0x1234],
                _ => [i as i16 * 17 - 15000, 15000 - i as i16 * 17],
            };
            for sample in frame {
                raw.extend(sample.to_le_bytes());
            }
        }
        fs::write(&bin, raw).unwrap();
        fs::write(bin.with_extension("cue"), "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
        let generated = dir.join("generated");
        let lifted = Command::new(env!("CARGO_BIN_EXE_pal-wayland-lift"))
            .arg(&exe)
            .arg(&generated)
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
            .output()
            .unwrap();
        successful(&lifted);
        let target = dir.join("target");
        let mut build_cmd = Command::new(env!("CARGO"));
        build_cmd
            .args(["build", "--offline"])
            .current_dir(&generated);
        // A release test must exercise an optimized generated CLI as well.
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            build_cmd.arg("--release");
            "release"
        };
        let build = build_cmd.fixture_output_to(&target).unwrap();
        successful(&build);
        let tree = Command::new(env!("CARGO"))
            .args(["tree", "--offline", "-e", "features"])
            .current_dir(&generated)
            .fixture_output()
            .unwrap();
        successful(&tree);
        let tree = String::from_utf8(tree.stdout).unwrap();
        for excluded in ["iced-x86", "lazy_static", "winit", "softbuffer"] {
            assert!(!tree.contains(excluded), "unexpected {excluded}: {tree}");
        }
        Self {
            exe,
            bin,
            binary: target.join(profile).join("pal-wayland"),
            dir,
        }
    }
    fn run(&self, mount: bool, args: &[&OsStr]) -> Output {
        let mut cmd = Command::new(&self.binary);
        cmd.arg(&self.exe)
            .args([
                "--virtual-step-ns",
                &STEP_NS.to_string(),
                "--max-steps",
                "6000",
            ])
            .current_dir(&self.dir)
            // Even a broken window rejection must not reach the user's display.
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("XDG_RUNTIME_DIR");
        if mount {
            cmd.arg("--cd-bin").arg(&self.bin);
        }
        cmd.args(args).output().unwrap()
    }
    fn source_bytes(&self) -> Vec<u8> {
        let mut file = File::open(&self.bin).unwrap();
        file.seek(SeekFrom::Start(u64::from(AUDIO_START) * 2352))
            .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn frames(calls: u64) -> u64 {
    calls * STEP_NS * 44_100 / 1_000_000_000
}
fn wave(path: &Path, frames: u64) -> Vec<u8> {
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
fn directory_entries(path: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    entries
}

#[test]
fn lifted_guest_play_writes_exact_bin_pcm_with_pre_play_and_eof_silence() {
    let f = Fixture::new();
    let before = directory_entries(&f.dir);
    let control = f.run(true, &[]);
    successful(&control);
    assert!(String::from_utf8_lossy(&control.stdout).contains("guest_exit_code: 0"));
    assert!(!String::from_utf8_lossy(&control.stderr).contains("audio-capture"));
    assert_eq!(
        directory_entries(&f.dir),
        before,
        "capture must be explicit"
    );

    let path = f.dir.join("capture.wav");
    let output = f.run(
        true,
        &[
            OsStr::new("--audio-wav"),
            path.as_os_str(),
            OsStr::new("--audio-max-frames"),
            OsStr::new("3000"),
        ],
    );
    successful(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stop_reason: DOS-exit"), "{output:?}");
    assert!(stdout.contains("guest_exit_code: 0"), "{output:?}");
    // MOV DX + pre-delay + eight setup/PLAY instructions. Capture must drain
    // this entire preceding interval while stopped, BEFORE guest PLAY executes.
    let play_call = 1 + u64::from(PRE_PLAY_ITERATIONS) * 3 + 8;
    let total_calls = play_call + 2 + 1 + u64::from(POST_PLAY_ITERATIONS) * 3 + 2;
    let total_frames = frames(total_calls);
    let silence_frames = frames(play_call);
    let source = f.source_bytes();
    assert_eq!(source.len(), AUDIO_SECTORS as usize * 2352);
    assert!(source.iter().any(|&byte| byte != 0));
    let mut expected = vec![0; silence_frames as usize * 4];
    expected.extend(&source);
    assert!(
        total_frames as usize * 4 > expected.len(),
        "fixture must pass EOF"
    );
    expected.resize(total_frames as usize * 4, 0);
    let pcm = wave(&path, total_frames);
    assert_eq!(pcm, expected, "PLAY must not turn past silence into audio");
    assert!(
        source.len() / 4 > 1000,
        "exercise more than a capture chunk"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "audio-capture frames={total_frames} source_frames={}",
            source.len() / 4
        )),
        "{output:?}"
    );

    // Explicit finite budgets fail closed, preserving a finalized partial WAV.
    let limited = f.dir.join("limited.wav");
    let output = f.run(
        true,
        &[
            OsStr::new("--audio-wav"),
            limited.as_os_str(),
            OsStr::new("--audio-max-frames"),
            OsStr::new("1000"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("total frame budget exceeded"),
        "{output:?}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("guest_exit_code: 0"));
    assert_eq!(wave(&limited, 1000), expected[..4000]);
}

#[test]
fn lifted_cli_rejects_unsupported_backend_invalid_options_and_existing_output() {
    let f = Fixture::new();
    let path = f.dir.join("rejected.wav");
    let output_path = path.to_str().unwrap();
    for (mount, args, message) in [
        (
            true,
            vec!["--audio-wav", output_path],
            "requires explicit audio-max-frames",
        ),
        (
            true,
            vec!["--audio-max-frames", "3000"],
            "requires explicit audio-max-frames",
        ),
        (
            false,
            vec!["--audio-wav", output_path, "--audio-max-frames", "3000"],
            "audio-wav requires CD mount",
        ),
        (
            true,
            vec!["--audio-wav", output_path, "--audio-max-frames", "0"],
            "WAV max_frames must be",
        ),
        (
            true,
            vec![
                "--audio-wav",
                output_path,
                "--audio-max-frames",
                "1073741815",
            ],
            "WAV max_frames must be",
        ),
        (
            true,
            vec!["--audio-wav", output_path, "--audio-max-frames", "no"],
            "invalid audio-max-frames",
        ),
        (
            true,
            vec![
                "--audio-wav",
                output_path,
                "--audio-max-frames",
                "3000",
                "--window",
                "yes",
            ],
            "audio-wav capture is headless-only",
        ),
        (
            true,
            vec![
                "--window",
                "yes",
                "--audio-wav",
                output_path,
                "--audio-max-frames",
                "3000",
            ],
            "audio-wav capture is headless-only",
        ),
        (
            true,
            vec!["--window", "yes", "--audio-wav", output_path],
            "requires explicit audio-max-frames",
        ),
        (
            false,
            vec![
                "--window",
                "yes",
                "--audio-wav",
                output_path,
                "--audio-max-frames",
                "3000",
            ],
            "audio-wav requires CD mount",
        ),
        (true, vec!["--audio-wav"], "option missing value"),
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        let output = f.run(mount, &args);
        assert!(!output.status.success(), "accepted {args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{args:?}: {output:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "guest ran on rejected options: {output:?}"
        );
        assert!(!path.exists(), "rejected options created output: {args:?}");
    }
    let sentinel = b"existing user file must remain untouched";
    fs::write(&path, sentinel).unwrap();
    let output = f.run(
        true,
        &[
            OsStr::new("--audio-wav"),
            path.as_os_str(),
            OsStr::new("--audio-max-frames"),
            OsStr::new("3000"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("WAV capture create"),
        "{output:?}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(path).unwrap(), sentinel);
}
