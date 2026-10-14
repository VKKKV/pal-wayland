// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Compile and execute emitted instructions against the decoder-free runtime.
//! The old-STI mutation changes only scratch-generated code, never lift.rs.

mod common;
use common::FixtureCargo;
use pal_wayland::lift::{Address, emit_rust, recover};
use std::{fs, path::Path, process::Command};

const CS: u16 = 0x1010;
const OLD_STI: &str = "if !m.regs.flags.contains(Flags::IF){m.irq_shadow=1;}";

fn emit(code: &[u8]) -> String {
    let mut memory = vec![0; 0x100000];
    let start = usize::from(CS) * 16;
    memory[start..start + code.len()].copy_from_slice(code);
    let graph = recover(&memory, &[Address { cs: CS, ip: 0 }], code.len()).unwrap();
    emit_rust(&graph).unwrap()
}

fn run(program: &Path, args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        // Do not inherit a target directory that could deadlock the outer Cargo.
        .fixture_output()
        .expect("execute generated IRQ fixture")
}

#[test]
fn emitted_irq_shadows_execute_and_reject_unconditional_sti_mutation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = common::unique_dir("pal-irq-codegen");
    fs::create_dir_all(dir.join("src")).unwrap();
    // The integration test performs recovery; the emitted executable links only
    // the real runtime, with analysis/iced-x86 and window features disabled.
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"pal-irq-codegen-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\npal-wayland = {{ path = {root:?}, default-features = false }}\n"
        ),
    )
    .unwrap();
    for (name, code, sti_count) in [
        ("sti", &[0xfb, 0x90, 0x90][..], 1),
        ("sti_sti", &[0xfb, 0xfb, 0x90, 0x90][..], 2),
        ("mov_ss", &[0x8e, 0xd0, 0x90, 0x90][..], 0),
        ("pop_ss", &[0x17, 0x90, 0x90][..], 0),
        ("popf", &[0x9d, 0x90, 0x90][..], 0),
    ] {
        let source = emit(code);
        fs::write(dir.join(format!("src/{name}.rs")), &source).unwrap();
        if sti_count != 0 {
            // Exact pre-fix semantics: every STI refreshes the shadow, even IF=1.
            // Require each intended mutation site, so this cannot silently be a
            // no-op after a codegen refactor.
            assert_eq!(source.matches(OLD_STI).count(), sti_count);
            let mutant = source.replace(OLD_STI, "m.irq_shadow=1;");
            fs::write(dir.join(format!("src/old_{name}.rs")), mutant).unwrap();
        }
    }
    fs::write(dir.join("src/main.rs"), HARNESS).unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo = Path::new(&cargo);
    let build = run(cargo, &["build", "--offline", "--quiet"], &dir);
    assert!(
        build.status.success(),
        "fixture build in {dir:?}: {build:?}"
    );
    let tree = run(cargo, &["tree", "--offline", "-e", "features"], &dir);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        !tree.contains("iced-x86"),
        "runtime must be decoder-free: {tree}"
    );
    assert!(
        !tree.contains("winit"),
        "fixture must not open a GUI: {tree}"
    );

    let binary = dir.join("target/debug/pal-irq-codegen-fixture");
    for scenario in [
        "sti_if1",
        "sti_sti_if0",
        "sti_if0",
        "mov_ss",
        "pop_ss",
        "popf",
    ] {
        let result = run(&binary, &[scenario], &dir);
        assert!(result.status.success(), "{scenario} in {dir:?}: {result:?}");
        let stdout = String::from_utf8(result.stdout).unwrap();
        assert!(stdout.contains(&format!("PASS {scenario}")), "{stdout}");
        print!("{stdout}");
    }
    for scenario in ["sti_if1", "sti_sti_if0"] {
        let mutant = run(&binary, &[scenario, "--old-sti"], &dir);
        let stdout = String::from_utf8_lossy(&mutant.stdout);
        let stderr = String::from_utf8_lossy(&mutant.stderr);
        assert!(
            !mutant.status.success(),
            "old STI survived {scenario}: {stdout}"
        );
        assert!(
            stderr.contains("pending IRQ must precede the next NOP"),
            "mutation must fail the IRQ-boundary assertion, not build/setup: {mutant:?}"
        );
        println!("REJECTED old STI {scenario}: {stdout}{stderr}");
    }
}

