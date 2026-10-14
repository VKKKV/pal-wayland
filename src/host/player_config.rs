// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded TOML policy with explicit atomic initialization. No device, resource
//! or save access here.
//! Config-file symlinks are rejected; explicit config paths permit ordinary
//! symlinked parents, while default namespace lookup rejects all symlinks.
//! Polling opens a fresh regular file (also detects atomic replace).
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Single source for the initializer and built-in key bindings. Other example
/// settings are not applied as defaults: audio output depends on startup mode.
pub const EXAMPLE: &str = include_str!("../../examples/config.toml");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Renderer {
    #[default]
    Wgpu,
    Softbuffer,
}
impl TryFrom<String> for Renderer {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "wgpu" => Ok(Self::Wgpu),
            "softbuffer" => Ok(Self::Softbuffer),
            _ => Err("video.renderer must be wgpu or softbuffer"),
        }
    }
}
impl Renderer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wgpu => "wgpu",
            Self::Softbuffer => "softbuffer",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum AudioOutput {
    Off,
    #[default]
    Pipewire,
}
impl TryFrom<String> for AudioOutput {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "off" => Ok(Self::Off),
            "pipewire" => Ok(Self::Pipewire),
            _ => Err("audio.output must be off or pipewire"),
        }
    }
}
impl AudioOutput {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pipewire => "pipewire",
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    pub renderer: Renderer,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// Omission keeps the startup mode's default; it is not an explicit backend.
    pub output: Option<AudioOutput>,
    pub volume: u8,
    pub muted: bool,
}
impl Default for Audio {
    fn default() -> Self {
        Self {
            output: None,
            volume: 100,
            muted: false,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub resource_root: Option<PathBuf>,
    pub saves: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reload {
    pub enabled: bool,
    pub interval_ms: u64,
}
impl Default for Reload {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 500,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub video: Video,
    #[serde(default)]
    pub audio: Audio,
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub reload: Reload,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            video: Video::default(),
            audio: Audio::default(),
            keymap: BTreeMap::new(),
            paths: Paths::default(),
            reload: Reload::default(),
        }
    }
}

/// Parse the embedded asset once, retaining only key bindings, not player-only
/// backend choices. Config::keymap itself stays user-only for source reporting.
fn default_keymap() -> &'static BTreeMap<String, String> {
    static KEYS: std::sync::LazyLock<BTreeMap<String, String>> = std::sync::LazyLock::new(|| {
        Config::parse(EXAMPLE.as_bytes(), Path::new("examples/config.toml"))
            .expect("embedded player config must be valid")
            .keymap
    });
    &KEYS
}

