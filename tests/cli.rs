// SPDX-License-Identifier: GPL-3.0-or-later
use std::{fs::OpenOptions, io::Write, process::Command};

#[test]
fn authored_mz_cli_success_and_error_sources() {
    // Exclusive creation protects existing files and concurrent test runs. The
    // fixture is left in TMPDIR for the managed scratch directory's cleanup.
    let (path, mut file) = (0..1000)
        .find_map(|attempt| {
            let path = std::env::temp_dir().join(format!(
                "pal-cli-fixture-{}-{attempt} with spaces.mz",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => Some((path, file)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(err) => panic!("create fixture: {err}"),
            }
        })
        .expect("unique scratch fixture");
    // Original data-only fixture: 32-byte header, 64-byte image, no relocation.
    let mut data = vec![0; 96];
    for (i, value) in [0x5a4d_u16, 96, 1, 0, 2, 0, 0xffff, 0, 64, 0, 0, 0, 28, 0]
        .into_iter()
        .enumerate()
    {
        data[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }
    file.write_all(&data).unwrap();
    drop(file);
    let invoke = |command: &str, segment: Option<&str>| {
        let mut process = Command::new(env!("CARGO_BIN_EXE_pal-wayland-tool"));
        process.arg(command).arg(&path);
        if let Some(segment) = segment {
            process.arg(segment);
        }
        process.current_dir(std::env::temp_dir()).output().unwrap()
    };
    let output = invoke("inspect", None);
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    let report = String::from_utf8(output.stdout).unwrap();
    for line in [
        "declared_file_bytes: 96",
        "header_bytes: 32",
        "image_bytes: 64",
        "entry_file_offset: 0x20",
    ] {
        assert!(report.lines().any(|got| got == line), "missing {line}");
    }
    let mut previous = None;
    for segment in ["4096", "0x1000", "0X1000"] {
        let output = invoke("load-check", Some(segment));
        assert!(output.status.success(), "{segment}: {output:?}");
        assert!(output.stderr.is_empty());
        let report = String::from_utf8(output.stdout).unwrap();
        for line in [
            "load_segment: 0x1000",
            "allocation_bytes: 64",
            "entry_loaded: 1000:0000",
            "stack_loaded: 1000:0040",
            "applied_relocations: 0",
        ] {
            assert!(report.lines().any(|got| got == line), "missing {line}");
        }
        if let Some(previous) = &previous {
            assert_eq!(&report, previous);
        }
        previous = Some(report);
    }
    for segment in ["0x10000", "-1", "wat", "", "0x", "65536"] {
        let output = invoke("load-check", Some(segment));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("pal-wayland-tool: invalid load segment")
        );
    }
    let output = invoke("load-check", Some("0xffff"));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("pal-wayland-tool: load: allocation")
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        data,
        "CLI must not change input"
    );
}

fn tool(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pal-wayland-tool"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("execute pal-wayland-tool")
}

#[test]
fn help_succeeds_without_assets() {
    let output = tool(&["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("no guest instructions"));
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_arguments_fail_with_stderr_only() {
    for args in [
        vec![],
        vec!["unknown"],
        vec!["inspect"],
        vec!["inspect", "Cargo.toml", "extra"],
        vec!["load-check", "Cargo.toml"],
    ] {
        let output = tool(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn missing_or_invalid_input_fails_without_success_output() {
    for path in ["Cargo.toml", "assets/does-not-exist-mz-test.exe"] {
        let output = tool(&["inspect", path]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
