// SPDX-License-Identifier: GPL-3.0-or-later
//! A private daemon with ONLY a dummy driver: no ALSA, session manager,
//! actual-user socket, or audible sink. Environment changes occur in subprocesses.
use pal_wayland::{
    audio_output::{AudioOutput, MAX_BUFFER_FRAMES},
    runtime::Machine,
};
use std::{
    fs,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    dir: PathBuf,
    daemon: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "pal-private-pw-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("private.conf"), r#"
context.properties = { core.daemon = true core.name = pal-private default.clock.rate = 44100 default.clock.quantum = 256 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-client-node }
 { name = libpipewire-module-adapter }
 { name = libpipewire-module-access args = { access.force = unrestricted } }
]
context.objects = [ { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver node.group = pipewire.dummy priority.driver = 20000 } } ]
"#).unwrap();
        fs::write(dir.join("client.conf"), r#"
context.properties = { log.level = 0 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [ { name = libpipewire-module-protocol-native } { name = libpipewire-module-client-node } { name = libpipewire-module-adapter } ]
stream.properties = { node.always-process = true node.group = pipewire.dummy node.autoconnect = false adapter.auto-port-config = { mode = dsp } }
"#).unwrap();
        Self { dir, daemon: None }
    }
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env("PIPEWIRE_RUNTIME_DIR", &self.dir)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("PIPEWIRE_REMOTE", "pal-private")
            .env("PIPEWIRE_CONFIG_DIR", &self.dir)
            .env("PIPEWIRE_CONFIG_PREFIX", "")
            .env("PIPEWIRE_CONFIG_NAME", "client.conf")
            .env("PAL_PRIVATE_PIPEWIRE_TEST", &self.dir)
            .env_remove("PIPEWIRE_PROPS")
            .env_remove("PIPEWIRE_NO_CONFIG")
            .env_remove("PIPEWIRE_CORE")
            .env_remove("PIPEWIRE_DEBUG")
            .stdin(Stdio::null());
        c
    }
    fn probe(&self, mode: &str) {
        let result = self
            .command(std::env::current_exe().unwrap())
            .args(["--exact", "native_probe", "--ignored", "--nocapture"])
            .env("PAL_PRIVATE_PIPEWIRE_MODE", mode)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fn start_daemon(&mut self) {
        self.daemon = Some(
            self.command("pipewire")
                .env("PIPEWIRE_CONFIG_NAME", "private.conf")
                .args(["-c", "private.conf"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("private PipeWire daemon required"),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.dir.join("pal-private").exists() {
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "private daemon exited"
            );
            assert!(Instant::now() < deadline, "private daemon socket missing");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.daemon {
            let _ = child.kill();
            let _ = child.wait();
        }
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
#[test]
fn isolated_native_readiness_fifo_finish_and_fail_closed() {
    let mut f = Fixture::new();
    f.probe("missing");
    // Socket connectivity alone must not pass constructor readiness.
    let fake = UnixListener::bind(f.dir.join("pal-private")).unwrap();
    f.probe("unresponsive");
    drop(fake);
    fs::remove_file(f.dir.join("pal-private")).unwrap();
    f.start_daemon();
    f.probe("stream");
    f.probe("drop");
    for mode in ["stalled", "finish-timeout"] {
        let marker = f.dir.join("stream-ready");
        let probe = f
            .command(std::env::current_exe().unwrap())
            .args(["--exact", "native_probe", "--ignored", "--nocapture"])
            .env("PAL_PRIVATE_PIPEWIRE_MODE", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let pid = f.daemon.as_ref().unwrap().id().to_string();
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid])
                .status()
                .unwrap()
                .success()
        );
        fs::write(f.dir.join("server-stopped"), b"stopped").unwrap();
        let result = probe.wait_with_output().unwrap();
        assert!(
            Command::new("kill")
                .args(["-CONT", &pid])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        fs::remove_file(marker).unwrap();
        fs::remove_file(f.dir.join("server-stopped")).unwrap();
    }
    let probe = f
        .command(std::env::current_exe().unwrap())
        .args(["--exact", "native_probe", "--ignored", "--nocapture"])
        .env("PAL_PRIVATE_PIPEWIRE_MODE", "server-loss")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !f.dir.join("stream-ready").exists() {
        assert!(
            Instant::now() < deadline,
            "server-loss probe readiness timed out"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let daemon = f.daemon.as_mut().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let result = probe.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
fn machine() -> Machine {
    let mut bytes = vec![0; 544];
    for (i, word) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    Machine::from_mz(&bytes, 0x1010).unwrap()
}
#[test]
#[ignore = "only the private-daemon parent may run this probe"]
fn native_probe() {
    let root = PathBuf::from(
        std::env::var_os("PAL_PRIVATE_PIPEWIRE_TEST").expect("isolated parent required"),
    );
    assert!(root.starts_with(PathBuf::from(std::env::var_os("TMPDIR").unwrap())));
    assert_eq!(
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").unwrap(),
        root.as_os_str()
    );
    let mode = std::env::var("PAL_PRIVATE_PIPEWIRE_MODE").unwrap();
    let start = Instant::now();
    if mode == "missing" || mode == "unresponsive" {
        let error = match AudioOutput::start_pipewire() {
            Err(e) => e,
            Ok(_) => panic!("non-server passed negotiation"),
        };
        assert!(start.elapsed() < Duration::from_secs(3), "{error}");
        eprintln!("mode={mode} bounded_error={error}");
        return;
    }
    let mut output = AudioOutput::start_pipewire().expect("private dummy stream negotiation");
    assert!(start.elapsed() < Duration::from_secs(3));
    if mode == "stalled" || mode == "finish-timeout" {
        fs::write(root.join("stream-ready"), b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !root.join("server-stopped").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut m = machine();
        m.sound_blaster = Some(pal_wayland::sound_blaster::SoundBlaster::default());
        m.retain_sfx_pcm = true;
        output.advance_machine(0, &mut m).unwrap();
        if mode == "finish-timeout" {
            output.advance_machine(50_000_000, &mut m).unwrap();
            let at = Instant::now();
            let error = output.finish(Duration::from_millis(20)).unwrap_err();
            assert!(error.0.contains("timed out"), "{error}");
            assert!(at.elapsed() < Duration::from_millis(500));
            return;
        }
        loop {
            let before = output.progress().submitted_frames;
            let hardware = m.sound_blaster.as_ref().unwrap().status();
            match output.service_machine(200_000_000, &mut m) {
                Err(error) => {
                    assert!(error.0.contains("no progress"), "{error}");
                    assert_eq!(output.progress().submitted_frames, before);
                    assert_eq!(m.sound_blaster.as_ref().unwrap().status(), hardware);
                    assert!(output.poll().is_err());
                    break;
                }
                Ok(_) => assert!(output.progress().buffered_frames <= MAX_BUFFER_FRAMES),
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(m.steps, 0);
        let at = Instant::now();
        drop(output);
        assert!(at.elapsed() < Duration::from_millis(500));
        return;
    }
    if mode == "server-loss" {
        fs::write(root.join("stream-ready"), b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(error) = output.poll() {
                assert!(output.poll().is_err(), "backend error must be sticky");
                assert_eq!(output.progress().submitted_frames, 0);
                eprintln!("server_loss={error}");
                break;
            }
            assert!(Instant::now() < deadline, "lost server went undetected");
            std::thread::sleep(Duration::from_millis(2));
        }
        let at = Instant::now();
        drop(output);
        assert!(at.elapsed() < Duration::from_millis(500));
        return;
    }
    let mut m = machine();
    m.sound_blaster = Some(pal_wayland::sound_blaster::SoundBlaster::default());
    m.retain_sfx_pcm = true;
    output.advance_machine(0, &mut m).unwrap();
    // No source/rendering occurs in callback. Authored mixer silence is still
    // accounted as authored frames, distinct from transport underrun silence.
    for target in [50_000_000, 100_000_000, 200_000_000] {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let service = output.service_machine(target, &mut m).unwrap();
            assert!(output.progress().buffered_frames <= MAX_BUFFER_FRAMES);
            if service.is_ready() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(output.progress().submitted_frames, 8820);
    assert_eq!(m.steps, 0);
    if mode == "drop" {
        let at = Instant::now();
        drop(output);
        assert!(at.elapsed() < Duration::from_millis(500));
        return;
    }
    let progress = output.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(progress.sink_written_frames, 8820);
    assert_eq!(progress.pipe_written_frames, 0);
    assert_eq!(progress.buffered_frames, 0);
    assert_eq!(progress.device_played_frames, None);
    eprintln!(
        "native_private_stream submitted={} sink_written={} helper_written={} buffered={} device_played={:?} underrun={}",
        progress.submitted_frames,
        progress.sink_written_frames,
        progress.pipe_written_frames,
        progress.buffered_frames,
        progress.device_played_frames,
        progress.underrun_silence_frames
    );
}