/// Physical host names and DOS scan/ASCII pairs. Letters are lowercase,
/// numbers unshifted; no host keyboard-layout/Unicode interpretation.
pub fn supported_keys() -> BTreeMap<String, (u8, u8)> {
    let mut keys = BTreeMap::new();
    for (name, scan, ascii) in [
        ("Escape", 0x01, 27),
        ("Enter", 0x1c, 13),
        ("Space", 0x39, 32),
        ("ArrowUp", 0x48, 0),
        ("ArrowDown", 0x50, 0),
        ("ArrowLeft", 0x4b, 0),
        ("ArrowRight", 0x4d, 0),
        ("Tab", 0x0f, 9),
        ("Backspace", 0x0e, 8),
        ("ControlLeft", 0x1d, 0),
        ("ControlRight", 0x1d, 0),
        ("ShiftLeft", 0x2a, 0),
        ("ShiftRight", 0x36, 0),
        ("AltLeft", 0x38, 0),
        ("AltRight", 0x38, 0),
        ("Home", 0x47, 0),
        ("End", 0x4f, 0),
        ("PageUp", 0x49, 0),
        ("PageDown", 0x51, 0),
        ("Insert", 0x52, 0),
        ("Delete", 0x53, 0),
    ] {
        keys.insert(name.into(), (scan, ascii));
    }
    for (letter, scan) in [
        ('A', 0x1e),
        ('B', 0x30),
        ('C', 0x2e),
        ('D', 0x20),
        ('E', 0x12),
        ('F', 0x21),
        ('G', 0x22),
        ('H', 0x23),
        ('I', 0x17),
        ('J', 0x24),
        ('K', 0x25),
        ('L', 0x26),
        ('M', 0x32),
        ('N', 0x31),
        ('O', 0x18),
        ('P', 0x19),
        ('Q', 0x10),
        ('R', 0x13),
        ('S', 0x1f),
        ('T', 0x14),
        ('U', 0x16),
        ('V', 0x2f),
        ('W', 0x11),
        ('X', 0x2d),
        ('Y', 0x15),
        ('Z', 0x2c),
    ] {
        keys.insert(
            format!("Key{letter}"),
            (scan, letter.to_ascii_lowercase() as u8),
        );
    }
    for digit in 0..=9u8 {
        keys.insert(
            format!("Digit{digit}"),
            (if digit == 0 { 0x0b } else { digit + 1 }, b'0' + digit),
        );
    }
    for n in 1..=12u8 {
        keys.insert(
            format!("F{n}"),
            (if n <= 10 { 0x3a + n } else { 0x57 + n - 11 }, 0),
        );
    }
    keys
}
impl Config {
    pub fn parse(bytes: &[u8], file: &Path) -> Result<Self, String> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err("config exceeds 64 KiB".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|e| format!("config is not UTF-8: {e}"))?;
        // toml's default recursion limit stays enabled; no `unbounded` feature.
        let mut config: Self =
            toml::from_str(text).map_err(|e| format!("invalid TOML config: {e}"))?;
        if config.version != 1 {
            return Err("config version must be 1".into());
        }
        if config.audio.volume > 100 {
            return Err("audio.volume must be 0..100".into());
        }
        if !(100..=5000).contains(&config.reload.interval_ms) {
            return Err("reload.interval_ms must be 100..5000".into());
        }
        let supported = supported_keys();
        for (host, target) in &config.keymap {
            if !supported.contains_key(host) {
                return Err(format!("unsupported host key {host:?}"));
            }
            if target != "None" && !supported.contains_key(target) {
                return Err(format!("unsupported DOS key {target:?}"));
            }
        }
        let parent = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for path in [&mut config.paths.resource_root, &mut config.paths.saves]
            .into_iter()
            .flatten()
        {
            if path.as_os_str().is_empty() {
                return Err("config paths must not be empty".into());
            }
            if !path.is_absolute() {
                *path = parent.join(&*path);
            }
        }
        Ok(config)
    }
    pub fn resolved_keymap(&self) -> BTreeMap<String, (u8, u8)> {
        let supported = supported_keys();
        let mut map: BTreeMap<_, _> = default_keymap()
            .iter()
            .filter_map(|(host, target)| supported.get(target).map(|&pair| (host.clone(), pair)))
            .collect();
        for (host, target) in &self.keymap {
            if target == "None" {
                map.remove(host);
            } else if let Some(pair) = supported.get(target) {
                map.insert(host.clone(), *pair);
            }
        }
        map
    }
    pub fn controls_text(&self) -> String {
        let mut text = String::from(
            "input-basis=physical (QWERTY-position names, not layout characters)\noutput-target=DOS key (scan, ASCII hexadecimal)\nprecedence: default < user [keymap]; None disables a host key\nsources: default=built-in DOS mapping; user=[keymap] override\nhost-key -> DOS key (scan, ASCII hexadecimal) [source]\n",
        );
        let supported = supported_keys();
        text.push_str(&format!(
            "supported-keys={}\n",
            supported.keys().cloned().collect::<Vec<_>>().join(",")
        ));
        for (host, (scan, ascii)) in self.resolved_keymap() {
            let target = self
                .keymap
                .get(&host)
                .map(String::as_str)
                .unwrap_or_else(|| {
                    default_keymap()
                        .get(&host)
                        .map(String::as_str)
                        .expect("resolved host has a built-in target")
                });
            debug_assert_eq!(supported[target], (scan, ascii));
            let source = if self.keymap.contains_key(&host) {
                "user"
            } else {
                "default"
            };
            text.push_str(&format!(
                "{host} -> {target} ({scan:02x}, {ascii:02x}) [source={source}]\n"
            ));
        }
        for (host, target) in &self.keymap {
            if target == "None" {
                text.push_str(&format!("{host} -> None (disabled) [source=user]\n"));
            }
        }
        text
    }
}