const HARNESS: &str = r#"
#![allow(unused_parens, unused_variables, dead_code)]
use pal_wayland::{runtime::{Fault, Machine}, semantics::{Flags, SegmentedAddress as A}};
mod sti;
mod sti_sti;
mod old_sti;
mod old_sti_sti;
mod mov_ss;
mod pop_ss;
mod popf;

const CS: u16 = 0x1010;
const HANDLER_CS: u16 = 0x6000;
const HANDLER_IP: u16 = 0x100;
type Step = fn(&mut Machine) -> Result<(), Fault>;

fn machine(code: &[u8], initial_if: bool) -> Machine {
    let mut bytes = vec![0; 96];
    for (i, word) in [0x5a4d_u16, 96, 1, 0, 2, 0, 0xffff, 0, 64, 0, 0, 0, 28, 0]
        .into_iter().enumerate()
    {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    let mut m = Machine::from_mz(&bytes, CS).unwrap();
    m.regs.cs = CS;
    m.regs.ip = 0;
    m.regs.ss = 0x7000;
    m.regs.sp = 0x1000;
    m.regs.flags = Flags::from_bits(Flags::CF | 2);
    m.regs.flags.set(Flags::IF, initial_if);
    m.enable_irqs().unwrap();
    m.virtual_elapsed_ns = Some(0); // PIT cannot make this a timing-dependent test.
    m.port_out(0x21, 0xfd, 8).unwrap(); // Only keyboard IRQ1 is unmasked.
    m.write16(A::new(0, 9 * 4), HANDLER_IP).unwrap();
    m.write16(A::new(0, 9 * 4 + 2), HANDLER_CS).unwrap();
    for (i, byte) in code.iter().enumerate() {
        m.write8(A::new(CS, i as u16), *byte).unwrap();
    }
    m
}

fn queue_irq(m: &mut Machine) {
    m.key_event(0x1c, 13, true);
    assert_eq!(m.port_in(0x64, 8).unwrap(), 1);
}

fn instruction(m: &mut Machine, step: Step, expected_ip: u16) {
    let steps = m.steps;
    let flags = m.regs.flags;
    step(m).unwrap();
    assert_eq!((m.regs.cs, m.regs.ip), (CS, expected_ip));
    assert_eq!(m.steps, steps + 1);
    assert_eq!(m.irq_deliveries, 0, "protected instruction must execute, not enter IRQ");
    assert_eq!(m.regs.flags, flags, "protected NOP/STI must preserve flags");
}

fn irq_before_nop(m: &mut Machine, step: Step, return_ip: u16) {
    let before = m.regs;
    let steps = m.steps;
    assert_eq!(before.ip, return_ip);
    step(m).unwrap();
    println!("IRQ boundary expected saved IP={return_ip:04x}; observed CS:IP={:04x}:{:04x}, deliveries={}, shadow={}, steps={}",
        m.regs.cs, m.regs.ip, m.irq_deliveries, m.irq_shadow, m.steps);
    assert_eq!((m.regs.cs, m.regs.ip), (HANDLER_CS, HANDLER_IP),
        "pending IRQ must precede the next NOP");
    assert_eq!(m.irq_deliveries, 1);
    assert_eq!(m.irq_shadow, 0);
    assert_eq!(m.steps, steps, "IRQ entry must not execute/count the NOP");
    assert_eq!(m.regs.sp, before.sp - 6);
    assert_eq!(m.read16(A::new(m.regs.ss, m.regs.sp)).unwrap(), return_ip);
    assert_eq!(m.read16(A::new(m.regs.ss, m.regs.sp + 2)).unwrap(), CS);
    assert_eq!(m.read16(A::new(m.regs.ss, m.regs.sp + 4)).unwrap(),
        Machine::normalize_flags(before.flags.bits()));
    assert_eq!(m.regs.flags.bits(),
        Machine::normalize_flags(before.flags.bits()) & !(Flags::IF | Flags::TF));
    assert_eq!(m.port_in(0x60, 8).unwrap(), 0x1c);
    m.port_out(0x20, 0x20, 8).unwrap();
    m.interrupt_return().unwrap();
    assert_eq!(m.regs, before, "IRET must restore the exact boundary state");
    step(m).unwrap();
    assert_eq!((m.regs.cs, m.regs.ip), (CS, return_ip + 1));
    assert_eq!(m.steps, steps + 1);
    assert_eq!(m.irq_deliveries, 1, "consumed IRQ must not be redelivered");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().unwrap();
    let old = args.next().as_deref() == Some("--old-sti");
    match scenario.as_str() {
        "sti_if1" => {
            let step: Step = if old {old_sti::step} else {sti::step};
            let mut m = machine(&[0xfb, 0x90, 0x90], true);
            instruction(&mut m, step, 1);
            queue_irq(&mut m); // Queue after STI so STI itself can really execute.
            irq_before_nop(&mut m, step, 1);
        }
        "sti_sti_if0" => {
            let step: Step = if old {old_sti_sti::step} else {sti_sti::step};
            let mut m = machine(&[0xfb, 0xfb, 0x90, 0x90], false);
            queue_irq(&mut m);
            step(&mut m).unwrap();
            assert_eq!((m.regs.cs, m.regs.ip), (CS, 1));
            assert!(m.regs.flags.contains(Flags::IF));
            assert_eq!(m.irq_shadow, 1);
            instruction(&mut m, step, 2); // Second STI consumes, never renews, shadow.
            irq_before_nop(&mut m, step, 2);
        }
        "sti_if0" => {
            let mut m = machine(&[0xfb, 0x90, 0x90], false);
            queue_irq(&mut m);
            sti::step(&mut m).unwrap();
            assert_eq!(m.regs.ip, 1);
            assert!(m.regs.flags.contains(Flags::IF));
            assert_eq!(m.irq_shadow, 1);
            instruction(&mut m, sti::step, 2);
            assert_eq!(m.irq_shadow, 0);
            irq_before_nop(&mut m, sti::step, 2);
        }
        "mov_ss" => {
            let mut m = machine(&[0x8e, 0xd0, 0x90, 0x90], true);
            m.regs.ax = 0x7100; // Real segment change, not a no-op MOV SS.
            mov_ss::step(&mut m).unwrap();
            assert_eq!((m.regs.ss, m.regs.ip, m.irq_shadow), (0x7100, 2, 1));
            queue_irq(&mut m);
            instruction(&mut m, mov_ss::step, 3);
            assert_eq!(m.irq_shadow, 0);
            irq_before_nop(&mut m, mov_ss::step, 3);
        }
        "pop_ss" => {
            let mut m = machine(&[0x17, 0x90, 0x90], true);
            m.write16(A::new(m.regs.ss, m.regs.sp), 0x7100).unwrap();
            pop_ss::step(&mut m).unwrap();
            assert_eq!((m.regs.ss, m.regs.sp, m.regs.ip, m.irq_shadow),
                (0x7100, 0x1002, 1, 1));
            queue_irq(&mut m);
            instruction(&mut m, pop_ss::step, 2);
            assert_eq!(m.irq_shadow, 0);
            irq_before_nop(&mut m, pop_ss::step, 2);
        }
        "popf" => {
            let mut m = machine(&[0x9d, 0x90, 0x90], false);
            m.write16(A::new(m.regs.ss, m.regs.sp), 0x203).unwrap();
            queue_irq(&mut m);
            popf::step(&mut m).unwrap();
            assert_eq!((m.regs.ip, m.regs.sp, m.irq_shadow), (1, 0x1002, 0));
            assert!(m.regs.flags.contains(Flags::IF));
            irq_before_nop(&mut m, popf::step, 1);
        }
        _ => panic!("unknown scenario: {scenario}"),
    }
    println!("PASS {scenario}");
}
"#;
