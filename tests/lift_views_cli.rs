// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Original synthetic MZ only. Exercise the emitted CLI and decoder-free runtime.
mod common;
use common::FixtureCargo;
use pal_wayland::{
    lift::{Address, recover_in_ranges},
    runtime::Machine,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    dir: PathBuf,
    exe: PathBuf,
    original: Vec<u8>,
    hooked: PathBuf,
    interrupt: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn call(code: &mut Vec<u8>, target: u16) {
    let next = 0x80 + code.len() as u16 + 3;
    code.push(0xe8);
    code.extend_from_slice(&target.wrapping_sub(next).to_le_bytes());
}
fn fixture() -> Fixture {
    let dir = common::unique_dir("pal-wayland-lift-views");
    fs::create_dir(&dir).unwrap();
    let mut body = vec![0x90; 256];
    body[0] = 0xc3;
    body[5] = 0xc3;
    body[0x40..0x42].copy_from_slice(&[0x43, 0xcb]); // INC BX; RETF
    body[0x60..0x63].copy_from_slice(&[0xcd, 0x00, 0xc3]);
    body[0xf0..0xf6].copy_from_slice(&[0, 0, 0x40, 0, 0x60, 0]);
    let mut driver = Vec::new();
    call(&mut driver, 0); // Original RET.
    driver.extend_from_slice(&[0x2e, 0xc7, 0x06, 0, 0, 0x9a, 0x40]);
    driver.extend_from_slice(&[0x2e, 0xc7, 0x06, 2, 0, 0, 0x10]);
    driver.extend_from_slice(&[0x2e, 0xc6, 0x06, 4, 0, 0x10]);
    call(&mut driver, 0); // Guest-installed far CALL -> INC BX -> RETF -> RET.
    driver.extend_from_slice(&[0x2e, 0xc6, 0x06, 0, 0, 0xc3]);
    call(&mut driver, 0); // Restored RET, now with different trailing bytes.
    driver.extend_from_slice(&[0x2e, 0xc6, 0x06, 0x61, 0, 0x12]);
    call(&mut driver, 0x60); // Exact CD12 alternative, not any INT immediate.
    driver.extend_from_slice(&[0xb8, 0, 0x4c, 0xcd, 0x21]);
    body[0x80..0x80 + driver.len()].copy_from_slice(&driver);
    let mut mz = vec![0; 32];
    mz[..2].copy_from_slice(b"MZ");
    for (offset, word) in [
        (2, 288u16),
        (4, 1),
        (8, 2),
        (10, 0x100),
        (12, 0xffff),
        (16, 0x1000),
        (20, 0x80),
        (24, 28),
    ] {
        mz[offset..offset + 2].copy_from_slice(&word.to_le_bytes());
    }
    mz.extend_from_slice(&body);
    let exe = dir.join("original.exe");
    fs::write(&exe, &mz).unwrap();
    let original = Machine::from_mz(&mz, 0x1010)
        .unwrap()
        .memory
        .as_bytes()
        .to_vec();
    let mut hook = original.clone();
    hook[0x10100..0x10105].copy_from_slice(&[0x9a, 0x40, 0, 0x10, 0x10]);
    let hooked = dir.join("hooked.mem");
    fs::write(&hooked, &hook).unwrap();
    let mut int = original.clone();
    int[0x10161] = 0x12;
    let interrupt = dir.join("int12.mem");
    fs::write(&interrupt, &int).unwrap();
    Fixture {
        dir,
        exe,
        original,
        hooked,
        interrupt,
    }
}
fn run(program: &str, args: &[&str], dir: &Path) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .fixture_output()
        .unwrap()
}
fn lift(f: &Fixture, output: &Path, args: &[&str]) -> Output {
    let mut all = vec![f.exe.to_str().unwrap(), output.to_str().unwrap()];
    all.extend_from_slice(args);
    run(env!("CARGO_BIN_EXE_pal-wayland-lift"), &all, &f.dir)
}
fn reject(f: &Fixture, name: &str, args: &[&str], diagnostic: &str) {
    let out = f.dir.join(name);
    let result = lift(f, &out, args);
    assert!(!result.status.success(), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(diagnostic),
        "{result:?}"
    );
    assert!(!out.exists(), "rejected analysis must not create output");
}
#[test]
fn generated_cli_executes_guest_ret_call_and_exact_interrupt_views() {
    let f = fixture();
    let out = f.dir.join("generated");
    let hook = f.hooked.to_str().unwrap();
    let int = f.interrupt.to_str().unwrap();
    let args = [
        "--near-table",
        "1010:00f0:3",
        "--static-view",
        hook,
        "--view-seed",
        "1010:0000",
        "--view-range",
        "10100:10106",
        "--view-range",
        "10140:10142",
        "--static-view",
        hook,
        "--view-seed",
        "1010:0000",
        "--view-range",
        "0x10100:0x10106",
        "--view-range",
        "10140:10142",
        "--static-view",
        int,
        "--view-seed",
        "1010:0060",
        "--view-range",
        "10160:10163",
    ];
    let made = lift(&f, &out, &args);
    assert!(made.status.success(), "{made:?}");
    // Independently calculate deduplicated counts, including repeated views.
    let original = recover_in_ranges(
        &f.original,
        &[
            Address {
                cs: 0x1010,
                ip: 0x80,
            },
            Address { cs: 0x1010, ip: 0 },
            Address {
                cs: 0x1010,
                ip: 0x40,
            },
            Address {
                cs: 0x1010,
                ip: 0x60,
            },
        ],
        100_000,
        std::slice::from_ref(&(0x10100..0x10200)),
    )
    .unwrap();
    let hooked = recover_in_ranges(
        &fs::read(&f.hooked).unwrap(),
        &[Address { cs: 0x1010, ip: 0 }],
        100_000,
        &[0x10100..0x10106, 0x10140..0x10142],
    )
    .unwrap();
    let interrupt = recover_in_ranges(
        &fs::read(&f.interrupt).unwrap(),
        &[Address {
            cs: 0x1010,
            ip: 0x60,
        }],
        100_000,
        std::slice::from_ref(&(0x10160..0x10163)),
    )
    .unwrap();
    let graphs = [&original, &hooked, &hooked, &interrupt];
    let addresses: BTreeSet<_> = graphs.iter().flat_map(|g| g.nodes.keys()).collect();
    let variants: BTreeSet<_> = graphs
        .iter()
        .flat_map(|g| g.nodes.iter().map(|(at, n)| (*at, n.bytes.clone())))
        .collect();
    let stdout = String::from_utf8_lossy(&made.stdout);
    assert!(
        stdout.contains(&format!("static_instructions: {}\n", addresses.len())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("static_variants: {}\n", variants.len())),
        "{stdout}"
    );
    assert!(stdout.contains("static_views: 4\n"));
    let mapping = fs::read_to_string(out.join("source-map.txt")).unwrap();
    assert_eq!(mapping.matches("VIEW ").count(), 4);
    for i in 0..4 {
        assert!(mapping.contains(&format!("VIEW {i} ")));
    }
    assert!(mapping.contains("ranges=[65888..65891] roots=[Address { cs: 4112, ip: 96 }]"));
    assert!(mapping.contains("1010:0060"));
    let translated = fs::read_to_string(out.join("src/translated.rs")).unwrap();
    assert_eq!(
        translated
            .matches("m.guard_code(&[154, 64, 0, 16, 16])")
            .count(),
        2
    );
    assert_eq!(translated.matches("m.guard_code(&[205, 18])").count(), 2);
    assert!(!translated.contains("Decoder"));
    assert!(!out.join("hooked.mem").exists());
    let tree = run("cargo", &["tree", "--offline", "-e", "features"], &out);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8_lossy(&tree.stdout);
    for excluded in ["iced-x86", "winit", "softbuffer", "feature \"analysis\""] {
        assert!(!tree.contains(excluded), "{tree}");
    }
    let build = run("cargo", &["build", "--offline"], &out);
    assert!(build.status.success(), "{build:?}");
    let binary = out.join("target/debug/pal-wayland");
    let memory = f.dir.join("final.mem");
    let ran = run(
        binary.to_str().unwrap(),
        &[
            f.exe.to_str().unwrap(),
            "--memory",
            memory.to_str().unwrap(),
        ],
        &f.dir,
    );
    assert!(ran.status.success(), "{ran:?}");
    let state = String::from_utf8_lossy(&ran.stdout);
    assert!(state.contains("\"bx\":1"), "{state}");
    assert!(state.contains("stop_reason: DOS-exit"), "{state}");
    assert!(state.contains("guest_exit_code: 0"), "{state}");
    let final_memory = fs::read(memory).unwrap();
    assert_eq!(final_memory[0x10100], 0xc3);
    assert_eq!(&final_memory[0x10160..0x10163], &[0xcd, 0x12, 0xc3]);
    // Same opcode, unconfirmed operand must fault without counting a step.
    for (name, ip, bytes) in [
        ("bad-call", "0000", vec![0x9a, 0x41, 0, 0x10, 0x10]),
        ("bad-int", "0060", vec![0xcd, 0x13]),
    ] {
        let mut unknown = f.original.clone();
        let start = 0x10100 + u16::from_str_radix(ip, 16).unwrap() as usize;
        unknown[start..start + bytes.len()].copy_from_slice(&bytes);
        let path = f.dir.join(format!("{name}.mem"));
        fs::write(&path, unknown).unwrap();
        let ran = run(
            binary.to_str().unwrap(),
            &[
                f.exe.to_str().unwrap(),
                "--memory-input",
                path.to_str().unwrap(),
                "--set",
                &format!("ip={ip}"),
            ],
            &f.dir,
        );
        assert!(!ran.status.success(), "{ran:?}");
        assert!(
            String::from_utf8_lossy(&ran.stderr).contains("unknown static instruction variant"),
            "{ran:?}"
        );
        assert!(
            String::from_utf8_lossy(&ran.stdout).contains("\"step\":0"),
            "{ran:?}"
        );
    }
    // Existing output is not touched, even after valid analysis.
    fs::write(out.join("marker"), "preserve").unwrap();
    let again = lift(&f, &out, &args);
    assert!(!again.status.success());
    assert_eq!(fs::read_to_string(out.join("marker")).unwrap(), "preserve");
    // Default generation remains original-only and does not authorize guest patches.
    let original_out = f.dir.join("original-generated");
    let made = lift(&f, &original_out, &["--near-table", "1010:00f0:3"]);
    assert!(made.status.success(), "{made:?}");
    let build = run("cargo", &["build", "--offline"], &original_out);
    assert!(build.status.success(), "{build:?}");
    let ran = run(
        original_out
            .join("target/debug/pal-wayland")
            .to_str()
            .unwrap(),
        &[f.exe.to_str().unwrap()],
        &f.dir,
    );
    assert!(!ran.status.success());
    assert!(
        String::from_utf8_lossy(&ran.stderr).contains("code changed"),
        "{ran:?}"
    );
}

