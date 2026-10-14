// SPDX-License-Identifier: GPL-3.0-or-later
//! Nested Cargo builds never inherit the player's target directory. Dependencies
//! persist in a dedicated cache; executables are snapshotted into each fixture.
#![allow(dead_code)]
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub const TARGET_ENV: &str = "PAL_NATIVE_TEST_TARGET_DIR";

pub fn unique_dir(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required for fixtures")).join(format!(
        "{label}-{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

pub trait FixtureCargo {
    fn fixture_output(&mut self) -> std::io::Result<Output>;
    fn fixture_output_to(&mut self, artifacts: &Path) -> std::io::Result<Output>;
}
impl FixtureCargo for Command {
    fn fixture_output(&mut self) -> std::io::Result<Output> {
        output(self)
    }
    fn fixture_output_to(&mut self, artifacts: &Path) -> std::io::Result<Output> {
        output_to(self, artifacts)
    }
}

pub fn target_root(explicit: Option<OsString>, scratch: &Path) -> PathBuf {
    match explicit {
        Some(value) => {
            assert!(!value.is_empty(), "{TARGET_ENV} must not be empty");
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
            }
        }
        None => scratch.join("pal-wayland-test-target"),
    }
}

// Resolve existing ancestors without creating the requested cache. This also
// checks symlink aliases when the final cache directory does not exist yet.
fn resolve_cache_path(path: &Path, follow_links: bool) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            component => {
                resolved.push(component.as_os_str());
                if follow_links {
                    match fs::canonicalize(&resolved) {
                        Ok(path) => resolved = path,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn validate_target_root(root: &Path) -> std::io::Result<PathBuf> {
    let lexical = resolve_cache_path(root, false)?;
    let resolved = resolve_cache_path(root, true)?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let active_target = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    for protected in [
        // Generated artifacts include player builds outside the outer target.
        Some(repo.join("generated")),
        Some(repo.join("target")),
        std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        active_target,
    ]
    .into_iter()
    .flatten()
    {
        for protected in [
            resolve_cache_path(&protected, false)?,
            resolve_cache_path(&protected, true)?,
        ] {
            assert!(
                !lexical.starts_with(&protected) && !resolved.starts_with(&protected),
                "{TARGET_ENV} must be separate from player/generated/outer Cargo target: {}",
                protected.display()
            );
        }
    }
    Ok(resolved)
}

// Partition feature/profile/compiler configurations, not fixture paths. Cargo
// fingerprints still invalidate dependencies when their source/toolchain changes.
pub fn configuration_key(command: &Command) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let args: Vec<_> = command.get_args().collect();
    let mut config = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = args[i];
        if ["--features", "-F", "--profile", "--target"]
            .iter()
            .any(|v| arg == OsStr::new(v))
        {
            config.push(arg);
            i += 1;
            config.push(*args.get(i).expect("missing Cargo configuration value"));
        } else if arg == "--release"
            || arg == "--no-default-features"
            || arg == "--all-features"
            || arg.to_string_lossy().starts_with("--features=")
            || arg.to_string_lossy().starts_with("--profile=")
            || arg.to_string_lossy().starts_with("--target=")
        {
            config.push(arg);
        }
        i += 1;
    }
    config.hash(&mut hash);
    for name in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_PROFILE_DEV_OPT_LEVEL",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
    ] {
        name.hash(&mut hash);
        let value = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value.map(OsStr::to_os_string))
            .unwrap_or_else(|| std::env::var_os(name));
        value.hash(&mut hash);
    }
    format!("v1-{:016x}", hash.finish())
}

pub fn output(command: &mut Command) -> std::io::Result<Output> {
    let program = command.get_program();
    let cargo = program == OsStr::new(env!("CARGO"))
        || Path::new(program).file_stem() == Some(OsStr::new("cargo"));
    if !cargo {
        return command.output();
    }
    let project = command
        .get_current_dir()
        .expect("fixture Cargo requires current_dir")
        .to_path_buf();
    output_to(command, &project.join("target"))
}

