// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Executes emitted Rust, not merely decoder/codegen string assertions.
mod common;
use common::FixtureCargo;
use std::{fs, path::Path, process::Command};

fn fixture() -> Vec<u8> {
    let mut bytes = vec![0; 160];
    for (i, w) in [0x5a4d_u16, 160, 1, 0, 2, 0, 0xffff, 0, 128, 0, 0, 0, 28, 0]
        .into_iter()
        .enumerate()
    {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    let mut code = vec![
        0xb8, 1, 0, 0xbb, 2, 0, 0x01, 0xd8, // AX=1+2
        0x3d, 3, 0, 0x75, 0, // CMP/JNE failure
        0xe8, 0, 0, 0x3d, 4, 0, 0x75, 0, // CALL increment/CMP/JNE
        0x6a, 0, 0x9d, 0x9c, 0x58, // PUSH 0/POPF/PUSHF/POP AX
        0xa9, 2, 0, 0x74, 0, // TEST AX,2/JZ failure
        0x0e, 0x1f, // PUSH CS/POP DS
        0xba, 0, 0, // DX=handler (fix below)
        0xb8, 0x12, 0x25, 0xcd, 0x21, // install INT12 handler
        0xcd, 0x12, // must enter guest handler, NOT BIOS memory-size shim
        0x3d, 0x34, 0x12, 0x75, 0, // AX==1234 or failure
        0xb8, 0, 0x4c, 0xcd, 0x21,
    ];
    let failure = code.len();
    code.extend([0xb8, 1, 0x4c, 0xcd, 0x21]);
    let increment = code.len();
    code.extend([0x40, 0xc3]);
    let handler = code.len();
    code.extend([0xb8, 0x34, 0x12, 0xcf]);
    for operand in [12, 20, 30, 47] {
        code[operand] = (failure - (operand + 1)) as u8;
    }
    code[14..16].copy_from_slice(&((increment - 16) as u16).to_le_bytes());
    code[34..36].copy_from_slice(&(handler as u16).to_le_bytes());
    bytes[32..32 + code.len()].copy_from_slice(&code);
    bytes
}
fn command(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(args[0])
        .args(&args[1..])
        .current_dir(cwd)
        .fixture_output()
        .expect("run pipeline tool")
}
#[test]
fn generate_build_execute_and_reject_overwrite_unknown_and_modified_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = common::unique_dir("pal-aot-fixture");
    fs::create_dir(&dir).unwrap();
    let exe = dir.join("fixture.exe");
    fs::write(&exe, fixture()).unwrap();
    let generated = dir.join("generated");
    let args = [
        env!("CARGO_BIN_EXE_pal-wayland-lift"),
        exe.to_str().unwrap(),
        generated.to_str().unwrap(),
        "--seed",
        "1010:003c",
    ];
    let output = command(&args, root);
    assert!(output.status.success(), "{output:?}");
    let before = fs::read(generated.join("src/translated.rs")).unwrap();
    let duplicate = command(&args, root);
    assert!(!duplicate.status.success());
    assert_eq!(
        before,
        fs::read(generated.join("src/translated.rs")).unwrap()
    );
    let build = command(&["cargo", "build", "--offline"], &generated);
    assert!(build.status.success(), "{build:?}");
    let tree = command(
        &["cargo", "tree", "--offline", "-e", "features"],
        &generated,
    );
    assert!(tree.status.success());
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(!tree.contains("iced-x86"));
    assert!(!tree.contains("lazy_static"));
    let binary = generated.join("target/debug/pal-wayland");
    let output = command(&[binary.to_str().unwrap(), exe.to_str().unwrap()], &dir);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("guest_exit_code: 0"));
    let output = command(
        &[
            binary.to_str().unwrap(),
            exe.to_str().unwrap(),
            "--set",
            "ip=70",
        ],
        &dir,
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown"),
        "{output:?}"
    );
    let mut modified = fixture();
    modified[33] = 7;
    let modified_exe = dir.join("modified.exe");
    fs::write(&modified_exe, &modified).unwrap();
    let output = command(
        &[binary.to_str().unwrap(), modified_exe.to_str().unwrap()],
        &dir,
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("code changed"),
        "{output:?}"
    );
    // Opt-in MOV immediate may change, while opcode and semantics remain fixed.
    let mutable_dir = dir.join("mutable-generated");
    let made = command(
        &[
            env!("CARGO_BIN_EXE_pal-wayland-lift"),
            exe.to_str().unwrap(),
            mutable_dir.to_str().unwrap(),
            "--seed",
            "1010:003c",
            "--mutable-immediate",
            "1010:0000",
        ],
        root,
    );
    assert!(made.status.success(), "{made:?}");
    let build = command(&["cargo", "build", "--offline"], &mutable_dir);
    assert!(build.status.success(), "{build:?}");
    let changed = command(
        &[
            mutable_dir
                .join("target/debug/pal-wayland")
                .to_str()
                .unwrap(),
            modified_exe.to_str().unwrap(),
            "--stop",
            "1010:0003",
        ],
        &dir,
    );
    assert!(changed.status.success(), "{changed:?}");
    assert!(String::from_utf8_lossy(&changed.stdout).contains("\"ax\":7"));
    modified[32] = 0xbb;
    let opcode = dir.join("bad-opcode.exe");
    fs::write(&opcode, &modified).unwrap();
    let changed = command(
        &[
            mutable_dir
                .join("target/debug/pal-wayland")
                .to_str()
                .unwrap(),
            opcode.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("code changed"));

    // Original fixture for software INT3D -> MOV AX,AX self-patch. The guest
    // handler itself performs the rewrite and IRET; no host handler shortcut.
    let mut sync = fixture();
    sync[32..].fill(0x90);
    let prefix = [
        0x0e, 0x1f, 0xba, 0x50, 0, 0xb8, 0x3d, 0x25, 0xcd, 0x21, 0xcd, 0x3d, 0xb8, 0, 0x4c, 0xcd,
        0x21,
    ];
    sync[32..32 + prefix.len()].copy_from_slice(&prefix);
    let handler = [
        0x55, 0x8b, 0xec, 0x1e, 0x56, 0xc5, 0x76, 2, 0x83, 0xee, 2, 0x89, 0x76, 2, 0xc7, 4, 0x89,
        0xc0, 0x5e, 0x1f, 0x5d, 0xcf,
    ];
    sync[32 + 0x50..32 + 0x50 + handler.len()].copy_from_slice(&handler);
    let sync_exe = dir.join("sync.exe");
    fs::write(&sync_exe, &sync).unwrap();
    let sync_dir = dir.join("sync-generated");
    let made = command(
        &[
            env!("CARGO_BIN_EXE_pal-wayland-lift"),
            sync_exe.to_str().unwrap(),
            sync_dir.to_str().unwrap(),
            "--seed",
            "1010:0050",
            "--fpu-macros",
            "borland",
        ],
        root,
    );
    assert!(made.status.success(), "{made:?}");
    let build = command(&["cargo", "build", "--offline"], &sync_dir);
    assert!(build.status.success(), "{build:?}");
    let ran = command(
        &[
            sync_dir.join("target/debug/pal-wayland").to_str().unwrap(),
            sync_exe.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(ran.status.success(), "{ran:?}");
    // Shared CLI post-processing must happen before the window early-return.
    // No display is opened: this build is decoder-free/headless, and the absent
    // CD mount must fail before either window initialization or feature errors.
    let ran = command(
        &[
            sync_dir.join("target/debug/pal-wayland").to_str().unwrap(),
            sync_exe.to_str().unwrap(),
            "--window",
            "yes",
            "--cd-header",
            "legacy13",
        ],
        &dir,
    );
    assert!(!ran.status.success());
    assert!(
        String::from_utf8_lossy(&ran.stderr).contains("legacy CD header requires a CD mount"),
        "{ran:?}"
    );
    // Deterministic timing fixture is explicit, never the normal runtime clock.
    let ran = command(
        &[
            sync_dir.join("target/debug/pal-wayland").to_str().unwrap(),
            sync_exe.to_str().unwrap(),
            "--virtual-step-ns",
            "1000",
        ],
        &dir,
    );
    assert!(ran.status.success(), "{ran:?}");
    // Original OUTSB DAC program, exercising byte width and full REP completion.
    let mut dac_exe = fixture();
    dac_exe[32..].fill(0x90);
    let code = [
        0x0e, 0x1f, 0xba, 0xc8, 3, 0xb0, 1, 0xee, 0x42, 0xbe, 0x60, 0, 0xb9, 3, 0, 0xf3, 0x6e,
        0xb8, 0, 0x4c, 0xcd, 0x21,
    ];
    dac_exe[32..32 + code.len()].copy_from_slice(&code);
    dac_exe[32 + 0x60..32 + 0x63].copy_from_slice(&[63, 0, 0]);
    let dac_path = dir.join("dac.exe");
    fs::write(&dac_path, &dac_exe).unwrap();
    let dac_dir = dir.join("dac-generated");
    let made = command(
        &[
            env!("CARGO_BIN_EXE_pal-wayland-lift"),
            dac_path.to_str().unwrap(),
            dac_dir.to_str().unwrap(),
        ],
        root,
    );
    assert!(made.status.success(), "{made:?}");
    let build = command(&["cargo", "build", "--offline"], &dac_dir);
    assert!(build.status.success(), "{build:?}");
    let ran = command(
        &[
            dac_dir.join("target/debug/pal-wayland").to_str().unwrap(),
            dac_path.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(ran.status.success(), "{ran:?}");
    // ENTER nesting zero must preserve BP/SP through LEAVE. This is emitted
    // and executed: decoder recognition alone does not certify stack behavior.
    let mut enter_exe = fixture();
    enter_exe[32..].fill(0x90);
    let code = [
        0x89, 0xe3, 0xbd, 0x34, 0x12, 0xc8, 0x10, 0, 0, 0xc9, 0x39, 0xdc, 0x75, 0x0b, 0x81, 0xfd,
        0x34, 0x12, 0x75, 5, 0xb8, 0, 0x4c, 0xcd, 0x21, 0xb8, 1, 0x4c, 0xcd, 0x21,
    ];
    enter_exe[32..32 + code.len()].copy_from_slice(&code);
    let enter_path = dir.join("enter.exe");
    fs::write(&enter_path, &enter_exe).unwrap();
    let enter_dir = dir.join("enter-generated");
    let made = command(
        &[
            env!("CARGO_BIN_EXE_pal-wayland-lift"),
            enter_path.to_str().unwrap(),
            enter_dir.to_str().unwrap(),
        ],
        root,
    );
    assert!(made.status.success(), "{made:?}");
    let build = command(&["cargo", "build", "--offline"], &enter_dir);
    assert!(build.status.success(), "{build:?}");
    let binary = enter_dir.join("target/debug/pal-wayland");
    let trace = dir.join("enter-trace.jsonl");
    let ran = command(
        &[
            binary.to_str().unwrap(),
            enter_path.to_str().unwrap(),
            "--trace",
            trace.to_str().unwrap(),
            "--trace-after",
            "10000",
            "--key-event",
            "0:1c:0d:down",
        ],
        &dir,
    );
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(fs::metadata(&trace).unwrap().len(), 0);
    // A different opcode mutation at the macro entry cannot use the whitelist.
    sync[32 + 10] = 0x88;
    let bad = dir.join("sync-bad.exe");
    fs::write(&bad, &sync).unwrap();
    let ran = command(
        &[
            sync_dir.join("target/debug/pal-wayland").to_str().unwrap(),
            bad.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(!ran.status.success());
    assert!(String::from_utf8_lossy(&ran.stderr).contains("code changed"));
}
