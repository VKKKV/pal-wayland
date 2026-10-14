// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored resources only. Exercise the generated ELF, never a real game,
//! compositor, audio helper or device. All writes remain under TMPDIR.
#![cfg(feature = "analysis")]
mod common;
use common::FixtureCargo;
use pal_wayland::native_startup::Startup;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
    bundle: PathBuf,
    binary: PathBuf,
    cwd: PathBuf,
    data: PathBuf,
}
static FIXTURE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
impl Fixture {
    fn new() -> Self {
        Self::with_window(false)
    }
    fn with_window(window: bool) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "pal-startup-{}-{n}-{}",
                std::process::id(),
                FIXTURE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        let bundle = root.join("bundle");
        let cwd = root.join("unrelated-cwd");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(bundle.join("assets/unpacked")).unwrap();
        fs::create_dir_all(bundle.join("assets/dos-cd")).unwrap();
        fs::create_dir_all(bundle.join("player-config")).unwrap();
        fs::write(
            bundle.join("player-config/SETUP.DAT"),
            b"authored configuration seed",
        )
        .unwrap();
        let mut mz = vec![0; 544];
        for (i, w) in [
            0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
        ]
        .into_iter()
        .enumerate()
        {
            mz[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
        }
        mz[32..37].copy_from_slice(&[0xb8, 0, 0x4c, 0xcd, 0x21]); // authored DOS exit 0
        let exe = bundle.join("assets/unpacked/PAL.raw.exe");
        fs::write(&exe, mz).unwrap();
        let mut raw = vec![0; 18 * 2352];
        for sector in raw.chunks_exact_mut(2352) {
            sector[15] = 1;
        }
        raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
        fs::write(bundle.join("assets/GC050.BIN"), raw).unwrap();
        fs::write(
            bundle.join("assets/GC050.cue"),
            "FILE \"GC050.BIN\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\n",
        )
        .unwrap();
        let project = bundle.join("generated");
        good(
            Command::new(env!("CARGO_BIN_EXE_pal-wayland-lift"))
                .arg(&exe)
                .arg(&project)
                .output()
                .unwrap(),
        );
        let target = root.join("target");
        let mut build = Command::new(env!("CARGO"));
        build.arg("build");
        if window {
            build.args(["--features", "window"]);
        }
        good(
            build
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_TARGET")
                .args(["--offline"])
                .current_dir(&project)
                .fixture_output_to(&target)
                .unwrap(),
        );
        let binary = bundle.join("bin/pal");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::copy(target.join("debug/pal-wayland"), &binary).unwrap();
        Self {
            data: root.join("xdg-data"),
            root,
            bundle,
            binary,
            cwd,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn command(&self) -> Command {
        let mut c = Command::new(&self.binary);
        c.current_dir(&self.cwd)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("HOME", self.root.join("home"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("PIPEWIRE_RUNTIME_DIR");
        c
    }
    fn saves(&self) -> PathBuf {
        self.data.join("pal-wayland/saves")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn good(o: Output) -> String {
    assert!(o.status.success(), "{o:?}");
    String::from_utf8(o.stdout).unwrap()
}
fn bad(o: Output, message: &str) {
    assert!(!o.status.success(), "unexpected success: {o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains(message),
        "{o:?}"
    );
}
#[test]
fn generated_window_and_headless_failures_do_not_initialize_saves() {
    let f = Fixture::with_window(true);
    // Compiled Wayland startup, not merely the feature-disabled early return.
    // Deliberately invalid helper proves no child is attempted before compositor.
    for args in [vec![], vec!["--audio-helper", "must-not-be-spawned"]] {
        bad(f.run(&args), "Wayland initialization failed");
        assert!(!f.data.exists());
    }
    let explicit_exe = f.bundle.join("assets/unpacked/PAL.raw.exe");
    bad(
        f.run(&[
            explicit_exe.to_str().unwrap(),
            "--audio-output",
            "pipewire",
            "--sound-blaster",
            "no",
        ]),
        "audio-output requires CD mount",
    );
    assert!(!f.data.exists());
    good(f.run(&["--print-config"]));
    assert!(!f.data.exists());
    // Shared CLI rejection precedes compositor, audio and persistent writes.
    for (args, message) in [
        (
            vec!["--window", "yes", "--state", "new.state"],
            "headless output",
        ),
        (
            vec!["--window", "yes", "--key-event", "0:1c:0d:down"],
            "headless-only",
        ),
        (
            vec!["--stop", "1010:0000", "--audio-helper", "must-not-spawn"],
            "--stop is headless-only",
        ),
        (
            vec![
                explicit_exe.to_str().unwrap(),
                "--window",
                "yes",
                "--stop",
                "1010:0000",
            ],
            "--stop is headless-only",
        ),
        (
            vec!["--window", "yes", "--virtual-step-ns", "1"],
            "wall time",
        ),
    ] {
        bad(f.run(&args), message);
        assert!(!f.data.exists());
        assert!(!f.cwd.join("new.state").exists());
    }
    // Headless selecting audio still requires successful backend startup before
    // copying the player seed. No source instruction is executed on failure.
    bad(
        f.run(&["--headless", "--audio-output", "pipewire"]),
        "PipeWire",
    );
    assert!(!f.data.exists());
    bad(
        f.run(&[
            "--headless",
            "--audio-output",
            "pipewire",
            "--audio-helper",
            "missing-exact-helper",
        ]),
        "PCM helper spawn",
    );
    assert!(!f.data.exists());
    // Existing data is never rolled back or modified to hide a failed backend.
    fs::create_dir_all(f.saves()).unwrap();
    fs::write(f.saves().join("SETUP.DAT"), b"existing config").unwrap();
    fs::write(f.saves().join("1.RPG"), b"existing save").unwrap();
    bad(f.run(&[]), "Wayland initialization failed");
    assert_eq!(
        fs::read(f.saves().join("SETUP.DAT")).unwrap(),
        b"existing config"
    );
    assert_eq!(fs::read(f.saves().join("1.RPG")).unwrap(), b"existing save");
    // Window-feature ELF preserves explicit/headless controls without GUI/audio.
    let exe = f.bundle.join("assets/unpacked/PAL.raw.exe");
    good(f.run(&[exe.to_str().unwrap()]));
    good(f.run(&["--headless"]));
    assert_eq!(
        fs::read(f.saves().join("SETUP.DAT")).unwrap(),
        b"existing config"
    );
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn generated_player_startup_is_relocatable_safe_and_preserves_explicit_cli() {
    let mut f = Fixture::new();
    // Exact default policy, exercised through production generated CLI dry-run.
    let out = good(f.run(&["--print-config"]));
    for expected in [
        "mode=player",
        "window=yes",
        "audio-output=pipewire",
        "sound-blaster=yes",
        "opl=absent",
        "irq=enabled",
        "ram-top=a000",
        "cd-header=legacy13",
        "max-steps=unlimited",
    ] {
        assert!(out.contains(expected), "{out}");
    }
    assert!(
        out.contains(
            f.bundle
                .join("assets/unpacked/PAL.raw.exe")
                .to_str()
                .unwrap()
        )
    );
    assert!(!f.data.exists());
    // Help wins anywhere, even over invalid syntax or an invalid environment.
    let help = good(
        f.command()
            .env("XDG_DATA_HOME", "relative")
            .args(["--bogus", "--help", "--saves"])
            .output()
            .unwrap(),
    );
    for flag in [
        "--resource-root",
        "--exe",
        "--headless",
        "--print-config",
        "--key-event",
        "--audio-helper",
        "--memory-input",
        "--clock-centiseconds",
        "--audio-max-frames",
        "--trace-after",
    ] {
        assert!(help.contains(flag));
    }
    assert!(!f.data.exists());
    for (args, message) in [
        (vec!["--assets"], "missing value"),
        (vec!["--saves", "unused", "--bogus", "x"], "unknown option"),
        (vec!["--sound-blaster", "maybe"], "must be"),
        (vec!["--max-steps", "-1"], "expected u64"),
        (
            vec!["--headless", "--ram-top", "9000"],
            "ram-top must be between",
        ),
        (
            vec!["--ram-top", "9eff", "--print-config"],
            "ram-top must be between",
        ),
        (vec!["--ram-top", "a001"], "ram-top must be between"),
        (vec!["--headless", "--window", "yes"], "conflicts"),
        (
            vec!["--stop", "1010:0000", "--print-config"],
            "--stop is headless-only",
        ),
        (
            vec!["--window", "no", "--audio-helper", "unused"],
            "requires",
        ),
        (vec!["--virtual-step-ns", "1"], "wall time"),
        (
            vec!["--window", "yes", "--state", "unused"],
            "headless output",
        ),
        (
            vec!["--window", "no", "--audio-wav", "unused"],
            "audio-max-frames",
        ),
    ] {
        bad(f.run(&args), message);
        assert!(!f.data.exists());
        assert!(!f.cwd.join("unused").exists());
    }
    bad(f.run(&[]), "--features window"); // zero args really selects window (not built here)
    assert!(!f.data.exists());
    bad(
        f.command()
            .env("XDG_DATA_HOME", "relative")
            .arg("--print-config")
            .output()
            .unwrap(),
        "XDG_DATA_HOME must be absolute",
    );
    bad(
        f.command()
            .env_remove("XDG_DATA_HOME")
            .env("HOME", "relative")
            .arg("--print-config")
            .output()
            .unwrap(),
        "HOME must be absolute",
    );
    let home = f.root.join("fallback-home");
    let out = good(
        f.command()
            .env_remove("XDG_DATA_HOME")
            .env("HOME", &home)
            .arg("--print-config")
            .output()
            .unwrap(),
    );
    assert!(
        out.contains(
            home.join(".local/share/pal-wayland/saves")
                .to_str()
                .unwrap()
        )
    );
    assert!(!home.exists());
    // HOME fallback ignores old namespaces, just like explicit XDG bases.
    let home_config = home.join(".config/pal-dos-native/config.toml"); // LEGACY
    let home_saves = home.join(".local/share/pal-dos-native/saves"); // LEGACY
    fs::create_dir_all(home_config.parent().unwrap()).unwrap();
    fs::write(&home_config, "version=1\n[audio]\nvolume=19").unwrap();
    fs::create_dir_all(&home_saves).unwrap();
    fs::write(home_saves.join("1.RPG"), b"home legacy save").unwrap();
    let out = good(
        f.command()
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home)
            .arg("--print-config")
            .output()
            .unwrap(),
    );
    assert!(
        out.contains(&format!(
            "player-config={}\n",
            home.join(".config/pal-wayland/config.toml").display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "saves={}\n",
            home.join(".local/share/pal-wayland/saves").display()
        )),
        "{out}"
    );
    assert!(out.contains("audio-volume=100\n"));
    assert!(out.contains("config-loaded=false\n"));
    assert_eq!(
        fs::read_to_string(&home_config).unwrap(),
        "version=1\n[audio]\nvolume=19"
    );
    assert_eq!(
        fs::read(home_saves.join("1.RPG")).unwrap(),
        b"home legacy save"
    );
    assert!(!home.join(".config/pal-wayland").exists());
    assert!(!home.join(".local/share/pal-wayland").exists());
    assert!(!home_saves.join("SETUP.DAT").exists());
    // Authored TOML only: controls/init run without resources or devices, CLI
    // priority and headless safety run through the real generated executable.
    let default_controls = good(f.run(&["--controls", "--no-config"]));
    for expected in [
        "KeyW -> ArrowUp (48, 00) [source=default]",
        "KeyA -> ArrowLeft (4b, 00) [source=default]",
        "KeyS -> ArrowDown (50, 00) [source=default]",
        "KeyD -> ArrowRight (4d, 00) [source=default]",
        "ArrowUp -> ArrowUp (48, 00) [source=default]",
    ] {
        assert!(default_controls.contains(expected), "{default_controls}");
    }
    let cfg = f.root.join("authored-config.toml");
    fs::write(&cfg, "version=1\n[video]\nrenderer='softbuffer'\n[audio]\nvolume=37\nmuted=true\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'\nEscape='None'\n").unwrap();
    let out = good(f.run(&["--controls", "--config", cfg.to_str().unwrap()]));
    assert!(
        out.contains("KeyW -> ArrowUp (48, 00) [source=user]"),
        "{out}"
    );
    assert!(
        out.contains("Escape -> None (disabled) [source=user]"),
        "{out}"
    );
    assert!(!out.contains("preset"), "{out}");
    let out = good(f.run(&[
        "--headless",
        "--print-config",
        "--config",
        cfg.to_str().unwrap(),
    ]));
    for expected in [
        "renderer=softbuffer\n",
        "audio-output=off\n",
        "audio-volume=37\n",
        "audio-muted=true\n",
    ] {
        assert!(out.contains(expected), "{out}");
    }
    let out = good(f.run(&[
        "--headless",
        "--print-config",
        "--config",
        cfg.to_str().unwrap(),
        "--renderer",
        "wgpu",
    ]));
    assert!(out.contains("renderer=wgpu\n"));
    assert!(!f.data.exists());
    bad(f.run(&["--controls", "--config", "missing.toml"]), "config");
    bad(
        f.run(&["--config", cfg.to_str().unwrap(), "--no-config"]),
        "conflicts",
    );
    let generated_cfg = f.root.join("generated-settings/config.toml");
    good(f.run(&["--init-config", "--config", generated_cfg.to_str().unwrap()]));
    let template = fs::read(&generated_cfg).unwrap();
    bad(
        f.run(&["--init-config", "--config", generated_cfg.to_str().unwrap()]),
        "refusing to overwrite",
    );
    assert_eq!(fs::read(&generated_cfg).unwrap(), template);
    let default_cfg = f.root.join("xdg-config/pal-wayland/config.toml");
    let legacy_cfg = f.root.join("xdg-config/pal-dos-native/config.toml"); // LEGACY lookup fixture
    fs::create_dir_all(legacy_cfg.parent().unwrap()).unwrap();
    let legacy_text = "version=1\n[audio]\nvolume=23\n";
    fs::write(&legacy_cfg, legacy_text).unwrap();
    for flags in [
        &["--print-config"][..],
        &["--controls"][..],
        &["--help"][..],
    ] {
        let out = good(f.run(flags));
        if flags[0] != "--help" {
            assert!(
                out.contains(&format!("player-config={}\n", default_cfg.display())),
                "{out}"
            );
        }
        assert!(!default_cfg.parent().unwrap().exists());
        assert!(!f.data.exists());
        assert_eq!(fs::read_to_string(&legacy_cfg).unwrap(), legacy_text);
    }
    good(f.run(&["--init-config"]));
    let default_template = fs::read(&default_cfg).unwrap();
    bad(f.run(&["--init-config"]), "refusing to overwrite");
    assert_eq!(fs::read(&default_cfg).unwrap(), default_template);
    assert_eq!(fs::read_to_string(&legacy_cfg).unwrap(), legacy_text);
    fs::write(&default_cfg, "version=1\n[video]\nrenderer='softbuffer'\n").unwrap();
    let out = good(f.run(&["--print-config"]));
    assert!(out.contains("renderer=softbuffer\n"));
    assert!(out.contains(&format!("player-config={}\n", default_cfg.display())));
    assert!(out.contains("audio-volume=100\n")); // New wins, no merging with legacy.
    assert_eq!(fs::read_to_string(&legacy_cfg).unwrap(), legacy_text);
    fs::write(&legacy_cfg, "invalid legacy]").unwrap();
    assert!(good(f.run(&["--print-config"])).contains("renderer=softbuffer\n"));
    fs::write(&legacy_cfg, legacy_text).unwrap();
    assert!(good(f.run(&["--no-config", "--print-config"])).contains("renderer=wgpu\n"));
    fs::write(&default_cfg, "invalid]").unwrap();
    bad(f.run(&["--print-config"]), "invalid TOML");
    let explicit_exe = f.bundle.join("assets/unpacked/PAL.raw.exe");
    good(f.run(&[explicit_exe.to_str().unwrap(), "--print-config"]));
    fs::remove_file(&default_cfg).unwrap();
    fs::create_dir(&default_cfg).unwrap();
    bad(f.run(&["--print-config"]), "invalid default");
    fs::remove_dir(&default_cfg).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for target in [&legacy_cfg, &f.root.join("absent-config")] {
            symlink(target, &default_cfg).unwrap();
            bad(f.run(&["--print-config"]), "symlink");
            good(f.run(&["--help"]));
            assert!(
                fs::symlink_metadata(&default_cfg)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            fs::remove_file(&default_cfg).unwrap();
        }
        let config_dir = default_cfg.parent().unwrap();
        fs::remove_dir(config_dir).unwrap();
        symlink(f.root.join("missing-config-parent"), config_dir).unwrap();
        bad(f.run(&["--print-config"]), "symlink");
        bad(f.run(&["--init-config"]), "symlink");
        assert!(!f.root.join("missing-config-parent").exists());
        fs::remove_file(config_dir).unwrap();
        fs::create_dir(config_dir).unwrap();
        fs::write(&default_cfg, "version=1").unwrap();
        fs::set_permissions(&default_cfg, fs::Permissions::from_mode(0o000)).unwrap();
        bad(f.run(&["--print-config"]), "Permission denied");
        fs::set_permissions(&default_cfg, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&default_cfg).unwrap();
        fs::set_permissions(config_dir, fs::Permissions::from_mode(0o000)).unwrap();
        bad(f.run(&["--print-config"]), "Permission denied");
        fs::set_permissions(config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(fs::read_to_string(&legacy_cfg).unwrap(), legacy_text);
    fs::write(&legacy_cfg, "invalid legacy]").unwrap();
    let out = good(f.run(&["--print-config"]));
    assert!(out.contains(&format!("player-config={}\n", default_cfg.display())));
    assert!(out.contains("audio-volume=100\n"));
    good(f.run(&["--init-config"]));
    assert_eq!(fs::read_to_string(&legacy_cfg).unwrap(), "invalid legacy]");
    fs::remove_file(&default_cfg).unwrap();
    fs::remove_file(&legacy_cfg).unwrap();
    fs::create_dir(&legacy_cfg).unwrap();
    good(f.run(&["--print-config"]));
    good(f.run(&["--init-config"]));
    assert!(legacy_cfg.is_dir());
    assert!(default_cfg.is_file());
    bad(f.run(&["--init-config"]), "refusing to overwrite");
    fs::remove_file(&default_cfg).unwrap();
    assert!(!f.data.exists());
    // Default saves ignore the old namespace without migrating or modifying it.
    let legacy_saves = f.data.join("pal-dos-native/saves"); // LEGACY lookup fixture
    fs::create_dir_all(&legacy_saves).unwrap();
    fs::write(legacy_saves.join("1.RPG"), b"legacy user save").unwrap();
    for flags in [&["--print-config"][..], &["--help"][..]] {
        let out = good(f.run(flags));
        if flags[0] == "--print-config" {
            assert!(
                out.contains(&format!("saves={}\n", f.saves().display())),
                "{out}"
            );
        }
        assert!(!f.saves().parent().unwrap().exists());
        assert_eq!(
            fs::read(legacy_saves.join("1.RPG")).unwrap(),
            b"legacy user save"
        );
        assert!(!legacy_saves.join("SETUP.DAT").exists());
    }
    fs::create_dir_all(f.saves()).unwrap();
    fs::write(f.saves().join("1.RPG"), b"new user save").unwrap();
    let out = good(f.run(&["--print-config"]));
    assert!(
        out.contains(&format!("saves={}\n", f.saves().display())),
        "{out}"
    );
    assert_eq!(fs::read(f.saves().join("1.RPG")).unwrap(), b"new user save");
    assert_eq!(
        fs::read(legacy_saves.join("1.RPG")).unwrap(),
        b"legacy user save"
    );
    assert!(!f.saves().join("SETUP.DAT").exists());
    fs::remove_dir_all(f.saves()).unwrap();
    fs::write(f.saves(), b"invalid saves").unwrap();
    bad(f.run(&["--print-config"]), "invalid default");
    fs::remove_file(f.saves()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for target in [&legacy_saves, &f.root.join("missing-saves")] {
            symlink(target, f.saves()).unwrap();
            bad(f.run(&["--print-config"]), "symlink");
            fs::remove_file(f.saves()).unwrap();
        }
        let data_dir = f.saves().parent().unwrap().to_path_buf();
        fs::remove_dir(&data_dir).unwrap();
        symlink(f.root.join("missing-data-parent"), &data_dir).unwrap();
        bad(f.run(&["--print-config"]), "symlink");
        fs::remove_file(&data_dir).unwrap();
        fs::create_dir(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o000)).unwrap();
        bad(f.run(&["--print-config"]), "Permission denied");
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(
        fs::read(legacy_saves.join("1.RPG")).unwrap(),
        b"legacy user save"
    );
    // An invalid old save path cannot block the absent pal-wayland default.
    fs::remove_dir_all(&legacy_saves).unwrap();
    fs::write(&legacy_saves, b"invalid old saves").unwrap();
    let out = good(f.run(&["--print-config"]));
    assert!(out.contains(&format!("saves={}\n", f.saves().display())));
    assert_eq!(fs::read(&legacy_saves).unwrap(), b"invalid old saves");
    assert!(!f.saves().exists());
    // Explicit CLI overrides bypass namespace lookup, including invalid defaults.
    fs::write(f.saves(), b"invalid default bypassed").unwrap();
    fs::write(&default_cfg, "invalid default]").unwrap();
    let out = good(f.run(&[
        "--print-config",
        "--saves",
        "custom-saves",
        "--config",
        cfg.to_str().unwrap(),
    ]));
    assert!(out.contains(&format!("player-config={}\n", cfg.display())));
    // Resolved options preserve the explicit CLI spelling; filesystem checks
    // separately resolve that path against the child's cwd.
    assert!(out.contains("saves=custom-saves\n"), "{out}");
    assert!(out.contains("audio-volume=37\n"));
    assert_eq!(
        fs::read_to_string(&default_cfg).unwrap(),
        "invalid default]"
    );
    good(f.run(&["--print-config", "--saves", "custom-saves", "--no-config"]));
    fs::remove_file(&default_cfg).unwrap();
    assert!(!f.cwd.join("custom-saves").exists());
    fs::remove_dir_all(&f.data).unwrap();
    // Positional original EXE has no PAL defaults, config seed, or writable mount.
    let exe = f.bundle.join("assets/unpacked/PAL.raw.exe");
    let exe_s = exe.to_str().unwrap();
    // This TOML contains live gain/keymap settings, but no backend selector.
    for text in [
        "version=1",
        "version=1\n[audio]\nvolume=37\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'",
    ] {
        fs::write(&cfg, text).unwrap();
        let out = good(f.run(&[exe_s, "--config", cfg.to_str().unwrap(), "--print-config"]));
        assert!(out.contains("audio-output=off\n"), "{out}");
        assert!(
            good(f.run(&[exe_s, "--config", cfg.to_str().unwrap()])).contains("guest_exit_code: 0")
        );
        let out = good(f.run(&["--config", cfg.to_str().unwrap(), "--print-config"]));
        assert!(out.contains("audio-output=pipewire\n"), "{out}");
    }
    fs::write(&cfg, "version=1\n[audio]\noutput='pipewire'").unwrap();
    bad(
        f.run(&[exe_s, "--config", cfg.to_str().unwrap()]),
        "audio-output requires CD mount",
    );
    let out = good(f.run(&[exe_s, "--print-config"]));
    for expected in [
        "mode=explicit-exe",
        "window=no",
        "audio-output=off",
        "sound-blaster=no",
        "irq=disabled",
        "ram-top=9f00",
        "cd-header=standard",
        "assets=none",
        "saves=none",
        "config-seed=none",
        "max-steps=1000000",
    ] {
        assert!(out.contains(expected), "{out}");
    }
    assert!(good(f.run(&[exe_s])).contains("guest_exit_code: 0"));
    bad(f.run(&[exe_s, "--max-steps", "0"]), "budget exceeded");
    bad(
        f.run(&["does-not-exist.exe", "--bogus", "x"]),
        "unknown option",
    );
    assert!(!f.data.exists());
    // Resource failures and corrupt resource contents precede save creation.
    let cue = f.bundle.join("assets/GC050.cue");
    fs::rename(&cue, cue.with_extension("held")).unwrap();
    bad(f.run(&["--headless"]), "missing CD cue");
    assert!(!f.data.exists());
    fs::rename(cue.with_extension("held"), &cue).unwrap();
    let image = f.bundle.join("assets/GC050.BIN");
    let original = fs::read(&image).unwrap();
    fs::write(&image, b"invalid authored CD").unwrap();
    assert!(!f.run(&["--headless"]).status.success());
    assert!(!f.data.exists());
    fs::write(&image, original).unwrap();
    // ISO override needs no sibling cue; immutable ISO mount is also preflighted.
    let iso = f.cwd.join("disc.iso");
    let mut iso_data = vec![0; 18 * 2048];
    iso_data[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
    fs::write(&iso, iso_data).unwrap();
    let out = good(f.run(&["--headless", "--cd-iso", "disc.iso", "--print-config"]));
    assert!(out.contains("cd-iso=disc.iso"));
    assert!(out.contains("cd-bin=none"));
    assert!(!f.data.exists());
    fs::write(f.cwd.join("existing.state"), b"keep").unwrap();
    bad(
        f.run(&["--headless", "--state", "existing.state"]),
        "new output file",
    );
    bad(
        f.run(&["--headless", "--state", "missing-parent/new.state"]),
        "parent must exist",
    );
    bad(
        f.run(&[
            "--headless",
            "--audio-wav",
            "new.wav",
            "--audio-max-frames",
            "0",
        ]),
        "WAV max_frames must be",
    );
    assert!(!f.data.exists());
    assert_eq!(fs::read(f.cwd.join("existing.state")).unwrap(), b"keep");
    // Relative overrides intentionally use cwd, including lower RAM top override.
    fs::create_dir(f.cwd.join("custom-assets")).unwrap();
    let out = good(f.run(&[
        "--headless",
        "--assets",
        "custom-assets",
        "--saves",
        "custom-saves",
        "--ram-top",
        "9f00",
        "--irq",
        "disabled",
        "--sound-blaster",
        "no",
        "--max-steps",
        "20",
        "--print-config",
    ]));
    for expected in [
        "window=no",
        "audio-output=off",
        "ram-top=9f00",
        "irq=disabled",
        "sound-blaster=no",
        "max-steps=20",
    ] {
        assert!(out.contains(expected), "{out}");
    }
    assert!(!f.cwd.join("custom-saves").exists());
    // No nested/overlapping saves, including resources' parents.
    for path in [
        f.bundle.join("assets"),
        f.bundle.join("assets/dos-cd/nested"),
        f.bundle.clone(),
    ] {
        bad(
            f.run(&["--headless", "--saves", path.to_str().unwrap()]),
            "disjoint",
        );
    }
    assert!(!f.bundle.join("assets/dos-cd/nested").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.bundle.join("assets/dos-cd"), f.cwd.join("save-link"))
            .unwrap();
        bad(f.run(&["--headless", "--saves", "save-link"]), "symlink");
        fs::create_dir(f.cwd.join("save-tree")).unwrap();
        std::os::unix::fs::symlink(&exe, f.cwd.join("save-tree/LINK.DAT")).unwrap();
        bad(f.run(&["--headless", "--saves", "save-tree"]), "symlink");
    }
    // A failed seed write must leave no final file or staging debris, so an
    // ordinary unrestricted retry can still initialize the full configuration.
    #[cfg(unix)]
    {
        let isolated = f.command();
        let mut limited = Command::new("python3");
        limited.current_dir(&f.cwd);
        for (key, value) in isolated.get_envs() {
            if let Some(value) = value {
                limited.env(key, value);
            } else {
                limited.env_remove(key);
            }
        }
        let output = limited.args(["-c", "import os,resource,signal,sys; signal.signal(signal.SIGXFSZ,signal.SIG_IGN); resource.setrlimit(resource.RLIMIT_FSIZE,(5,5)); os.execv(sys.argv[1],sys.argv[1:])"])
            .arg(&f.binary).arg("--headless").output().unwrap();
        bad(output, "seed SETUP.DAT");
        assert!(!f.saves().join("SETUP.DAT").exists());
        assert_eq!(fs::read_dir(f.saves()).unwrap().count(), 0);
    }
    // Safe headless retry executes the authored game; seed is copied unchanged.
    let out = good(f.run(&["--headless"]));
    assert!(out.contains("guest_exit_code: 0"));
    let setup = f.saves().join("SETUP.DAT");
    assert_eq!(fs::read(&setup).unwrap(), b"authored configuration seed");
    assert_eq!(fs::read_dir(f.saves()).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&setup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let out = good(f.run(&["--headless", "--stop", "1010:0000"]));
    assert!(out.contains("stop_reason: requested-stop"));
    assert!(out.contains("\"step\":0"));
    fs::write(&setup, b"existing user settings").unwrap();
    good(f.run(&["--window", "no"]));
    assert_eq!(fs::read(&setup).unwrap(), b"existing user settings");
    fs::rename(&setup, setup.with_file_name("setup.dat")).unwrap();
    good(f.run(&["--headless"]));
    assert!(!setup.exists());
    assert_eq!(
        fs::read(setup.with_file_name("setup.dat")).unwrap(),
        b"existing user settings"
    );
    // The copied layout can move as a unit; caller cwd does not become a root.
    let moved = f.root.join("relocated");
    fs::rename(&f.bundle, &moved).unwrap();
    f.bundle = moved;
    f.binary = f.bundle.join("bin/pal");
    let out = good(f.run(&["--print-config"]));
    assert!(out.contains(f.bundle.to_str().unwrap()));
    #[cfg(unix)]
    {
        let link = f.cwd.join("pal-link");
        std::os::unix::fs::symlink(&f.binary, &link).unwrap();
        let out = good(
            Command::new(link)
                .current_dir(&f.cwd)
                .env("XDG_DATA_HOME", &f.data)
                .env("XDG_CONFIG_HOME", f.root.join("xdg-config"))
                .arg("--print-config")
                .output()
                .unwrap(),
        );
        assert!(out.contains(f.bundle.to_str().unwrap()));
    }
    let stranded = f.root.join("stranded-pal");
    fs::copy(&f.binary, &stranded).unwrap();
    bad(
        Command::new(&stranded)
            .current_dir(&f.bundle)
            .env("XDG_DATA_HOME", &f.data)
            .env("XDG_CONFIG_HOME", f.root.join("xdg-config"))
            .arg("--print-config")
            .output()
            .unwrap(),
        "PAL resources not found",
    );
    // Explicit root works for a detached ELF; fully explicit paths also work.
    good(
        Command::new(&stranded)
            .env("XDG_DATA_HOME", &f.data)
            .env("XDG_CONFIG_HOME", f.root.join("xdg-config"))
            .args(["--print-config", "--resource-root"])
            .arg(&f.bundle)
            .output()
            .unwrap(),
    );
    good(
        Command::new(&stranded)
            .env("XDG_DATA_HOME", &f.data)
            .env("XDG_CONFIG_HOME", f.root.join("xdg-config"))
            .arg("--print-config")
            .arg("--exe")
            .arg(f.bundle.join("assets/unpacked/PAL.raw.exe"))
            .arg("--assets")
            .arg(f.bundle.join("assets/dos-cd"))
            .arg("--cd-bin")
            .arg(f.bundle.join("assets/GC050.BIN"))
            .output()
            .unwrap(),
    );
    // Unit entry takes injected paths for the true zero-argument policy.
    let startup = Startup::for_paths(&[], &f.binary, Some(&f.data), None).unwrap();
    assert!(startup.player);
    assert!(startup.config_text().contains("window=yes"));
    let explicit = Startup::for_paths(
        &strings(&["fixture.exe"]),
        Path::new("missing-elf"),
        None,
        None,
    )
    .unwrap();
    assert!(!explicit.player);
    assert!(explicit.options.is_empty());
}