pub fn output_to(command: &mut Command, artifacts: &Path) -> std::io::Result<Output> {
    let project = command
        .get_current_dir()
        .expect("fixture Cargo requires current_dir")
        .to_path_buf();
    let args: Vec<_> = command.get_args().map(OsStr::to_os_string).collect();
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--target-dir" || arg.to_string_lossy().starts_with("--target-dir=")),
        "fixture target selection belongs to tests/common"
    );
    // These tests produce host executables, not inherited cross-compilation output.
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--target" || arg.to_string_lossy().starts_with("--target=")),
        "fixture builds must target the host"
    );
    let scratch = std::env::var_os("TMPDIR").expect("TMPDIR required for fixture Cargo cache");
    let root = target_root(std::env::var_os(TARGET_ENV), Path::new(&scratch));
    let root = validate_target_root(&root)?;
    fs::create_dir_all(&root)?;
    let root = validate_target_root(&fs::canonicalize(root)?)?;
    let target = root.join(configuration_key(command));
    fs::create_dir_all(&target)?;
    // Cargo releases its own lock before output() returns. Hold a separate lock
    // across build AND snapshot so another same-name fixture cannot replace ELF.
    // File::lock is available on the repository's Rust 1.97 baseline; no flock CLI.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target.join(".fixture-build.lock"))?;
    lock.lock()?;
    command
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_BUILD_BUILD_DIR");
    if args
        .first()
        .is_some_and(|arg| arg == "build" || arg == "run" || arg == "test")
    {
        command.arg("--target-dir").arg(&target);
    }
    // Cargo's top-level binary name is not part of its package-path fingerprint.
    // Even a Fresh build can leave another same-name package's top-level ELF in
    // place. Give fixture packages AND binaries path-unique Cargo identities;
    // snapshot back to the original names expected by the tests.
    let original_path = project.join(".fixture-cargo-original-manifest");
    if !original_path.exists() {
        fs::copy(project.join("Cargo.toml"), &original_path)?;
    }
    let original = fs::read_to_string(original_path)?;
    let manifest: toml::Table = original.parse().expect("fixture manifest");
    use std::hash::{Hash, Hasher};
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    fs::canonicalize(&project)?.hash(&mut identity);
    let suffix = format!("-fixture-{:016x}", identity.finish());
    let original_name = manifest["package"]["name"]
        .as_str()
        .expect("fixture package name");
    // Preserve the authored TOML and dependency/features/profile tables exactly.
    // Current fixture manifests use one default src/main.rs bin, no custom bins.
    assert!(
        !manifest.contains_key("bin"),
        "custom fixture bin manifests need explicit snapshot mapping"
    );
    let mut normalized = original.clone();
    let mut replaced = false;
    for quote in ['\"', '\''] {
        let token = format!("{quote}{original_name}{quote}");
        if normalized.contains(&token) {
            assert_eq!(
                normalized.matches(&token).count(),
                1,
                "ambiguous fixture package name"
            );
            normalized =
                normalized.replacen(&token, &format!("{quote}{original_name}{suffix}{quote}"), 1);
            replaced = true;
            break;
        }
    }
    assert!(
        replaced,
        "fixture package name must be a quoted TOML string"
    );
    let unique = format!("{original_name}{suffix}");
    let names = if project.join("src/main.rs").exists() {
        vec![(original_name.to_owned(), unique)]
    } else {
        Vec::new()
    };
    if fs::read_to_string(project.join("Cargo.toml"))? != normalized {
        fs::write(project.join("Cargo.toml"), normalized)?;
    }
    let result = command.output()?;
    if result.status.success() && args.first().is_some_and(|arg| arg == "build") {
        let mut profile = if args.iter().any(|arg| arg == "--release") {
            "release".to_owned()
        } else {
            "debug".to_owned()
        };
        for pair in args.windows(2) {
            if pair[0] == "--profile" {
                profile = pair[1].to_string_lossy().into_owned();
            }
        }
        for arg in &args {
            if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--profile=")) {
                profile = value.to_owned();
            }
        }
        if profile == "dev" {
            profile = "debug".to_owned();
        }
        let destination = artifacts.join(&profile);
        fs::create_dir_all(&destination)?;
        for (original, unique) in names {
            let name = format!("{original}{}", std::env::consts::EXE_SUFFIX);
            let source = target
                .join(&profile)
                .join(format!("{unique}{}", std::env::consts::EXE_SUFFIX));
            let staged = destination.join(format!(".{name}.snapshot"));
            fs::copy(source, &staged)?;
            fs::rename(staged, destination.join(name))?;
        }
    }
    // run/test execute under this lock too; release on every return/unwind.
    drop(lock);
    Ok(result)
}