pub fn default_path(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    let base = if let Some(path) = xdg.filter(|p| !p.as_os_str().is_empty()) {
        if !path.is_absolute() {
            return Err("XDG_CONFIG_HOME must be absolute".into());
        }
        path.to_path_buf()
    } else {
        let home = home.ok_or("HOME or XDG_CONFIG_HOME is required for player config")?;
        if !home.is_absolute() {
            return Err("HOME must be absolute".into());
        }
        home.join(".config")
    };
    Ok(base.join("pal-wayland/config.toml"))
}
/// Default lookup rejects symlinks (including dangling ancestors), invalid
/// types and permission errors. Explicit config paths retain their existing
/// policy of allowing symlinked parents.
pub(crate) fn default_location_exists(path: &Path, directory: bool) -> Result<bool, String> {
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(md) => {
                if md.file_type().is_symlink() {
                    return Err(format!(
                        "default path must not traverse symlinks: {}",
                        ancestor.display()
                    ));
                }
                let valid = if ancestor == path && !directory {
                    md.is_file()
                } else {
                    md.is_dir()
                };
                if !valid {
                    return Err(format!(
                        "invalid default {}: expected {}",
                        ancestor.display(),
                        if ancestor == path && !directory {
                            "regular file"
                        } else {
                            "directory"
                        }
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("default path {}: {e}", ancestor.display())),
        }
    }
    Ok(true)
}
/// Choose the pal-wayland path once at startup; hot reload monitors this exact
/// path even if it vanishes. No other namespace is inspected.
pub fn lookup_path(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    let path = default_path(xdg, home)?;
    default_location_exists(&path, false)?;
    Ok(path)
}
/// Initialize only the pal-wayland namespace without replacing an existing file.
pub fn initialize_default(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    initialize(&lookup_path(xdg, home)?)
}
fn absolute(path: &Path) -> Result<PathBuf, String> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    })
}
fn read_bounded(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    // NONBLOCK prevents a hostile FIFO replacement from hanging startup. Open
    // descriptor metadata, not path metadata, establishes a regular file.
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    let file = File::from(fd);
    let md = file.metadata()?;
    if !md.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config must be a regular file (no symlink/FIFO/device)",
        ));
    }
    if md.len() > MAX_CONFIG_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config exceeds 64 KiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config exceeds 64 KiB",
        ));
    }
    Ok(bytes)
}
#[derive(Debug, Clone)]
pub struct InitialConfig {
    pub path: Option<PathBuf>,
    pub document: Config,
    pub loaded: bool,
    /// Explicit CLI settings pin restart-only fields during reload comparison.
    pub cli_overrides: BTreeMap<String, String>,
    audio_default: AudioOutput,
    strict_default_path: bool,
    observed: Option<Vec<u8>>,
}
impl InitialConfig {
    pub fn load(path: Option<&Path>, explicit: bool) -> Result<Self, String> {
        let path = path.map(absolute).transpose()?;
        let mut initial = Self {
            path,
            document: Config::default(),
            loaded: false,
            cli_overrides: BTreeMap::new(),
            audio_default: AudioOutput::Pipewire,
            strict_default_path: false,
            observed: None,
        };
        if let Some(path) = &initial.path {
            match read_bounded(path) {
                Ok(bytes) => {
                    initial.document = Config::parse(&bytes, path)
                        .map_err(|e| format!("config {}: {e}", path.display()))?;
                    initial.loaded = true;
                    initial.observed = Some(bytes);
                }
                Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(format!("config {}: {e}", path.display())),
            }
        }
        Ok(initial)
    }
    /// Only default namespace lookup has the no-symlink ancestor policy.
    /// Explicit config paths retain their supported dotfiles parent links.
    pub fn load_default(path: &Path) -> Result<Self, String> {
        default_location_exists(path, false)?;
        let mut initial = Self::load(Some(path), false)?;
        initial.strict_default_path = true;
        Ok(initial)
    }
    pub fn monitor(&self, now: Instant) -> HotReload {
        HotReload::new(self.clone(), now)
    }
    /// Startup sets this once; reload uses the same mode-dependent fallback.
    pub fn resolve_audio_output(&mut self, default: AudioOutput) -> AudioOutput {
        self.audio_default = default;
        self.document.audio.output.unwrap_or(default)
    }
    /// Compare against immutable startup values, ignoring CLI-pinned fields.
    pub fn restart_fields(&self, candidate: &Config) -> Vec<&'static str> {
        let mut fields = Vec::new();
        for (name, flag, changed) in [
            (
                "video.renderer",
                "--renderer",
                candidate.video != self.document.video,
            ),
            (
                "audio.output",
                "--audio-output",
                candidate.audio.output.unwrap_or(self.audio_default)
                    != self.document.audio.output.unwrap_or(self.audio_default),
            ),
            (
                "paths.resource_root",
                "--resource-root",
                candidate.paths.resource_root != self.document.paths.resource_root,
            ),
            (
                "paths.saves",
                "--saves",
                candidate.paths.saves != self.document.paths.saves,
            ),
        ] {
            if changed && !self.cli_overrides.contains_key(flag) {
                fields.push(name);
            }
        }
        fields
    }
}
#[derive(Debug, Clone)]
pub struct Candidate {
    pub document: Config,
    pub restart_fields: Vec<&'static str>,
}
#[derive(Debug)]
pub struct HotReload {
    initial: InitialConfig,
    current: Config,
    observed: Option<Vec<u8>>,
    next_poll: Instant,
    last_error: Option<String>,
    pending_error: Option<String>,
}
impl HotReload {
    pub fn new(initial: InitialConfig, now: Instant) -> Self {
        Self {
            current: initial.document.clone(),
            observed: initial.observed.clone(),
            next_poll: now + Duration::from_millis(initial.document.reload.interval_ms),
            initial,
            last_error: None,
            pending_error: None,
        }
    }
    pub fn current(&self) -> &Config {
        &self.current
    }
    /// Deduplicated error, taken once by caller for logging; last valid retained.
    pub fn take_error(&mut self) -> Option<String> {
        self.pending_error.take()
    }
    pub fn poll(&mut self, now: Instant) -> Option<Candidate> {
        if !self.current.reload.enabled || now < self.next_poll {
            return None;
        }
        let path = self.initial.path.as_ref()?;
        self.next_poll = now + Duration::from_millis(self.current.reload.interval_ms);
        let result = (if self.initial.strict_default_path {
            default_location_exists(path, false).map(|_| ())
        } else {
            Ok(())
        })
        .and_then(|()| read_bounded(path).map_err(|e| format!("config {}: {e}", path.display())))
        .and_then(|bytes| {
            if self.observed.as_ref() == Some(&bytes) && self.last_error.is_none() {
                return Ok(None);
            }
            self.observed = Some(bytes.clone());
            Config::parse(&bytes, path)
                .map(Some)
                .map_err(|e| format!("config {}: {e}", path.display()))
        });
        match result {
            Err(error) => {
                if self.last_error.as_ref() != Some(&error) {
                    self.pending_error = Some(error.clone());
                    self.last_error = Some(error);
                }
                None
            }
            Ok(None) => None,
            Ok(Some(document)) => {
                self.last_error = None;
                self.pending_error = None;
                if document == self.current {
                    return None;
                }
                self.next_poll = now + Duration::from_millis(document.reload.interval_ms);
                let restart_fields = self.initial.restart_fields(&document);
                self.current = document.clone();
                Some(Candidate {
                    document,
                    restart_fields,
                })
            }
        }
    }
}
/// Sole config write operation, explicitly requested. Publish complete, synced
/// private bytes (0600 on Unix) with a hard link that never replaces a target.
pub fn initialize(path: &Path) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = absolute(path)?;
    // Check before mkdir to avoid creating parents for an existing target.
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(format!(
                "config already exists; refusing to overwrite {}",
                path.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("config {}: {e}", path.display())),
    }
    let parent = path.parent().ok_or("config has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("config directory: {e}"))?;
    let (stage, mut file) = loop {
        let stage = parent.join(format!(
            ".config.toml.init-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // An explicit target may itself use our staging-name pattern.
        if stage.file_name() == path.file_name() {
            continue;
        }
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
            Err(e) => return Err(format!("stage config {}: {e}", path.display())),
        }
    };
    let result = (|| {
        file.write_all(EXAMPLE.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("write config {}: {e}", path.display()))?;
        fs::hard_link(&stage, &path)
            .map_err(|e| format!("create config {} (no overwrite): {e}", path.display()))
    })();
    drop(file);
    let cleanup = fs::remove_file(&stage);
    result?;
    cleanup.map_err(|e| format!("clean up staged config {}: {e}", stage.display()))?;
    Ok(path)
}
