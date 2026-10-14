// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "profiling")]
use pal_wayland::host_profile::{PresentationClock, Span};
use std::process::Command;

#[cfg(all(feature = "analysis", feature = "window"))]
mod common;

#[cfg(all(feature = "analysis", feature = "window"))]
#[test]
fn generated_template_profiles_guest_and_config_without_starting_backends() {
    use common::FixtureCargo;
    use pal_wayland::lift::{Address, emit_rust, recover};
    use std::fs;
    let root = common::unique_dir("pal-profile-template");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), format!(
        "[package]\nname='pal-profile-template-test'\nversion='0.0.0'\nedition='2024'\n[features]\nwindow=['pal-wayland/window']\nprofiling=['pal-wayland/profiling']\n[dependencies]\npal-wayland={{path={:?},default-features=false}}\n", env!("CARGO_MANIFEST_DIR"))).unwrap();
    // Original authored DOS exit: compile the real emitter and production
    // template, not a substitute guest executor or presentation adapter.
    let mut mz = vec![0u8; 544];
    for (i, word) in [
        0x5a4du16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        mz[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    mz[32..37].copy_from_slice(&[0xb8, 0, 0x4c, 0xcd, 0x21]);
    let machine = pal_wayland::runtime::Machine::from_mz(&mz, 0x1010).unwrap();
    let graph = recover(
        machine.memory.as_bytes(),
        &[Address { cs: 0x1010, ip: 0 }],
        4,
    )
    .unwrap();
    fs::write(root.join("guest.exe"), &mz).unwrap();
    fs::write(root.join("src/translated.rs"), emit_rust(&graph).unwrap()).unwrap();
    let mut template = include_str!("../templates/native_main.rs.in")
        .replace("@ENTRY_CS@", "0x1010")
        .replace("@ENTRY_IP@", "0");
    template.push_str(r#"
#[cfg(test)] mod profile_caller {
    use super::*;
    use pal_wayland::wayland::{GameHost, keymap::KeyRouter};
    #[test] fn real_windowhost_tick_profiles_guest_and_config() {
        let p=std::path::PathBuf::from("profile.toml");
        fs::write(&p,"version=1\n[reload]\ninterval_ms=100\n").unwrap();
        let initial=pal_wayland::player_config::InitialConfig::load(Some(&p),true).unwrap();
        let mut host=WindowHost {
            m:Machine::from_mz(&fs::read("guest.exe").unwrap(),0x1010).unwrap(),
            limit:100,virtual_step_ns:Some(1),audio_config:None,realtime:None,
            controls:ConfigRuntime::new(&initial),keys:KeyRouter::new(initial.document.resolved_keymap()),
        };
        for _ in 0..240 { assert!(!host.tick().unwrap()); }
        assert_eq!(host.m.exit_code,Some(0));
        assert_eq!(host.m.steps,2);
    }
}
"#);
    fs::write(root.join("src/main.rs"), template).unwrap();
    let output = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args(["test", "--offline", "--features", "window,profiling"])
        .env("RUST_TEST_NOCAPTURE", "1")
        .env("PAL_PROFILE", "1")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("PIPEWIRE_REMOTE")
        .env_remove("PIPEWIRE_RUNTIME_DIR")
        .fixture_output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture={} {output:?}",
        root.display()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
    for stage in ["guest_execution", "config_reload"] {
        assert!(
            stderr.contains(&format!("profile span={stage} count=120 ")),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("profile span={stage} count=240 ")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("renderer="));
    assert!(!stderr.contains("audio-output"));
    fs::remove_dir_all(root).unwrap();
}

// Spawn the real collector in isolated processes: never mutate the parallel
// Cargo runner's PAL_PROFILE or share its OnceLock/aggregate counters.
#[test]
fn profile_child_probe() {
    if std::env::var_os("PAL_PROFILE_CHILD").is_none() {
        return;
    }
    let mut cadence = PresentationClock::default();
    cadence.observe(true); // initial seed; not an interval
    for _ in 0..240 {
        for stage in [
            "tick",
            "frame",
            "execution",
            "config_reload",
            "frame_decode",
            "swapchain_acquire",
            "gpu_cpu_upload",
            "queue_submit",
            "present_cpu",
            "softbuffer_cpu_scale",
            "softbuffer_cpu_copy",
        ] {
            drop(Span::new(stage));
        }
        cadence.observe(false); // timeout/error/skipped draw is not a sample
        cadence.observe(true);
        drop(Span::new("not-a-profile-stage"));
    }
}

#[test]
fn profiling_is_explicit_opt_in_and_output_is_bounded() {
    let binary = std::env::current_exe().unwrap();
    for value in [None, Some("0"), Some("true"), Some("1")] {
        let mut command = Command::new(&binary);
        command
            .args(["--exact", "profile_child_probe", "--nocapture"])
            .env("PAL_PROFILE_CHILD", "1")
            .env_remove("PAL_PROFILE");
        if let Some(value) = value {
            command.env("PAL_PROFILE", value);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !stdout.contains("profile span="),
            "diagnostics must stay off stdout"
        );
        if value != Some("1") {
            assert!(stderr.is_empty(), "opt-out {value:?}: {stderr}");
            continue;
        }
        let lines: Vec<_> = stderr.lines().collect();
        assert_eq!(lines.len(), 24, "two reports per 12 fixed stages: {stderr}");
        for stage in [
            "tick",
            "frame",
            "guest_execution",
            "config_reload",
            "frame_decode",
            "swapchain_acquire",
            "gpu_cpu_upload",
            "queue_submit",
            "present_cpu",
            "presentation_interval",
            "softbuffer_cpu_scale",
            "softbuffer_cpu_copy",
        ] {
            let reports: Vec<_> = lines
                .iter()
                .filter(|line| line.starts_with(&format!("profile span={stage} ")))
                .collect();
            assert_eq!(reports.len(), 2, "{stage}: {stderr}");
            assert!(reports[0].contains(" count=120 "));
            assert!(reports[1].contains(" count=240 "));
            for line in reports {
                for field in [
                    "total_us=",
                    "max_us=",
                    "p50_bucket_upper_us=",
                    "p95_bucket_upper_us=",
                    "p99_bucket_upper_us=",
                    "quantiles=log2_bucket_upper_bound",
                    "scope=host_cpu_not_gpu_completion",
                ] {
                    assert!(line.contains(field), "missing {field}: {line}");
                }
            }
        }
    }
}