#[test]
fn invalid_static_views_fail_before_creating_any_output() {
    let f = fixture();
    let hook = f.hooked.to_str().unwrap();
    let short = f.dir.join("short.mem");
    fs::write(&short, [0xc3]).unwrap();
    reject(
        &f,
        "short",
        &[
            "--static-view",
            short.to_str().unwrap(),
            "--view-seed",
            "1010:0000",
            "--view-range",
            "10100:10106",
        ],
        "exactly 1 MiB",
    );
    reject(
        &f,
        "no-roots",
        &["--static-view", hook, "--view-range", "10100:10106"],
        "requires at least one --view-seed",
    );
    reject(
        &f,
        "no-range",
        &["--static-view", hook, "--view-seed", "1010:0000"],
        "--view-range bounds",
    );
    reject(
        &f,
        "orphan-seed",
        &["--view-seed", "1010:0000"],
        "preceding --static-view",
    );
    reject(
        &f,
        "orphan-range",
        &["--view-range", "10100:10106"],
        "preceding --static-view",
    );
    for (i, bounds) in ["10100:10100", "10106:10100", "0:100001", "no:hex", "10100"]
        .iter()
        .enumerate()
    {
        reject(
            &f,
            &format!("range-{i}"),
            &[
                "--static-view",
                hook,
                "--view-seed",
                "1010:0000",
                "--view-range",
                bounds,
            ],
            "range",
        );
    }
    reject(
        &f,
        "overlap",
        &[
            "--static-view",
            hook,
            "--view-seed",
            "1010:0000",
            "--view-range",
            "10100:10106",
            "--view-range",
            "10105:10108",
        ],
        "disjoint",
    );
    reject(
        &f,
        "outside-root",
        &[
            "--static-view",
            hook,
            "--view-seed",
            "1010:0000",
            "--view-range",
            "10140:10142",
        ],
        "outside confirmed view ranges",
    );
    reject(
        &f,
        "binding",
        &[
            "--static-view",
            hook,
            "--view-seed",
            "1010:0000",
            "--view-range",
            "10100:10106",
            "--static-view",
            hook,
        ],
        "requires at least one --view-seed",
    );
    reject(
        &f,
        "ambiguous-prefix",
        &[
            "--seed",
            "1010:0060",
            "--static-view",
            hook,
            "--view-seed",
            "1010:0060",
            "--view-range",
            "10160:10161",
        ],
        "ambiguous byte-prefix",
    );
    reject(&f, "missing-value", &["--static-view"], "missing value");
    // Mutable-immediate exemption must not silently disappear on another view.
    let mut changed = f.original.clone();
    changed[0x10181] ^= 1; // Near CALL is not MOV: explicit reject.
    let path = f.dir.join("unsupported.mem");
    fs::write(path, changed).unwrap();
    reject(
        &f,
        "unsupported-mutable",
        &[
            "--mutable-immediate",
            "1010:0080",
            "--static-view",
            hook,
            "--view-seed",
            "1010:0000",
            "--view-range",
            "10100:10106",
        ],
        "MOV",
    );
    let mut changed = f.original.clone();
    // Final MOV AX,4C00 at original driver offset is independently located by its full bytes.
    let at = changed[0x10180..0x101f0]
        .windows(5)
        .position(|b| b == [0xb8, 0, 0x4c, 0xcd, 0x21])
        .unwrap()
        + 0x80;
    changed[0x10100 + at + 1] = 1;
    let path = f.dir.join("conflict.mem");
    fs::write(&path, changed).unwrap();
    let target = format!("1010:{at:04x}");
    reject(
        &f,
        "mutable-conflict",
        &[
            "--mutable-immediate",
            &target,
            "--static-view",
            path.to_str().unwrap(),
            "--view-seed",
            &target,
            "--view-range",
            "10180:101f0",
        ],
        "exact bytes",
    );
}
