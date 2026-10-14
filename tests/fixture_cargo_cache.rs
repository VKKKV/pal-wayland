// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
mod common;
use common::FixtureCargo;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[test]
fn dedicated_target_selection_and_configuration_partitions() {
    let scratch = Path::new("/authored/scratch");
    assert_eq!(
        common::target_root(None, scratch),
        scratch.join("pal-wayland-test-target")
    );
    assert_eq!(
        common::target_root(Some("/authored/cache".into()), scratch),
        Path::new("/authored/cache")
    );
    assert_eq!(
        common::target_root(Some("relative-cache".into()), scratch),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("relative-cache")
    );
    let key = |args: &[&str], flags: &str| {
        let mut c = Command::new(env!("CARGO"));
        c.args(args).env("RUSTFLAGS", flags);
        common::configuration_key(&c)
    };
    let base = key(&["build", "--offline"], "");
    assert_eq!(base, key(&["test", "--offline", "--quiet"], ""));
    assert_ne!(base, key(&["build", "--features", "window"], ""));
    assert_ne!(base, key(&["build", "--release"], ""));
    assert_ne!(base, key(&["build", "--profile", "player"], ""));
    assert_ne!(base, key(&["build", "--no-default-features"], ""));
    assert_ne!(base, key(&["build"], "-C overflow-checks=yes"));
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Run the helper through its real entry point with process-local environment;
// never mutate the integration test runner's environment during parallel tests.
#[test]
fn rejected_target_child() {
    let Some(project) = std::env::var_os("PAL_FIXTURE_REJECTION_PROJECT") else {
        return;
    };
    let project = PathBuf::from(project);
    let original = fs::read(project.join("Cargo.toml")).unwrap();
    let rejected = std::panic::catch_unwind(|| {
        Command::new(env!("CARGO"))
            .args(["metadata", "--offline", "--no-deps"])
            .current_dir(&project)
            .fixture_output()
    });
    let panic = rejected.expect_err("protected cache root was accepted");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(message.contains("must be separate from"), "{message}");
    assert_eq!(fs::read(project.join("Cargo.toml")).unwrap(), original);
    assert!(!project.join(".fixture-cargo-original-manifest").exists());
}

#[test]
fn player_and_generated_targets_are_rejected_before_writes() {
    let root = common::unique_dir("pal-cache-rejection");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rejected-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let scratch = Scratch(root);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let leaf = format!("rejected-cache-{}", std::process::id());
    let generated = repo.join("generated/native-player-config/project/target");
    let mut cases = vec![
        (generated.join(&leaf), generated.join(&leaf)),
        (
            PathBuf::from("generated/native-player-config/project/target").join(&leaf),
            generated.join(&leaf),
        ),
        (
            repo.join("generated").join(&leaf),
            repo.join("generated").join(&leaf),
        ),
        (
            repo.join("target").join(&leaf),
            repo.join("target").join(&leaf),
        ),
    ];
    #[cfg(unix)]
    {
        let alias = scratch.0.join("repo-alias");
        std::os::unix::fs::symlink(repo, &alias).unwrap();
        cases.push((
            alias
                .join("generated/native-player-config/project/target")
                .join(&leaf),
            generated.join(&leaf),
        ));
    }
    for (configured, real) in cases {
        assert!(
            !real.exists(),
            "regression path already exists: {}",
            real.display()
        );
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rejected_target_child", "--nocapture"])
            .env(common::TARGET_ENV, &configured)
            .env("PAL_FIXTURE_REJECTION_PROJECT", &scratch.0)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "root {}: {child:?}",
            configured.display()
        );
        assert!(
            !real.exists(),
            "rejected cache root was created: {}",
            real.display()
        );
    }
}

#[test]
fn same_name_authored_fixtures_reuse_dependencies_but_keep_their_own_executable() {
    let root = PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
        "pal-cache-regression-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let scratch = Scratch(root);
    let mut fixtures = Vec::new();
    for exit in [0u8, 7] {
        let dir = scratch.0.join(format!("fixture-{exit}"));
        fs::create_dir(&dir).unwrap();
        let mut mz = vec![0; 544];
        for (i, word) in [
            0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
        ]
        .into_iter()
        .enumerate()
        {
            mz[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
        }
        mz[32..37].copy_from_slice(&[0xb8, exit, 0x4c, 0xcd, 0x21]);
        let exe = dir.join("guest.exe");
        fs::write(&exe, mz).unwrap();
        let project = dir.join("generated");
        let lifted = Command::new(env!("CARGO_BIN_EXE_pal-wayland-lift"))
            .arg(&exe)
            .arg(&project)
            .output()
            .unwrap();
        assert!(lifted.status.success(), "{lifted:?}");
        fixtures.push((exit, exe, project));
    }
    // Warm once so the following independent same-name roots must reuse runtime
    // dependencies, rather than silently rebuilding a fresh target per fixture.
    let warm = Command::new(env!("CARGO"))
        .args(["build", "--offline", "-vv"])
        .current_dir(&fixtures[0].2)
        .fixture_output()
        .unwrap();
    assert!(warm.status.success(), "{warm:?}");
    let manifest_path = fixtures[0].2.join("Cargo.toml");
    let warmed_manifest = fs::read(&manifest_path).unwrap();
    let original: toml::Table =
        fs::read_to_string(fixtures[0].2.join(".fixture-cargo-original-manifest"))
            .unwrap()
            .parse()
            .unwrap();
    let mut rewritten: toml::Table = std::str::from_utf8(&warmed_manifest)
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(rewritten["package"]["name"], original["package"]["name"]);
    rewritten["package"]["name"] = original["package"]["name"].clone();
    assert_eq!(
        rewritten, original,
        "manifest rewrite changed nonidentity fields"
    );
    let built = std::thread::scope(|scope| {
        let handles: Vec<_> = fixtures
            .into_iter()
            .map(|(exit, exe, project)| {
                scope.spawn(move || {
                    let build = Command::new(env!("CARGO"))
                        .args(["build", "--offline", "-vv"])
                        .env(
                            "CARGO_BUILD_TARGET",
                            "deliberately-invalid-inherited-target",
                        )
                        .current_dir(&project)
                        .fixture_output()
                        .unwrap();
                    assert!(build.status.success(), "{build:?}");
                    let stderr = String::from_utf8_lossy(&build.stderr);
                    assert!(
                        stderr.contains("Fresh pal-wayland") && stderr.contains("Fresh serde"),
                        "dependencies were not reused: {stderr}"
                    );
                    (exit, exe, project)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    // Repeated calls must not accumulate identity suffixes or lose authored TOML.
    assert_eq!(fs::read(manifest_path).unwrap(), warmed_manifest);
    // Both competing cache builds finish before either fixture snapshot runs.
    for (exit, exe, project) in built {
        let ran = Command::new(project.join("target/debug/pal-wayland"))
            .arg(exe)
            .args(["--headless", "--max-steps", "10"])
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("PIPEWIRE_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains(&format!("guest_exit_code: {exit}")),
            "wrong same-name artifact: {ran:?}"
        );
    }
}
