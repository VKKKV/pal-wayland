// SPDX-License-Identifier: GPL-3.0-or-later
//! Native player startup policy. Explicit positional EXEs retain the diagnostic
//! CLI defaults; only flags-only invocations opt into the PAL player layout.
//! Saves must be private: concurrent hostile directory replacement is outside
//! this std-only adapter's model, as it is for DosFiles.
use crate::player_config::{self, InitialConfig};
use std::{
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub const HELP: &str = r#"Usage: pal-wayland [OPTIONS]
       pal-wayland ORIGINAL_EXE [OPTIONS]
       pal remains a compatible short entry point.

No positional EXE: launch the PAL player (window, PipeWire, SB PCM, IRQs,
RAM top a000, legacy13 CD header). Resources are found beside executable
ancestors, never by guessing from the working directory:
  assets/unpacked/PAL.raw.exe, assets/dos-cd, assets/GC050.BIN + GC050.cue
Saves: $XDG_DATA_HOME/pal-wayland/saves, or
$HOME/.local/share/pal-wayland/saves. Both environment bases must be absolute.
Default config/saves lookup uses only pal-wayland; no old namespace fallback.
Missing config uses built-in settings (WASD + arrow keys for movement).
--init-config creates an example; [keymap] overrides built-in bindings.
Invalid paths, symlinks and permission errors fail, never trigger fallback.
Bundle player-config/SETUP.DAT seeds only a missing save SETUP.DAT; never replaces
existing settings. Explicit positional EXE: safe headless/audio-off defaults,
no automatic PAL resources, saves, device setup or config seeding.

Startup:
  --help, -h                 Show this help, anywhere; no filesystem access
  --print-config             Validate and print resolved settings; no writes/devices
  --config FILE              Explicit TOML config (missing/invalid is fatal)
  --no-config                Disable auto-loading and hot reload
  --controls                 Print physical QWERTY-position -> DOS keys and sources
                            Not layout characters; no devices/resources
  --init-config              Create private example config; NEVER overwrite (only --config allowed)
                            Default: $XDG_CONFIG_HOME/pal-wayland/config.toml
                            or $HOME/.config/pal-wayland/config.toml
                            Existing target config is never replaced; no old
                            namespace lookup or migration.
                            Regular files only, no config-file symlinks; limit 64 KiB
                            Config relative paths use file directory; CLI paths use cwd
  --resource-root DIR        Override player layout root (flags-only mode)
  --exe FILE                 Override player original EXE (flags-only mode)
  --assets DIR               Read-only DOS resource directory
  --saves DIR                Save directory; player creates it after preflight
  --cd-bin FILE              Raw CD image; matching .cue required
  --cd-iso FILE              ISO CD image instead of raw BIN/CUE
  --cd-header legacy13       Enable original PAL's legacy CD request header
  --window yes|no            Window or headless; player headless defaults audio off
  --renderer wgpu|softbuffer  Window renderer (default wgpu; explicit CPU comparison)
  --headless                 Alias for --window no (conflicts with --window yes)
  --audio-output off|pipewire Realtime audio (requires CD or SB; wall clock only)
  --audio-helper EXACTPATH   Trusted raw stdin helper, no shell/implicit arguments;
                            only with pipewire; s16ne, 44100 Hz, stereo interleaved
  --sound-blaster yes|no     SB PCM at 220h, IRQ7, DMA1; SB implies OPL absent
  --opl absent              FM synthesis unavailable
  --irq enabled|disabled    Guest hardware interrupt delivery
  --ram-top HEX             Conventional memory top segment (9f00..=a000)
  --x87 present|absent       Guest x87 presence

Advanced diagnostics (values required; repeat --stop/--set/--key-event as needed):
  --max-steps N              Instruction budget; default headless 1000000,
                            window unlimited unless explicitly supplied
  --virtual-step-ns N        Positive virtual nanoseconds per step; no realtime audio
  --clock-centiseconds N     Fixed DOS clock centiseconds (u8)
  --stop CS:IP               Headless-only: stop before this hexadecimal address
  --set REGISTER=HEX        ax bx cx dx si di bp sp cs ds es ss ip flags
  --key-event STEP:SCAN:ASCII:down|up  Scheduled headless key (scan/ascii hex)
  --memory-input FILE       Exactly 1 MiB guest-memory fixture, not a host save
  --trace NEWFILE           Headless JSONL instruction states (create-new)
  --trace-after N           Begin tracing at this step
  --memory NEWFILE          Headless final guest RAM (create-new)
  --state NEWFILE           Headless final register JSON (create-new)
  --frame-ppm NEWFILE       Headless final graphics frame (create-new)
  --audio-wav NEWFILE        Headless PCM capture (create-new); no realtime audio
  --audio-max-frames N      Explicit capture bound, required with --audio-wav

Relative explicit overrides are relative to the invoking working directory.
Unknown options, missing/invalid values and incompatible settings fail before
save-directory creation. Save/resource trees must be disjoint; save symlinks
are rejected. A moved ELF needs its resource layout or explicit overrides.
"#;

#[derive(Debug)]
pub struct Startup {
    pub exe: PathBuf,
    /// Constructors validate every CLI value and the effective TOML/CLI
    /// combinations. Keep ordered pairs: applying repeated machine options can
    /// have side effects that a last-value map would lose.
    pub options: Vec<String>,
    pub player: bool,
    pub print_config: bool,
    pub saves: Option<PathBuf>,
    pub seed: Option<PathBuf>,
    pub config: InitialConfig,
}
fn hex(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
        .map_err(|_| format!("invalid hexadecimal value {s:?}"))
}
fn unsigned(s: &str, flag: &str) -> Result<u64, String> {
    s.parse()
        .map_err(|_| format!("invalid {}: expected u64", flag.trim_start_matches("--")))
}
fn validate(flag: &str, value: &str) -> Result<(), String> {
    let choices: &[&str] = match flag {
        "--window" | "--sound-blaster" => &["yes", "no"],
        "--renderer" => &["wgpu", "softbuffer"],
        "--irq" => &["enabled", "disabled"],
        "--x87" => &["present", "absent"],
        "--audio-output" => &["off", "pipewire"],
        "--opl" => &["absent"],
        "--cd-header" => &["legacy13"],
        _ => &[],
    };
    if !choices.is_empty() && !choices.contains(&value) {
        return Err(format!("{flag} must be {}", choices.join(" or ")));
    }
    match flag {
        "--max-steps" | "--trace-after" => {
            unsigned(value, flag)?;
        }
        "--audio-max-frames" => {
            let n = unsigned(value, flag)?;
            if n == 0 || n > crate::audio_capture::MAX_WAV_FRAMES {
                return Err(format!(
                    "WAV max_frames must be 1..={}",
                    crate::audio_capture::MAX_WAV_FRAMES
                ));
            }
        }
        "--virtual-step-ns" => {
            if unsigned(value, flag)? == 0 {
                return Err("virtual-step-ns must be positive".into());
            }
        }
        "--clock-centiseconds" => {
            value
                .parse::<u8>()
                .map_err(|_| "invalid clock-centiseconds: expected u8")?;
        }
        "--ram-top" => {
            if !(0x9f00..=0xa000).contains(&hex(value)?) {
                return Err("ram-top must be between 9f00 and a000".into());
            }
        }
        "--stop" => {
            let (cs, ip) = value.split_once(':').ok_or("expected CS:IP")?;
            hex(cs)?;
            hex(ip)?;
        }
        "--set" => {
            let (name, val) = value.split_once('=').ok_or("expected REGISTER=HEX")?;
            if ![
                "ax", "bx", "cx", "dx", "si", "di", "bp", "sp", "cs", "ds", "es", "ss", "ip",
                "flags",
            ]
            .contains(&name.to_ascii_lowercase().as_str())
            {
                return Err(format!("unknown register {name}"));
            }
            hex(val)?;
        }
        "--key-event" => {
            let fields: Vec<_> = value.split(':').collect();
            if fields.len() != 4 {
                return Err("expected step:hex-scan:hex-ascii:down|up".into());
            }
            unsigned(fields[0], flag)?;
            if hex(fields[1])? >= 128 || hex(fields[2])? > 255 {
                return Err("key event values out of range".into());
            }
            if !["down", "up"].contains(&fields[3]) {
                return Err("expected down or up".into());
            }
        }
        "--assets" | "--saves" | "--cd-bin" | "--cd-iso" | "--memory-input" | "--trace"
        | "--memory" | "--state" | "--frame-ppm" | "--audio-wav" | "--audio-helper" | "--exe"
        | "--resource-root" => {
            if value.is_empty() {
                return Err(format!("{flag} path must not be empty"));
            }
        }
        "--window" | "--renderer" | "--sound-blaster" | "--irq" | "--x87" | "--audio-output"
        | "--opl" | "--cd-header" => {}
        _ => return Err(format!("unknown option {flag}")),
    }
    Ok(())
}
fn last<'a>(pairs: &'a [String], flag: &str) -> Option<&'a str> {
    pairs
        .chunks_exact(2)
        .rev()
        .find(|p| p[0] == flag)
        .map(|p| p[1].as_str())
}
fn add(pairs: &mut Vec<String>, flag: &str, value: impl AsRef<str>) {
    pairs.push(flag.into());
    pairs.push(value.as_ref().into());
}
fn path_string(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", p.display()))
}
fn require(p: &Path, directory: bool, kind: &str) -> Result<(), String> {
    if if directory { p.is_dir() } else { p.is_file() } {
        Ok(())
    } else {
        Err(format!(
            "missing {kind}: {}; keep the PAL layout beside the ELF or use --resource-root/--exe/--assets/--cd-bin",
            p.display()
        ))
    }
}
fn persistent_saves(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    let base = match xdg.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => {
            if !p.is_absolute() {
                return Err("XDG_DATA_HOME must be absolute".into());
            }
            p.to_path_buf()
        }
        None => {
            let p = home.ok_or("HOME or XDG_DATA_HOME is required for persistent saves")?;
            if !p.is_absolute() {
                return Err("HOME must be absolute".into());
            }
            p.join(".local/share")
        }
    };
    let path = base.join("pal-wayland/saves");
    player_config::default_location_exists(&path, true)?;
    Ok(path)
}
impl Startup {
    /// Call before resource/EXE/backend initialization. Help remains caller-owned
    /// and wins everywhere. Only explicitly requested init writes anything.
    pub fn control_output(args: &[String]) -> Result<Option<String>, String> {
        if args.iter().any(|s| s == "--help" || s == "-h") {
            return Ok(Some(HELP.into()));
        }
        let (_, _, _, _, _, flags) = parse(args)?;
        if !flags.controls && !flags.initialize {
            return Ok(None);
        }
        let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let home = env::var_os("HOME").map(PathBuf::from);
        if flags.initialize {
            let path = match &flags.path {
                Some(path) => player_config::initialize(path)?,
                None => player_config::initialize_default(xdg.as_deref(), home.as_deref())?,
            };
            return Ok(Some(format!("Created config {}\n", path.display())));
        }
        let config = selected_config(&flags, true, xdg.as_deref(), home.as_deref())?;
        Ok(Some(format!(
            "player-config={}\n{}",
            config
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none".into()),
            config.document.controls_text()
        )))
    }
    pub fn from_env(args: &[String]) -> Result<Self, String> {
        // Parse syntax before even looking up the running executable.
        let (positional, options, print_config, root, exe_override, flags) = parse(args)?;
        let executable = env::current_exe().map_err(|e| e.to_string())?;
        let xdg = env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let home = env::var_os("HOME").map(PathBuf::from);
        let config_home = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let config = selected_config(
            &flags,
            positional.is_none(),
            config_home.as_deref(),
            home.as_deref(),
        )?;
        Self::resolve(
            positional,
            options,
            print_config,
            root,
            exe_override,
            &executable,
            xdg.as_deref(),
            home.as_deref(),
            config,
        )
    }
    /// Injected executable/environment paths support authored relocation tests.
    pub fn for_paths(
        args: &[String],
        executable: &Path,
        xdg: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<Self, String> {
        let (positional, options, print_config, root, exe_override, flags) = parse(args)?;
        // Legacy injection never consults process XDG_CONFIG_HOME.
        let config = selected_config(&flags, positional.is_none() && home.is_some(), None, home)?;
        Self::resolve(
            positional,
            options,
            print_config,
            root,
            exe_override,
            executable,
            xdg,
            home,
            config,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        positional: Option<PathBuf>,
        mut options: Vec<String>,
        print_config: bool,
        root: Option<PathBuf>,
        exe_override: Option<PathBuf>,
        executable: &Path,
        xdg: Option<&Path>,
        home: Option<&Path>,
        mut config: InitialConfig,
    ) -> Result<Self, String> {
        let player = positional.is_none();
        let config_audio = config.resolve_audio_output(if player {
            player_config::AudioOutput::Pipewire
        } else {
            player_config::AudioOutput::Off
        });
        config.cli_overrides = options
            .chunks_exact(2)
            .map(|p| (p[0].clone(), p[1].clone()))
            .collect();
        if let Some(root) = &root {
            config
                .cli_overrides
                .insert("--resource-root".into(), path_string(root)?.into());
        }
        if last(&options, "--window") == Some("no") && last(&options, "--audio-output").is_none() {
            // Explicit headless stays audio-off even if the TOML selects PipeWire.
            config
                .cli_overrides
                .insert("--audio-output".into(), "off".into());
        }
        let root = root.or_else(|| config.document.paths.resource_root.clone());
        if config.loaded {
            if last(&options, "--renderer").is_none() {
                add(
                    &mut options,
                    "--renderer",
                    config.document.video.renderer.as_str(),
                );
            }
            if last(&options, "--audio-output").is_none() {
                let output = if last(&options, "--window") == Some("no") {
                    "off"
                } else {
                    config_audio.as_str()
                };
                add(&mut options, "--audio-output", output);
            }
            if last(&options, "--saves").is_none()
                && let Some(saves) = &config.document.paths.saves
            {
                add(&mut options, "--saves", path_string(saves)?);
            }
        }
        if !player {
            validate_combinations(&options)?;
            let saves = last(&options, "--saves").map(PathBuf::from);
            return Ok(Self {
                exe: positional.unwrap(),
                options,
                player,
                print_config,
                saves,
                seed: None,
                config,
            });
        }
        if last(&options, "--cd-bin").is_some() && last(&options, "--cd-iso").is_some() {
            return Err("choose only one of --cd-bin or --cd-iso in player mode".into());
        }
        let executable = fs::canonicalize(executable)
            .map_err(|e| format!("executable {}: {e}", executable.display()))?;
        let ancestors: Vec<_> = executable
            .parent()
            .ok_or("executable has no parent")?
            .ancestors()
            .collect();
        let root = root.or_else(|| {
            ancestors
                .iter()
                .find(|p| p.join("assets/unpacked/PAL.raw.exe").is_file())
                .map(|p| p.to_path_buf())
        });
        let need_root = exe_override.is_none()
            || last(&options, "--assets").is_none()
            || (last(&options, "--cd-bin").is_none() && last(&options, "--cd-iso").is_none());
        if need_root && root.is_none() {
            return Err(format!(
                "PAL resources not found beside executable {}; expected assets/unpacked/PAL.raw.exe, assets/dos-cd and assets/GC050.BIN + GC050.cue; use --resource-root or explicit --exe/--assets/--cd-bin",
                executable.display()
            ));
        }
        let exe = exe_override
            .unwrap_or_else(|| root.as_ref().unwrap().join("assets/unpacked/PAL.raw.exe"));
        let assets = last(&options, "--assets")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.as_ref().unwrap().join("assets/dos-cd"));
        let cd = last(&options, "--cd-bin")
            .or_else(|| last(&options, "--cd-iso"))
            .map(PathBuf::from)
            .unwrap_or_else(|| root.as_ref().unwrap().join("assets/GC050.BIN"));
        let bin = last(&options, "--cd-iso").is_none();
        require(&exe, false, "original EXE")?;
        require(&assets, true, "DOS assets directory")?;
        require(&cd, false, "CD image")?;
        if bin {
            require(&cd.with_extension("cue"), false, "CD cue")?;
        }
        let saves = match last(&options, "--saves") {
            Some(p) => PathBuf::from(p),
            None => persistent_saves(xdg, home)?,
        };
        let mut resources = vec![assets.clone(), exe.clone(), cd.clone()];
        if bin {
            resources.push(cd.with_extension("cue"));
        }
        let saves = validate_saves(&saves, &resources)?;
        let seed = ancestors
            .iter()
            .map(|p| p.join("player-config/SETUP.DAT"))
            .chain(root.iter().map(|p| p.join("player-config/SETUP.DAT")))
            .find(|p| p.exists());
        if let Some(p) = &seed {
            let md = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
            if !md.is_file() || md.file_type().is_symlink() {
                return Err("player config seed must be a regular file, not a symlink".into());
            }
            prospective(p)?; // Reject symlinked seed ancestors too.
            let seed_path = fs::canonicalize(p).map_err(|e| e.to_string())?;
            if seed_path.starts_with(&saves) || saves.starts_with(&seed_path) {
                return Err("save directory overlaps config seed".into());
            }
        }
        let mut defaults = Vec::new();
        add(&mut defaults, "--assets", path_string(&assets)?);
        add(
            &mut defaults,
            if bin { "--cd-bin" } else { "--cd-iso" },
            path_string(&cd)?,
        );
        add(&mut defaults, "--saves", path_string(&saves)?);
        for (flag, value) in [
            ("--ram-top", "a000"),
            ("--irq", "enabled"),
            ("--cd-header", "legacy13"),
            ("--sound-blaster", "yes"),
            ("--opl", "absent"),
            ("--window", "yes"),
            ("--renderer", "wgpu"),
        ] {
            add(&mut defaults, flag, value);
        }
        add(
            &mut defaults,
            "--audio-output",
            if last(&options, "--window") == Some("no") {
                "off"
            } else {
                "pipewire"
            },
        );
        // Do not apply overridden defaults first (RAM configuration is monotonic).
        let mut merged = Vec::new();
        for pair in defaults.chunks_exact(2) {
            if last(&options, &pair[0]).is_none() {
                add(&mut merged, &pair[0], &pair[1]);
            }
        }
        merged.extend(options);
        let defaults = merged;
        validate_combinations(&defaults)?;
        preflight_outputs(&defaults)?;
        Ok(Self {
            exe,
            options: defaults,
            player,
            print_config,
            saves: Some(saves),
            seed,
            config,
        })
    }
    /// Call only after machine/resource validation and selected backend readiness
    /// (window creation and audio startup), before guest execution; never for print-config.
    pub fn prepare_saves(&self) -> Result<(), String> {
        if !self.player {
            return Ok(());
        }
        let root = self.saves.as_ref().unwrap();
        let mut resources: Vec<_> = ["--assets", "--cd-bin", "--cd-iso"]
            .iter()
            .filter_map(|f| last(&self.options, f))
            .map(PathBuf::from)
            .chain(std::iter::once(self.exe.clone()))
            .collect();
        if let Some(bin) = last(&self.options, "--cd-bin") {
            resources.push(Path::new(bin).with_extension("cue"));
        }
        validate_saves(root, &resources)?;
        if let Some(seed) = &self.seed {
            prospective(seed)?;
        }
        let seed_bytes = self
            .seed
            .as_ref()
            .map(fs::read)
            .transpose()
            .map_err(|e| format!("config seed: {e}"))?;
        fs::create_dir_all(root).map_err(|e| format!("saves {}: {e}", root.display()))?;
        validate_saves(root, &resources)?;
        if let Some(bytes) = seed_bytes {
            let mut existing = false;
            for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("SETUP.DAT")
                {
                    if existing || !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                        return Err("ambiguous/non-file saved SETUP.DAT".into());
                    }
                    existing = true;
                }
            }
            if !existing {
                seed_setup(root, &bytes)?;
                validate_saves(root, &resources)?;
            }
        }
        Ok(())
    }
    pub fn config_text(&self) -> String {
        let mut text = format!(
            "mode={}\nexe={}\n",
            if self.player {
                "player"
            } else {
                "explicit-exe"
            },
            self.exe.display()
        );
        for (flag, default) in [
            ("--assets", "none"),
            ("--saves", "none"),
            ("--cd-bin", "none"),
            ("--cd-iso", "none"),
            ("--cd-header", "standard"),
            ("--ram-top", "9f00"),
            ("--irq", "disabled"),
            ("--sound-blaster", "no"),
            ("--opl", "unsupported"),
            ("--x87", "absent"),
            ("--window", "no"),
            ("--renderer", "wgpu"),
            ("--audio-output", "off"),
        ] {
            let value = last(&self.options, flag).unwrap_or(default);
            let value = if flag == "--opl" && last(&self.options, "--sound-blaster") == Some("yes")
            {
                "absent"
            } else {
                value
            };
            text.push_str(&format!("{}={}\n", &flag[2..], value));
        }
        let window = last(&self.options, "--window") == Some("yes");
        text.push_str(&format!(
            "max-steps={}\nconfig-seed={}\n",
            last(&self.options, "--max-steps").unwrap_or(if window {
                "unlimited"
            } else {
                "1000000"
            }),
            self.seed
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none".into())
        ));
        text.push_str(&format!("player-config={}\nconfig-loaded={}\naudio-volume={}\naudio-muted={}\nreload-enabled={}\nreload-interval-ms={}\n", self.config.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()), self.config.loaded, self.config.document.audio.volume, self.config.document.audio.muted, self.config.path.is_some() && self.config.document.reload.enabled, self.config.document.reload.interval_ms));
        text.push_str(&self.config.document.controls_text());
        text
    }
}
#[derive(Default)]
struct ConfigFlags {
    path: Option<PathBuf>,
    disabled: bool,
    controls: bool,
    initialize: bool,
}
fn selected_config(
    flags: &ConfigFlags,
    player: bool,
    xdg: Option<&Path>,
    home: Option<&Path>,
) -> Result<InitialConfig, String> {
    let path = if flags.disabled {
        None
    } else if let Some(path) = &flags.path {
        Some(path.clone())
    } else if player {
        return InitialConfig::load_default(&player_config::lookup_path(xdg, home)?);
    } else {
        None
    };
    InitialConfig::load(path.as_deref(), flags.path.is_some())
}
type Parsed = (
    Option<PathBuf>,
    Vec<String>,
    bool,
    Option<PathBuf>,
    Option<PathBuf>,
    ConfigFlags,
);
fn parse(args: &[String]) -> Result<Parsed, String> {
    let positional = args
        .first()
        .filter(|s| !s.starts_with('-'))
        .map(PathBuf::from);
    let mut i = usize::from(positional.is_some());
    let mut options = Vec::new();
    let mut print = false;
    let mut root = None;
    let mut exe = None;
    let mut headless = false;
    let mut flags = ConfigFlags::default();
    while i < args.len() {
        let flag = &args[i];
        match flag.as_str() {
            "--no-config" => {
                flags.disabled = true;
                i += 1;
                continue;
            }
            "--controls" => {
                flags.controls = true;
                i += 1;
                continue;
            }
            "--init-config" => {
                flags.initialize = true;
                i += 1;
                continue;
            }
            "--config" => {
                let value = args
                    .get(i + 1)
                    .filter(|s| !s.starts_with("--") && !s.is_empty())
                    .ok_or("option missing value: --config")?;
                if flags.path.is_some() {
                    return Err("--config may only be supplied once".into());
                }
                flags.path = Some(PathBuf::from(value));
                i += 2;
                continue;
            }
            _ => (),
        }
        if flag == "--print-config" {
            print = true;
            i += 1;
            continue;
        }
        if flag == "--headless" {
            headless = true;
            add(&mut options, "--window", "no");
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .filter(|s| !s.starts_with("--"))
            .ok_or_else(|| format!("option missing value: {flag}"))?;
        validate(flag, value)?;
        match flag.as_str() {
            "--resource-root" => root = Some(PathBuf::from(value)),
            "--exe" => exe = Some(PathBuf::from(value)),
            _ => add(&mut options, flag, value),
        }
        i += 2;
    }
    if flags.disabled && flags.path.is_some() {
        return Err("--config conflicts with --no-config".into());
    }
    if flags.initialize
        && (flags.disabled
            || flags.controls
            || print
            || positional.is_some()
            || !options.is_empty()
            || root.is_some()
            || exe.is_some())
    {
        return Err("--init-config permits only --config FILE".into());
    }
    if flags.controls
        && (flags.initialize
            || print
            || positional.is_some()
            || !options.is_empty()
            || root.is_some()
            || exe.is_some())
    {
        return Err("--controls permits only --config FILE or --no-config".into());
    }
    if positional.is_some() && (root.is_some() || exe.is_some()) {
        return Err("--resource-root/--exe require flags-only player mode".into());
    }
    if headless
        && options
            .chunks_exact(2)
            .any(|p| p[0] == "--window" && p[1] == "yes")
    {
        return Err("--headless conflicts with --window yes".into());
    }
    // Combination checks run after TOML/CLI merge: config can legitimately
    // supply --audio-output for an explicit CLI --audio-helper.
    Ok((positional, options, print, root, exe, flags))
}
fn validate_combinations(pairs: &[String]) -> Result<(), String> {
    // Option-only policy, shared by player and explicit-EXE startup after merge.
    // Successful resource mounting and machine initialization are checked by
    // the generated caller, not inferred here from the presence of a flag.
    let has = |flag| last(pairs, flag).is_some();
    let audio = last(pairs, "--audio-output") == Some("pipewire");

    if has("--audio-wav") != has("--audio-max-frames") {
        return Err("audio-wav requires explicit audio-max-frames and vice versa".into());
    }
    let device =
        has("--cd-bin") || has("--cd-iso") || last(pairs, "--sound-blaster") == Some("yes");
    if has("--audio-wav") && !device {
        return Err("audio-wav requires CD mount".into());
    }
    if has("--audio-helper") && !audio {
        return Err("audio-helper requires --audio-output pipewire".into());
    }
    if audio && has("--virtual-step-ns") {
        return Err("audio-output requires wall time; cannot combine with virtual-step-ns".into());
    }
    if audio && has("--audio-wav") {
        return Err("audio-output cannot combine with audio-wav (single CD PCM consumer)".into());
    }
    if audio && !device {
        return Err("audio-output requires CD mount".into());
    }
    if last(pairs, "--window") == Some("yes") {
        if has("--stop") {
            return Err("--stop is headless-only".into());
        }
        if has("--audio-wav") {
            return Err("audio-wav capture is headless-only".into());
        }
        if has("--key-event") {
            return Err(
                "scheduled key events are headless-only; window uses real host input".into(),
            );
        }
        if ["--trace", "--memory", "--state", "--frame-ppm"]
            .iter()
            .any(|f| has(f))
        {
            return Err("window mode does not support headless output options".into());
        }
    }
    Ok(())
}
/// Publish only complete seed bytes. A hard link is an atomic no-replace
/// publication within this private directory, unlike an overwriting rename.
fn seed_setup(root: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (stage, mut file) = loop {
        let stage = root.join(format!(
            ".SETUP.DAT.seed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&stage) {
            Ok(file) => break (stage, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("stage SETUP.DAT: {e}")),
        }
    };
    let result = (|| {
        file.write_all(bytes).and_then(|_| file.sync_all())?;
        match fs::hard_link(&stage, root.join("SETUP.DAT")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    })();
    drop(file);
    let cleanup = fs::remove_file(&stage);
    result.map_err(|e| format!("seed SETUP.DAT: {e}"))?;
    cleanup.map_err(|e| format!("clean up staged SETUP.DAT: {e}"))
}
fn preflight_outputs(options: &[String]) -> Result<(), String> {
    for flag in [
        "--trace",
        "--memory",
        "--state",
        "--frame-ppm",
        "--audio-wav",
    ] {
        if let Some(value) = last(options, flag) {
            let p = Path::new(value);
            match fs::symlink_metadata(p) {
                Ok(_) => return Err(format!("{flag} requires a new output file: {value}")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{flag} {value}: {e}")),
            }
            let parent = p
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !parent.is_dir() {
                return Err(format!(
                    "{flag} output parent must exist: {}",
                    parent.display()
                ));
            }
        }
    }
    Ok(())
}
/// Resolve a not-yet-created path without losing symlink/overlap checks.
fn prospective(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_err(|e| e.to_string())?.join(path)
    };
    let mut built = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                built.pop();
            }
            Component::CurDir => {}
            _ => {
                built.push(component);
            }
        }
        match fs::symlink_metadata(&built) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!(
                    "save path must not traverse symlinks: {}",
                    built.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("save path {}: {e}", built.display())),
        }
    }
    Ok(built)
}
pub fn validate_saves(path: &Path, resources: &[PathBuf]) -> Result<PathBuf, String> {
    let path = prospective(path)?;
    for resource in resources {
        let r = fs::canonicalize(resource)
            .map_err(|e| format!("resource {}: {e}", resource.display()))?;
        if path.starts_with(&r) || r.starts_with(&path) {
            return Err(
                "save and resource paths must be disjoint (no parent/child overlap)".into(),
            );
        }
    }
    fn walk(p: &Path) -> Result<(), String> {
        for entry in fs::read_dir(p).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let ty = entry.file_type().map_err(|e| e.to_string())?;
            if ty.is_symlink() {
                return Err(format!(
                    "saves must not contain symlinks: {}",
                    entry.path().display()
                ));
            }
            if ty.is_dir() {
                walk(&entry.path())?;
            }
        }
        Ok(())
    }
    if path.exists() {
        if !path.is_dir() {
            return Err("saves is not a directory".into());
        }
        walk(&path)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    // Exercise the explicit-EXE constructor's post-merge validation without
    // files or host environment. The generated caller owns actual mounting.
    fn explicit(values: &[&str], toml: Option<&str>) -> Result<Startup, String> {
        let (positional, options, print, root, exe, _) = parse(&strings(values))?;
        let mut config = InitialConfig::load(None, false)?;
        if let Some(text) = toml {
            config.document =
                player_config::Config::parse(text.as_bytes(), Path::new("/authored/config.toml"))?;
            config.loaded = true;
        }
        Startup::resolve(
            positional,
            options,
            print,
            root,
            exe,
            Path::new("/unused/elf"),
            None,
            None,
            config,
        )
    }

    #[test]
    fn scalar_choices_are_validated_before_application() {
        for (flag, choices) in [
            ("--window", &["yes", "no"][..]),
            ("--sound-blaster", &["yes", "no"][..]),
            ("--renderer", &["wgpu", "softbuffer"][..]),
            ("--irq", &["enabled", "disabled"][..]),
            ("--x87", &["present", "absent"][..]),
            ("--audio-output", &["off", "pipewire"][..]),
            ("--opl", &["absent"][..]),
            ("--cd-header", &["legacy13"][..]),
        ] {
            for choice in choices {
                assert!(validate(flag, choice).is_ok(), "{flag} {choice}");
            }
            for invalid in ["", "unknown", "YES"] {
                assert!(validate(flag, invalid).unwrap_err().starts_with(flag));
            }
        }
    }

    #[test]
    fn numeric_and_key_event_boundaries_match_application_types() {
        for (flag, accepted, rejected) in [
            (
                "--max-steps",
                &["0", "18446744073709551615"][..],
                &["-1", "18446744073709551616"][..],
            ),
            (
                "--trace-after",
                &["0", "18446744073709551615"][..],
                &["-1", "18446744073709551616"][..],
            ),
            (
                "--virtual-step-ns",
                &["1", "18446744073709551615"][..],
                &["0", "-1", "18446744073709551616"][..],
            ),
            (
                "--clock-centiseconds",
                &["0", "255"][..],
                &["-1", "256"][..],
            ),
            (
                "--ram-top",
                &["9f00", "0x9f00", "a000"][..],
                &["9eff", "a001", "10000", "0X9f00"][..],
            ),
            (
                "--key-event",
                &["0:00:00:up", "18446744073709551615:7f:ff:down"][..],
                &[
                    "0:80:00:down",
                    "0:7f:100:up",
                    "0:1c:0d:held",
                    "0:1c:0d",
                    "-1:1c:0d:down",
                ][..],
            ),
        ] {
            for value in accepted {
                assert!(validate(flag, value).is_ok(), "{flag} {value}");
            }
            for value in rejected {
                assert!(validate(flag, value).is_err(), "{flag} {value}");
            }
        }
        for value in ["1".into(), crate::audio_capture::MAX_WAV_FRAMES.to_string()] {
            assert!(validate("--audio-max-frames", &value).is_ok());
        }
        for value in [
            "0".into(),
            (crate::audio_capture::MAX_WAV_FRAMES + 1).to_string(),
        ] {
            assert!(validate("--audio-max-frames", &value).is_err());
        }
    }

    #[test]
    fn effective_combinations_reject_before_generated_side_effects() {
        for (options, expected) in [
            (
                vec!["--audio-wav", "new.wav"],
                "audio-wav requires explicit audio-max-frames and vice versa",
            ),
            (
                vec!["--audio-max-frames", "1"],
                "audio-wav requires explicit audio-max-frames and vice versa",
            ),
            (
                vec!["--audio-wav", "new.wav", "--audio-max-frames", "1"],
                "audio-wav requires CD mount",
            ),
            (
                vec!["--audio-helper", "must-not-spawn"],
                "audio-helper requires --audio-output pipewire",
            ),
            (
                vec!["--audio-output", "pipewire", "--virtual-step-ns", "1"],
                "audio-output requires wall time; cannot combine with virtual-step-ns",
            ),
            (
                vec![
                    "--audio-output",
                    "pipewire",
                    "--sound-blaster",
                    "yes",
                    "--audio-wav",
                    "new.wav",
                    "--audio-max-frames",
                    "1",
                ],
                "audio-output cannot combine with audio-wav (single CD PCM consumer)",
            ),
            (
                vec!["--audio-output", "pipewire"],
                "audio-output requires CD mount",
            ),
            (
                vec![
                    "--window",
                    "yes",
                    "--sound-blaster",
                    "yes",
                    "--audio-wav",
                    "new.wav",
                    "--audio-max-frames",
                    "1",
                ],
                "audio-wav capture is headless-only",
            ),
            (
                vec!["--window", "yes", "--key-event", "0:1c:0d:down"],
                "scheduled key events are headless-only; window uses real host input",
            ),
        ] {
            let mut args = vec!["authored.exe"];
            args.extend(options);
            assert_eq!(explicit(&args, None).unwrap_err(), expected);
        }
        for output in ["--trace", "--memory", "--state", "--frame-ppm"] {
            assert_eq!(
                explicit(
                    &["authored.exe", "--window", "yes", output, "new.file"],
                    None
                )
                .unwrap_err(),
                "window mode does not support headless output options"
            );
        }
    }

    #[test]
    fn explicit_exe_combinations_use_toml_merge_and_cli_priority() {
        let config = "version=1\n[audio]\noutput='pipewire'";
        let args = [
            "authored.exe",
            "--audio-helper",
            "must-not-spawn",
            "--sound-blaster",
            "yes",
        ];
        let startup = explicit(&args, Some(config)).unwrap();
        assert!(!startup.player);
        assert_eq!(last(&startup.options, "--audio-output"), Some("pipewire"));
        assert_eq!(
            last(&startup.options, "--audio-helper"),
            Some("must-not-spawn")
        );
        assert_eq!(
            explicit(&["authored.exe", "--virtual-step-ns", "1"], Some(config)).unwrap_err(),
            "audio-output requires wall time; cannot combine with virtual-step-ns"
        );
        assert_eq!(
            explicit(
                &[
                    "authored.exe",
                    "--audio-helper",
                    "must-not-spawn",
                    "--audio-output",
                    "off"
                ],
                Some(config)
            )
            .unwrap_err(),
            "audio-helper requires --audio-output pipewire"
        );
        let startup = explicit(
            &["authored.exe", "--headless", "--virtual-step-ns", "1"],
            Some(config),
        )
        .unwrap();
        assert_eq!(last(&startup.options, "--audio-output"), Some("off"));
    }

    #[test]
    fn repeated_options_keep_application_order_and_validate_every_value() {
        let args = [
            "authored.exe",
            "--ram-top",
            "a000",
            "--ram-top",
            "9f00",
            "--sound-blaster",
            "yes",
            "--sound-blaster",
            "no",
            "--set",
            "AX=ffff",
            "--set",
            "ax=0000",
            "--stop",
            "1010:0000",
            "--stop",
            "1010:0001",
        ];
        let startup = explicit(&args, None).unwrap();
        assert_eq!(startup.options, strings(&args[1..]));
        // Last-value policy must not excuse an earlier invalid value.
        assert_eq!(
            explicit(
                &["authored.exe", "--window", "invalid", "--window", "no"],
                None
            )
            .unwrap_err(),
            "--window must be yes or no"
        );
        assert_eq!(
            explicit(
                &[
                    "authored.exe",
                    "--audio-output",
                    "pipewire",
                    "--sound-blaster",
                    "yes",
                    "--sound-blaster",
                    "no"
                ],
                None
            )
            .unwrap_err(),
            "audio-output requires CD mount"
        );
        // A mount flag passes option-only policy; it does not claim a usable CD.
        assert!(
            explicit(
                &[
                    "authored.exe",
                    "--audio-output",
                    "pipewire",
                    "--cd-bin",
                    "missing.bin"
                ],
                None
            )
            .is_ok()
        );
    }
}
