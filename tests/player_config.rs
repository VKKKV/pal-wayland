// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored config/EXE/resources only, no home files/devices/original assets.
use pal_wayland::{
    native_startup::Startup,
    player_config::{self, Config, InitialConfig, MAX_CONFIG_BYTES},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "pal-config-test-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn file(&self) -> PathBuf {
        self.0.join("config.toml")
    }
    fn write(&self, text: &str) {
        fs::write(self.file(), text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}
fn parse(text: &str) -> Result<Config, String> {
    Config::parse(text.as_bytes(), Path::new("/authored/config.toml"))
}
#[test]
fn strict_schema_rejects_all_unknowns_duplicates_types_ranges_and_names() {
    for text in [
        "",
        "version=2",
        "version=1\nunknown=1",
        "version=1\nversion=1",
        "version=\"1\"",
        "version=1\n[video]\nrenderer=\"auto\"",
        "version=1\n[video]\nrenderer={softbuffer=[]}",
        "version=1\n[video]\nrender=\"wgpu\"",
        "version=1\n[audio]\noutput=\"alsa\"",
        "version=1\n[audio]\noutput={pipewire={}}",
        "version=1\n[audio]\nvolume=101",
        "version=1\n[audio]\nvolume=-1",
        "version=1\n[audio]\nvolume=1.5",
        "version=1\n[audio]\nvolume=nan",
        "version=1\n[audio]\nmuted=\"false\"",
        "version=1\n[audio]\nunknown=true",
        "version=1\n[keymap]\npreset=\"wasd\"",
        "version=1\n[keymap]\npreset={ wasd = {} }",
        "version=1\n[keymap]\nKeyW=\"ArrowUp\"\nKeyW=\"ArrowDown\"",
        "version=1\n[keymap]\nunknown=\"Enter\"",
        "version=1\n[input]\npreset=\"wasd\"",
        "version=1\n[keymap]\nunknown=true",
        "version=1\n[paths]\nsaves=\"\"",
        "version=1\n[paths]\nunknown=\"x\"",
        "version=1\n[reload]\ninterval_ms=99",
        "version=1\n[reload]\ninterval_ms=5001",
        "version=1\n[reload]\nenabled=1",
        "version=1\n[reload]\nunknown=true",
        "version=1\n[keymap]\nUnknown=\"Enter\"",
        "version=1\n[keymap]\nKeyW=\"Unknown\"",
        "version=1\n[keymap]\nKeyW=1",
        "version=1\n[keymap]\nKeyW=\"none\"",
    ] {
        assert!(parse(text).is_err(), "accepted {text:?}");
    }
    assert!(Config::parse(&[0xff], Path::new("x")).is_err());
    assert!(Config::parse(&vec![b' '; MAX_CONFIG_BYTES + 1], Path::new("x")).is_err());
    let mut bounded = b"version=1\n#".to_vec();
    bounded.resize(MAX_CONFIG_BYTES, b'a');
    assert!(Config::parse(&bounded, Path::new("x")).is_ok());
    let deep = format!("version=1\nunknown={}0{}", "[".repeat(200), "]".repeat(200));
    assert!(parse(&deep).is_err());
}
#[test]
fn default_wasd_bindings_are_exact_and_custom_disabled_supported_names_work() {
    let original = Config::default().resolved_keymap();
    let expected = [
        ("Escape", (1, 27)),
        ("Enter", (0x1c, 13)),
        ("Space", (0x39, 32)),
        ("ArrowUp", (0x48, 0)),
        ("ArrowDown", (0x50, 0)),
        ("ArrowLeft", (0x4b, 0)),
        ("ArrowRight", (0x4d, 0)),
        ("KeyW", (0x48, 0)),
        ("KeyA", (0x4b, 0)),
        ("KeyS", (0x50, 0)),
        ("KeyD", (0x4d, 0)),
        ("KeyE", (0x12, b'e')),
        ("KeyQ", (0x10, b'q')),
        ("ControlLeft", (0x1d, 0)),
        ("ShiftLeft", (0x2a, 0)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(original, expected);
    let example = parse(player_config::EXAMPLE).unwrap();
    assert_eq!(example.keymap.len(), original.len());
    assert_eq!(example.resolved_keymap(), original);
    assert_eq!(Config::default().audio.output, None);
    for text in ["version=1", "version=1\n[keymap]\n"] {
        let config = parse(text).unwrap();
        assert!(config.keymap.is_empty());
        assert_eq!(config.resolved_keymap(), original);
    }
    let original_text = Config::default().controls_text();
    assert!(
        original_text
            .contains("input-basis=physical (QWERTY-position names, not layout characters)")
    );
    assert!(original_text.contains("output-target=DOS key (scan, ASCII hexadecimal)"));
    assert!(original_text.contains("precedence: default < user [keymap]"));
    assert!(!original_text.contains("preset"));
    assert!(original_text.contains("KeyW -> ArrowUp (48, 00) [source=default]"));
    let config = parse(
        "version=1\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'\nKeyQ='Enter'\nEscape='None'\nKeyZ='Space'",
    )
    .unwrap();
    let keys = config.resolved_keymap();
    assert_eq!(keys["KeyW"], (0x48, 0));
    assert_eq!(keys["KeyA"], (0x4b, 0));
    assert_eq!(keys["KeyS"], (0x50, 0));
    assert_eq!(keys["KeyD"], (0x4d, 0));
    assert_eq!(keys["KeyQ"], (0x1c, 13));
    assert_eq!(keys["KeyZ"], (0x39, 32));
    assert!(!keys.contains_key("Escape"));
    let text = config.controls_text();
    for line in [
        "KeyW -> ArrowUp (48, 00) [source=user]",
        "ArrowUp -> ArrowUp (48, 00) [source=default]",
        "KeyA -> ArrowLeft (4b, 00) [source=user]",
        "KeyS -> ArrowDown (50, 00) [source=user]",
        "KeyD -> ArrowRight (4d, 00) [source=user]",
        "KeyQ -> Enter (1c, 0d) [source=user]",
        "KeyZ -> Space (39, 20) [source=user]",
        "Escape -> None (disabled) [source=user]",
    ] {
        assert!(text.contains(line), "{text}");
    }
    assert!(!text.contains("preset"));
    let mut identity = config.clone();
    identity.keymap.insert("KeyD".into(), "KeyD".into());
    assert_eq!(identity.resolved_keymap()["KeyD"], (0x20, b'd'));
    assert!(
        identity
            .controls_text()
            .contains("KeyD -> KeyD (20, 64) [source=user]")
    );
    // Custom physical WARS is explicit; arrows remain available.
    let wars = parse("version=1\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyR='ArrowDown'\nKeyS='ArrowRight'").unwrap();
    let keys = wars.resolved_keymap();
    for (host, target) in [
        ("KeyW", "ArrowUp"),
        ("KeyA", "ArrowLeft"),
        ("KeyR", "ArrowDown"),
        ("KeyS", "ArrowRight"),
    ] {
        assert_eq!(keys[host], player_config::supported_keys()[target]);
        assert_eq!(keys[target], player_config::supported_keys()[target]);
    }
    assert_eq!(keys["KeyD"], (0x4d, 0));
    for name in player_config::supported_keys().keys() {
        assert!(parse(&format!("version=1\n[keymap]\n{name}='{name}'")).is_ok());
    }
}
#[test]
fn default_paths_and_relative_values_are_not_expanded_or_cwd_based() {
    assert_eq!(
        player_config::default_path(Some(Path::new("/xdg")), Some(Path::new("/home"))).unwrap(),
        Path::new("/xdg/pal-wayland/config.toml")
    );
    assert_eq!(
        player_config::default_path(Some(Path::new("")), Some(Path::new("/home"))).unwrap(),
        Path::new("/home/.config/pal-wayland/config.toml")
    );
    assert!(
        player_config::default_path(Some(Path::new("relative")), Some(Path::new("/home"))).is_err()
    );
    assert!(player_config::default_path(None, Some(Path::new("relative"))).is_err());
    assert!(player_config::default_path(None, None).is_err());
    let config = parse("version=1\n[paths]\nresource_root='layout'\nsaves='~/literal'").unwrap();
    assert_eq!(
        config.paths.resource_root.unwrap(),
        Path::new("/authored/layout")
    );
    assert_eq!(
        config.paths.saves.unwrap(),
        Path::new("/authored/~/literal")
    );
}
#[test]
fn bounded_regular_read_and_safe_private_nonoverwriting_init() {
    let f = Fixture::new();
    let initial = InitialConfig::load(Some(&f.file()), false).unwrap();
    assert!(!initial.loaded);
    assert!(initial.path.is_some());
    assert!(InitialConfig::load(Some(&f.file()), true).is_err());
    player_config::initialize(&f.file()).unwrap();
    let template = InitialConfig::load(Some(&f.file()), true).unwrap().document;
    assert_eq!(
        template.resolved_keymap(),
        parse("version=1\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'").unwrap().resolved_keymap()
    );
    assert_eq!(
        template.audio.output,
        Some(player_config::AudioOutput::Pipewire)
    );
    assert_eq!(template.audio.volume, Config::default().audio.volume);
    assert_eq!(template.audio.muted, Config::default().audio.muted);
    let original = fs::read(f.file()).unwrap();
    assert!(player_config::initialize(&f.file()).is_err());
    assert_eq!(fs::read(f.file()).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            fs::metadata(f.file()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = f.0.join("link");
        symlink(f.file(), &link).unwrap();
        assert!(InitialConfig::load(Some(&link), true).is_err());
        assert!(player_config::initialize(&link).is_err());
        let parent_link = f.0.join("parent-link");
        symlink(&f.0, &parent_link).unwrap();
        assert!(InitialConfig::load(Some(&parent_link.join("config.toml")), true).is_ok());
        let dangling = f.0.join("dangling");
        symlink(f.0.join("absent"), &dangling).unwrap();
        assert!(player_config::initialize(&dangling).is_err());
        assert!(!f.0.join("absent").exists());
        let fifo = f.0.join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(InitialConfig::load(Some(&fifo), true).is_err());
    }
    fs::write(f.file(), vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
    assert!(
        InitialConfig::load(Some(&f.file()), true)
            .unwrap_err()
            .contains("64 KiB")
    );
    assert!(InitialConfig::load(Some(&f.0), true).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn initialization_write_failure_leaves_no_target_and_can_retry() {
    const CHILD_PATH: &str = "PAL_CONFIG_INIT_FAILURE_TEST_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let error = player_config::initialize(Path::new(&path)).unwrap_err();
        assert!(error.contains("write config"), "{error}");
        return;
    }
    let f = Fixture::new();
    assert!(player_config::EXAMPLE.len() > 128);
    // Ignore SIGXFSZ and limit only the exec'd test child, never this runner.
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg("import os, resource, signal, sys; signal.signal(signal.SIGXFSZ, signal.SIG_IGN); resource.setrlimit(resource.RLIMIT_FSIZE, (128, resource.getrlimit(resource.RLIMIT_FSIZE)[1])); os.execv(sys.argv[1], [sys.argv[1], '--exact', 'initialization_write_failure_leaves_no_target_and_can_retry', '--nocapture'])")
        .arg(std::env::current_exe().unwrap())
        .env(CHILD_PATH, f.file())
        .output()
        .expect("python3 is required for the child-only file-size limit");
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !f.file().exists(),
        "failed initialization published a partial file"
    );
    assert_eq!(
        fs::read_dir(&f.0).unwrap().count(),
        0,
        "staging file leaked"
    );
    player_config::initialize(&f.file()).unwrap();
    assert_eq!(
        fs::read(f.file()).unwrap(),
        player_config::EXAMPLE.as_bytes()
    );
    assert!(player_config::initialize(&f.file()).is_err());
    assert_eq!(
        fs::read(f.file()).unwrap(),
        player_config::EXAMPLE.as_bytes()
    );
    assert_eq!(
        fs::read_dir(&f.0).unwrap().count(),
        1,
        "staging file leaked"
    );
}
#[test]
fn reload_keeps_valid_state_deduplicates_errors_and_detects_same_size_atomic_replacement() {
    let f = Fixture::new();
    f.write("version=1\n[audio]\nvolume=90\n");
    let initial = InitialConfig::load(Some(&f.file()), true).unwrap();
    let now = Instant::now();
    let mut reload = initial.monitor(now);
    f.write("version=1\n[audio]\nvolume=80\n");
    assert!(reload.poll(now + Duration::from_millis(499)).is_none());
    let candidate = reload.poll(now + Duration::from_millis(500)).unwrap();
    assert_eq!(candidate.document.audio.volume, 80);
    assert!(candidate.restart_fields.is_empty());
    f.write("invalid]");
    assert!(reload.poll(now + Duration::from_millis(1000)).is_none());
    assert!(reload.take_error().is_some());
    assert_eq!(reload.current().audio.volume, 80);
    assert!(reload.poll(now + Duration::from_millis(1500)).is_none());
    assert!(reload.take_error().is_none());
    fs::remove_file(f.file()).unwrap();
    assert!(reload.poll(now + Duration::from_millis(2000)).is_none());
    assert!(reload.take_error().is_some());
    assert_eq!(reload.current().audio.volume, 80);
    assert!(reload.poll(now + Duration::from_millis(2500)).is_none());
    assert!(reload.take_error().is_none());
    fs::write(f.0.join("replace"), "version=1\n[audio]\nvolume=70\n").unwrap();
    fs::rename(f.0.join("replace"), f.file()).unwrap();
    assert_eq!(
        reload
            .poll(now + Duration::from_millis(3000))
            .unwrap()
            .document
            .audio
            .volume,
        70
    );
    f.write("version=1\n[video]\nrenderer='softbuffer'\n[audio]\noutput='off'\nvolume=60\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'\n[reload]\ninterval_ms=100\n");
    let candidate = reload.poll(now + Duration::from_millis(3500)).unwrap();
    assert_eq!(candidate.restart_fields, ["video.renderer", "audio.output"]);
    assert_eq!(candidate.document.keymap["KeyW"], "ArrowUp");
    assert_eq!(candidate.document.resolved_keymap()["KeyW"], (0x48, 0));
    f.write("version=1\n[reload]\nenabled=false\ninterval_ms=100\n");
    assert!(reload.poll(now + Duration::from_millis(3599)).is_none());
    assert!(reload.poll(now + Duration::from_millis(3600)).is_some());
    f.write("version=1");
    assert!(reload.poll(now + Duration::from_secs(20)).is_none());
    assert!(!reload.current().reload.enabled);
}
#[test]
fn identical_recovery_resets_error_deduplication() {
    let f = Fixture::new();
    f.write("version=1");
    let now = Instant::now();
    let mut reload = InitialConfig::load(Some(&f.file()), true)
        .unwrap()
        .monitor(now);
    fs::remove_file(f.file()).unwrap();
    assert!(reload.poll(now + Duration::from_millis(500)).is_none());
    assert!(reload.take_error().is_some());
    f.write("version=1");
    assert!(reload.poll(now + Duration::from_millis(1000)).is_none());
    assert!(reload.take_error().is_none());
    fs::remove_file(f.file()).unwrap();
    assert!(reload.poll(now + Duration::from_millis(1500)).is_none());
    assert!(reload.take_error().is_some());
}
#[test]
fn missing_default_can_appear_and_no_config_never_polls() {
    let f = Fixture::new();
    let now = Instant::now();
    let initial = InitialConfig::load(Some(&f.file()), false).unwrap();
    let mut reload = initial.monitor(now);
    f.write("version=1\n[audio]\nvolume=25");
    assert_eq!(
        reload
            .poll(now + Duration::from_millis(500))
            .unwrap()
            .document
            .audio
            .volume,
        25
    );
    let mut disabled = InitialConfig::load(None, false).unwrap().monitor(now);
    assert!(disabled.poll(now + Duration::from_secs(10)).is_none());
    assert!(disabled.take_error().is_none());

    // Default namespace selection is performed once, not on every reload poll.
    let primary = f.0.join("pal-wayland/config.toml");
    let legacy = f.0.join("pal-dos-native/config.toml"); // LEGACY lookup fixture
    let lookup = || player_config::lookup_path(Some(&f.0), None).unwrap();
    assert_eq!(lookup(), primary);
    assert!(!primary.parent().unwrap().exists());
    let mut missing = InitialConfig::load_default(&lookup()).unwrap().monitor(now);
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "version=1\n[audio]\nvolume=30").unwrap();
    assert!(missing.poll(now + Duration::from_millis(500)).is_none());
    assert!(
        missing
            .take_error()
            .unwrap()
            .contains(primary.to_str().unwrap())
    );
    assert_eq!(lookup(), primary);
    let initial = InitialConfig::load_default(&lookup()).unwrap();
    assert!(!initial.loaded);
    assert!(initial.document.keymap.is_empty());
    assert_eq!(
        initial.document.resolved_keymap(),
        Config::default().resolved_keymap()
    );
    assert_eq!(
        player_config::initialize_default(Some(&f.0), None).unwrap(),
        primary
    );
    let template = fs::read(&primary).unwrap();
    assert!(player_config::initialize_default(Some(&f.0), None).is_err());
    assert_eq!(fs::read(&primary).unwrap(), template);
    assert_eq!(
        fs::read_to_string(&legacy).unwrap(),
        "version=1\n[audio]\nvolume=30"
    );
    fs::write(&primary, "version=1\n[audio]\nvolume=90").unwrap();
    let mut selected = InitialConfig::load_default(&lookup()).unwrap().monitor(now);
    fs::write(&legacy, "invalid legacy]").unwrap();
    assert_eq!(lookup(), primary);
    assert!(selected.poll(now + Duration::from_millis(500)).is_none());
    assert!(selected.take_error().is_none());
    fs::write(&primary, "version=1\n[audio]\nvolume=91").unwrap();
    assert_eq!(
        selected
            .poll(now + Duration::from_millis(1000))
            .unwrap()
            .document
            .audio
            .volume,
        91
    );
    fs::remove_file(&primary).unwrap();
    assert!(selected.poll(now + Duration::from_millis(1500)).is_none());
    assert!(
        selected
            .take_error()
            .unwrap()
            .contains(primary.to_str().unwrap())
    );
    assert_eq!(selected.current().audio.volume, 91);
    assert_eq!(lookup(), primary);
    assert!(!InitialConfig::load_default(&lookup()).unwrap().loaded);
    player_config::initialize_default(Some(&f.0), None).unwrap();
    assert_eq!(fs::read_to_string(&legacy).unwrap(), "invalid legacy]");
    fs::remove_file(&legacy).unwrap();
    fs::create_dir(&legacy).unwrap();
    fs::remove_file(&primary).unwrap();
    assert_eq!(lookup(), primary);
    player_config::initialize_default(Some(&f.0), None).unwrap();
    assert!(legacy.is_dir());
    fs::remove_dir(&legacy).unwrap();
    fs::remove_file(&primary).unwrap();
    #[cfg(unix)]
    {
        // A namespace appearing via symlink must not bypass default startup
        // policy during reload. Explicit --config parent links stay allowed.
        use std::os::unix::fs::symlink;
        fs::remove_dir(legacy.parent().unwrap()).unwrap();
        let old_target = f.0.join("absent-old-config-parent");
        symlink(&old_target, legacy.parent().unwrap()).unwrap();
        assert_eq!(lookup(), primary);
        player_config::initialize_default(Some(&f.0), None).unwrap();
        assert!(!old_target.exists());
        assert!(
            fs::symlink_metadata(legacy.parent().unwrap())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&primary).unwrap();
        let actual = f.0.join("actual-config");
        fs::create_dir(&actual).unwrap();
        fs::write(actual.join("config.toml"), "version=1\n[audio]\nvolume=99").unwrap();
        fs::remove_dir(primary.parent().unwrap()).unwrap();
        symlink(&actual, primary.parent().unwrap()).unwrap();
        assert!(player_config::lookup_path(Some(&f.0), None).is_err());
        assert!(player_config::initialize_default(Some(&f.0), None).is_err());
        assert!(selected.poll(now + Duration::from_millis(2000)).is_none());
        assert!(selected.take_error().unwrap().contains("symlink"));
        assert_eq!(selected.current().audio.volume, 91);
        let mut explicit = InitialConfig::load(Some(&primary), true)
            .unwrap()
            .monitor(now);
        fs::write(actual.join("config.toml"), "version=1\n[audio]\nvolume=98").unwrap();
        assert_eq!(
            explicit
                .poll(now + Duration::from_millis(500))
                .unwrap()
                .document
                .audio
                .volume,
            98
        );
        fs::remove_file(primary.parent().unwrap()).unwrap();
    }
    player_config::initialize_default(Some(&f.0), None).unwrap();
    assert_eq!(lookup(), primary);
}
#[test]
fn cli_priority_explicit_exe_no_autoload_and_resource_free_commands() {
    let f = Fixture::new();
    f.write("version=1\n[video]\nrenderer='softbuffer'\n[audio]\nvolume=42\nmuted=true\noutput='off'\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'");
    let exe = f.0.join("authored.exe");
    fs::write(&exe, b"parser-only").unwrap();
    let plain = Startup::for_paths(
        &args(&[exe.to_str().unwrap()]),
        Path::new("missing"),
        None,
        None,
    )
    .unwrap();
    assert!(plain.config.path.is_none());
    assert!(plain.options.is_empty());
    // Unrelated TOML must not opt diagnostic EXEs into a player audio backend.
    for text in [
        "version=1",
        "version=1\n[audio]\nvolume=42\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'",
    ] {
        f.write(text);
        let startup = Startup::for_paths(
            &args(&[
                exe.to_str().unwrap(),
                "--config",
                f.file().to_str().unwrap(),
            ]),
            Path::new("missing"),
            None,
            None,
        )
        .unwrap();
        assert!(startup.config_text().contains("audio-output=off\n"));
        assert!(!startup.config.cli_overrides.contains_key("--audio-output"));
        let off = parse("version=1\n[audio]\noutput='off'").unwrap();
        let pipewire = parse("version=1\n[audio]\noutput='pipewire'").unwrap();
        assert!(startup.config.restart_fields(&off).is_empty());
        assert_eq!(startup.config.restart_fields(&pipewire), ["audio.output"]);
        let now = Instant::now();
        let mut monitor = startup.config.monitor(now);
        f.write("version=1\n[audio]\noutput='off'\nvolume=23");
        let candidate = monitor.poll(now + Duration::from_millis(500)).unwrap();
        assert_eq!(candidate.document.audio.volume, 23);
        assert!(candidate.restart_fields.is_empty());
        f.write("version=1\n[audio]\noutput='pipewire'\nvolume=24");
        assert_eq!(
            monitor
                .poll(now + Duration::from_millis(1000))
                .unwrap()
                .restart_fields,
            ["audio.output"]
        );
    }
    f.write("version=1\n[video]\nrenderer='softbuffer'\n[audio]\nvolume=42\nmuted=true\noutput='off'\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'");
    let startup = Startup::for_paths(
        &args(&[
            exe.to_str().unwrap(),
            "--config",
            f.file().to_str().unwrap(),
            "--renderer",
            "wgpu",
        ]),
        Path::new("missing"),
        None,
        None,
    )
    .unwrap();
    assert!(startup.config_text().contains("renderer=wgpu\n"));
    assert!(startup.config_text().contains("audio-volume=42\n"));
    assert!(startup.config_text().contains("audio-muted=true\n"));
    let changed = parse("version=1\n[video]\nrenderer='wgpu'\n[audio]\noutput='pipewire'").unwrap();
    assert_eq!(startup.config.restart_fields(&changed), ["audio.output"]);
    f.write("version=1\n[audio]\noutput='pipewire'");
    let headless = Startup::for_paths(
        &args(&[
            exe.to_str().unwrap(),
            "--config",
            f.file().to_str().unwrap(),
            "--headless",
        ]),
        Path::new("missing"),
        None,
        None,
    )
    .unwrap();
    assert!(headless.config_text().contains("audio-output=off\n"));
    assert!(headless.config.cli_overrides.contains_key("--audio-output"));
    let out = Startup::control_output(&args(&[
        "--controls",
        "--config",
        f.file().to_str().unwrap(),
    ]))
    .unwrap()
    .unwrap();
    assert!(out.contains("ArrowUp -> ArrowUp (48, 00)"));
    assert!(!f.0.join("saves").exists());
    let out = Startup::control_output(&args(&["--controls", "--no-config"]))
        .unwrap()
        .unwrap();
    assert!(out.contains("player-config=none"));
    assert!(out.contains("KeyW -> ArrowUp (48, 00) [source=default]"));
    for flags in [
        args(&["--config", f.file().to_str().unwrap(), "--no-config"]),
        args(&["--init-config", "--headless"]),
        args(&["--controls", "--renderer", "wgpu"]),
        args(&["--config"]),
    ] {
        assert!(Startup::control_output(&flags).is_err());
    }
    let target = f.0.join("new/config.toml");
    assert!(
        Startup::control_output(&args(&[
            "--init-config",
            "--config",
            target.to_str().unwrap()
        ]))
        .unwrap()
        .unwrap()
        .contains("Created config")
    );
    assert!(
        Startup::control_output(&args(&[
            "--init-config",
            "--config",
            target.to_str().unwrap()
        ]))
        .is_err()
    );
    assert!(
        Startup::control_output(&args(&["--bogus", "--help"]))
            .unwrap()
            .unwrap()
            .contains("Usage:")
    );
}
#[test]
fn player_config_relative_paths_merge_with_cli_without_creating_saves() {
    let f = Fixture::new();
    let layout = f.0.join("layout");
    fs::create_dir_all(layout.join("assets/unpacked")).unwrap();
    fs::create_dir_all(layout.join("assets/dos-cd")).unwrap();
    fs::write(
        layout.join("assets/unpacked/PAL.raw.exe"),
        b"authored parser fixture",
    )
    .unwrap();
    fs::write(layout.join("assets/GC050.BIN"), b"authored parser fixture").unwrap();
    fs::write(layout.join("assets/GC050.cue"), b"authored parser fixture").unwrap();
    let elf = f.0.join("pal");
    fs::write(&elf, b"authored parser fixture").unwrap();
    f.write("version=1\n[video]\nrenderer='softbuffer'\n[audio]\noutput='pipewire'\n[paths]\nresource_root='layout'\nsaves='private-saves'");
    let startup = Startup::for_paths(
        &args(&[
            "--config",
            f.file().to_str().unwrap(),
            "--headless",
            "--print-config",
        ]),
        &elf,
        None,
        None,
    )
    .unwrap();
    let text = startup.config_text();
    assert!(text.contains("renderer=softbuffer\n"));
    assert!(text.contains("audio-output=off\n"));
    assert_eq!(
        startup.saves.as_deref(),
        Some(f.0.join("private-saves").as_path())
    );
    assert!(!f.0.join("private-saves").exists());
    let cli_saves = f.0.join("cli-saves");
    let startup = Startup::for_paths(
        &args(&[
            "--config",
            f.file().to_str().unwrap(),
            "--headless",
            "--renderer",
            "wgpu",
            "--saves",
            cli_saves.to_str().unwrap(),
        ]),
        &elf,
        None,
        None,
    )
    .unwrap();
    assert_eq!(startup.saves.as_deref(), Some(cli_saves.as_path()));
    assert!(startup.config_text().contains("renderer=wgpu\n"));
    assert!(!cli_saves.exists());
}
