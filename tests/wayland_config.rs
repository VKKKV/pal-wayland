// SPDX-License-Identifier: GPL-3.0-or-later
//! These tests never connect to the user's compositor. Every event-loop attempt
//! runs in a new subprocess with inherited Wayland/X11 endpoints removed.
#![cfg(feature = "window")]

use pal_wayland::wayland::{GameHost, KeyCode, run};
use std::process::Command;

struct NeverRun;
impl GameHost for NeverRun {
    fn tick(&mut self) -> Result<bool, String> {
        panic!("no-display initialization must not execute the native core")
    }
    fn frame(&self) -> Result<pal_wayland::video::IndexedFrame, String> {
        panic!("no-display initialization must not request or invent a frame")
    }
    fn key(&mut self, _: KeyCode, _: bool) {
        panic!("no-display initialization must not deliver input")
    }
}

#[test]
fn no_display_returns_error_without_running_host() {
    const CHILD: &str = "PAL_WAYLAND_NO_DISPLAY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // Fail closed even if the child marker was accidentally inherited from
        // an unrelated runner. Never allow this test to use a live endpoint.
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
        assert!(std::env::var_os("WAYLAND_SOCKET").is_none());
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            assert_eq!(std::fs::read_dir(runtime).unwrap().count(), 0);
        }
        let error = run(NeverRun).expect_err("Wayland must not work without a compositor");
        assert!(
            error.starts_with("Wayland initialization failed (no X11 fallback):"),
            "{error}"
        );
        println!("verified no-display error: {error}");
        return;
    }

    // winit permits only one event loop per process, including failed attempts.
    // Isolate the environment with Command instead of unsafe set_var/remove_var
    // in a multithreaded Rust test process.
    let runtime =
        std::env::temp_dir().join(format!("pal-wayland-no-display-{}", std::process::id()));
    std::fs::create_dir(&runtime).expect("create empty isolated runtime directory");
    for missing_runtime in [false, true] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "no_display_returns_error_without_running_host",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("XDG_RUNTIME_DIR");
        if !missing_runtime {
            // A bogus X11 endpoint must not change the forced Wayland failure.
            command
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("DISPLAY", ":987654");
        }
        let output = command.output().expect("run isolated no-display test");
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("verified no-display error:"));
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    std::fs::remove_dir(runtime).expect("remove isolated runtime directory");
}
