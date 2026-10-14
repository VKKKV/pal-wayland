// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
//! Original synthetic instructions only. Compile/run against a decoder-free
//! runtime and exercise an actual guest MOV patch driver, not host-only edits.
mod common;
use common::FixtureCargo;
use pal_wayland::lift::{
    Address, Graph, StaticOverlay, apply_static_overlays, emit_rust, emit_rust_variants,
    recover_borland_in_ranges, recover_in_ranges,
};
use std::{fs, path::Path, process::Command};

const CS: u16 = 0x1010;
fn at(ip: u16) -> Address {
    Address { cs: CS, ip }
}
fn image() -> Vec<u8> {
    let mut bytes = vec![0; 0x100000];
    let base = at(0).physical();
    bytes[base..base + 6].copy_from_slice(&[0xc3, 0x90, 0x90, 0x90, 0x90, 0xc3]);
    // CS-relative MOV stores build CALL FAR 1010:0040 at the RET hook.
    let driver = [
        0x2e, 0xc7, 0x06, 0, 0, 0x9a, 0x40, 0x2e, 0xc7, 0x06, 2, 0, 0, 0x10, 0x2e, 0xc6, 0x06, 4,
        0, 0x10, 0xc3,
    ];
    bytes[base + 0x20..base + 0x20 + driver.len()].copy_from_slice(&driver);
    bytes[base + 0x40..base + 0x44].copy_from_slice(&[0xb8, 0x34, 0x12, 0xcb]);
    bytes[base + 0x80..base + 0x88].copy_from_slice(&[0x2e, 0xc7, 0x06, 0, 2, 0x34, 0x12, 0xc3]);
    bytes
}
fn graph(memory: &[u8], seeds: &[Address]) -> Graph {
    recover_in_ranges(
        memory,
        seeds,
        100,
        std::slice::from_ref(&(at(0).physical()..at(0x100).physical())),
    )
    .unwrap()
}
fn overlay() -> StaticOverlay {
    StaticOverlay {
        address: at(0),
        expected: vec![0xc3, 0x90, 0x90, 0x90, 0x90],
        replacement: vec![0x9a, 0x40, 0, 0x10, 0x10],
    }
}
fn make_views() -> Vec<Graph> {
    let original = image();
    let patched = apply_static_overlays(&original, &[overlay()]).unwrap();
    let seeds = [at(0), at(0x20), at(0x80)];
    let mut views = vec![graph(&original, &seeds), graph(&patched, &seeds)];
    for view in &mut views {
        let n = view.nodes.get_mut(&at(0x80)).unwrap();
        n.mutable_immediate =
            Some(pal_wayland::mutable_immediate::analyze(&n.instruction, &n.bytes).unwrap());
    }
    views
}

#[test]
fn overlays_are_exact_simultaneous_and_alias_safe() {
    let original = image();
    let patched = apply_static_overlays(&original, &[overlay()]).unwrap();
    assert_eq!(original[at(0).physical()], 0xc3);
    assert_eq!(patched[at(0).physical()], 0x9a);
    assert!(
        apply_static_overlays(&patched, &[overlay()])
            .unwrap_err()
            .contains("mismatch")
    );
    let mut alias = overlay();
    alias.address = Address { cs: CS - 1, ip: 16 };
    assert!(
        apply_static_overlays(&original, &[overlay(), alias])
            .unwrap_err()
            .contains("overlap")
    );
    for edit in [
        StaticOverlay {
            address: at(0),
            expected: vec![],
            replacement: vec![],
        },
        StaticOverlay {
            address: at(0),
            expected: vec![0xc3],
            replacement: vec![0x9a, 0],
        },
        StaticOverlay {
            address: at(0xffff),
            expected: vec![0; 2],
            replacement: vec![0; 2],
        },
        StaticOverlay {
            address: Address {
                cs: 0xffff,
                ip: 0xffff,
            },
            expected: vec![0],
            replacement: vec![1],
        },
    ] {
        assert!(apply_static_overlays(&original, &[edit]).is_err());
    }
}

#[test]
fn variants_are_deterministic_and_deduplicated() {
    let mut views = make_views();
    let emitted = emit_rust_variants(&views).unwrap();
    views.reverse();
    views.push(views[0].clone());
    assert_eq!(emit_rust_variants(&views).unwrap(), emitted);
    assert_eq!(emitted.matches("if m.poll_timer_irq()?").count(), 1);
    assert!(emitted.contains("m.guard_code(&[154, 64, 0, 16, 16])"));
    assert!(emit_rust_variants(&[]).is_err());
    let view = views.pop().unwrap();
    assert_eq!(
        emit_rust_variants(std::slice::from_ref(&view)).unwrap(),
        emit_rust(&view).unwrap()
    );
}

#[test]
fn stale_or_truncated_ir_cannot_turn_full_guards_into_prefix_guards() {
    let mut views = make_views();
    views[1].nodes.get_mut(&at(0)).unwrap().bytes.truncate(1);
    assert!(
        emit_rust_variants(&views)
            .unwrap_err()
            .contains("match decoded IR")
    );
    let mut views = make_views();
    views[1].nodes.get_mut(&at(0)).unwrap().bytes[2] = 1;
    assert!(
        emit_rust_variants(&views)
            .unwrap_err()
            .contains("match decoded IR")
    );
}

#[test]
fn conflicting_mov_variants_must_not_exempt_any_bytes() {
    let original = image();
    let mut patched = original.clone();
    patched[at(0x80).physical() + 5] = 0x99;
    let mut first = graph(&original, &[at(0x80)]);
    let second = graph(&patched, &[at(0x80)]);
    let n = first.nodes.get_mut(&at(0x80)).unwrap();
    n.mutable_immediate =
        Some(pal_wayland::mutable_immediate::analyze(&n.instruction, &n.bytes).unwrap());
    assert!(
        emit_rust_variants(&[first.clone(), second])
            .unwrap_err()
            .contains("exact bytes")
    );
    let mut fixed = first.clone();
    fixed.nodes.get_mut(&at(0x80)).unwrap().mutable_immediate = None;
    assert!(
        emit_rust_variants(&[first, fixed])
            .unwrap_err()
            .contains("inconsistent semantics")
    );
}

#[test]
fn borland_views_keep_the_existing_sync_patch_and_macro_guards() {
    let bytes = [0xcd, 0x3d, 0xcd, 0x3b, 0x07, 0xc3];
    let view = recover_borland_in_ranges(
        &bytes,
        &[Address { cs: 0, ip: 0 }],
        100,
        std::slice::from_ref(&(0..bytes.len())),
    )
    .unwrap();
    assert_eq!(
        emit_rust_variants(std::slice::from_ref(&view)).unwrap(),
        emit_rust(&view).unwrap()
    );
}

fn run(program: &Path, args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .fixture_output()
        .unwrap()
}
#[test]
fn emitted_variants_execute_guest_patch_driver_and_fail_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = common::unique_dir("pal-variants");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("Cargo.toml"), format!("[package]\nname=\"pal-variant-fixture\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    let code = emit_rust_variants(&make_views()).unwrap();
    fs::write(dir.join("src/good.rs"), &code).unwrap();
    // Deliberate mutations are confined to scratch. A first-byte-only selector
    // AND guard must be rejected by the same unknown-far-target regression.
    let guard = "m.guard_code(&[154, 64, 0, 16, 16])";
    assert_eq!(code.matches(guard).count(), 2);
    fs::write(
        dir.join("src/prefix.rs"),
        code.replace(guard, "m.guard_code(&[154])"),
    )
    .unwrap();
    let boundary = "if m.poll_timer_irq()? {return Ok(()); }";
    assert_eq!(code.matches(boundary).count(), 1);
    fs::write(
        dir.join("src/double.rs"),
        code.replace(boundary, &format!("{boundary}\nm.steps += 1;")),
    )
    .unwrap();
    fs::write(dir.join("src/main.rs"), HARNESS).unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo = Path::new(&cargo);
    let build = run(cargo, &["build", "--offline", "--quiet"], &dir);
    assert!(build.status.success(), "build {dir:?}: {build:?}");
    let tree = run(cargo, &["tree", "--offline", "-e", "features"], &dir);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        !tree.contains("iced-x86") && !tree.contains("winit"),
        "{tree}"
    );
    let bin = dir.join("target/debug/pal-variant-fixture");
    let result = run(&bin, &[], &dir);
    assert!(result.status.success(), "run {dir:?}: {result:?}");
    print!("{}", String::from_utf8(result.stdout).unwrap());
    for (mutant, reason) in [
        ("prefix", "unknown bytes must fault"),
        ("double", "instruction counted exactly once"),
    ] {
        let result = run(&bin, &[mutant], &dir);
        assert!(!result.status.success(), "mutation survived: {mutant}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(reason),
            "{result:?}"
        );
        println!("REJECTED {mutant}: {reason}");
    }
}

const HARNESS: &str = r#"
#![allow(dead_code, unused_variables, unused_parens, unreachable_code)]
use pal_wayland::{runtime::{Machine,Fault}, semantics::{SegmentedAddress as A,Flags}};
mod good; mod prefix; mod double;
const CS:u16=0x1010;
type Step=fn(&mut Machine)->Result<(),Fault>;
fn machine()->Machine {
    let mut mz=vec![0;96];
    for (i,word) in [0x5a4d_u16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate(){
        mz[i*2..i*2+2].copy_from_slice(&word.to_le_bytes());
    }
    let mut m=Machine::from_mz(&mz,CS).unwrap();
    m.regs.cs=CS; m.regs.ip=0; m.regs.ss=0x7000; m.regs.sp=0x1000;
    m.regs.flags=Flags::from_bits(2);
    put(&mut m,0,&[0xc3,0x90,0x90,0x90,0x90,0xc3]);
    put(&mut m,0x20,&[0x2e,0xc7,0x06,0,0,0x9a,0x40,0x2e,0xc7,0x06,2,0,0,0x10,0x2e,0xc6,0x06,4,0,0x10,0xc3]);
    put(&mut m,0x40,&[0xb8,0x34,0x12,0xcb]);
    put(&mut m,0x80,&[0x2e,0xc7,0x06,0,2,0x34,0x12,0xc3]);
    m.write16(A::new(m.regs.ss,m.regs.sp),0x50).unwrap(); m
}
fn put(m:&mut Machine,ip:u16,bytes:&[u8]){
    for (i,b) in bytes.iter().enumerate(){m.write8(A::new(CS,ip+i as u16),*b).unwrap();}
}
fn one(m:&mut Machine,step:Step){let before=m.steps;step(m).unwrap();assert_eq!(m.steps,before+1,"instruction counted exactly once");}
fn reject(m:&mut Machine,step:Step){
    let regs=m.regs;let steps=m.steps;
    let err=step(m).expect_err("unknown bytes must fault");
    assert!(err.0.contains("code changed") || err.0.contains("unknown static"),"{err:?}");
    assert_eq!(m.steps,steps,"rejected mutation must not count");assert_eq!(m.regs,regs,"rejected mutation must not execute");
}
fn main(){
    let mode=std::env::args().nth(1).unwrap_or_default();
    let step:Step=match mode.as_str(){"prefix"=>prefix::step,"double"=>double::step,_=>good::step};
    if mode=="prefix" {let mut m=machine();put(&mut m,0,&[0x9a,0x40,1,0x10,0x10]);reject(&mut m,step);return;}
    let mut m=machine();one(&mut m,step);assert_eq!(m.regs.ip,0x50);assert_eq!(m.regs.sp,0x1002);
    println!("PASS original RET len=1 steps=1");
    let mut m=machine();m.regs.ip=0x20;m.regs.sp-=2;m.write16(A::new(m.regs.ss,m.regs.sp),0).unwrap();
    for _ in 0..4 {one(&mut m,step);}
    assert_eq!(m.regs.ip,0);m.guard_code(&[0x9a,0x40,0,0x10,0x10]).unwrap();
    one(&mut m,step);assert_eq!((m.regs.cs,m.regs.ip),(CS,0x40));assert_eq!(m.regs.sp,0xffc);
    assert_eq!(m.read16(A::new(m.regs.ss,m.regs.sp)).unwrap(),5);
    assert_eq!(m.read16(A::new(m.regs.ss,m.regs.sp+2)).unwrap(),CS);
    one(&mut m,step);assert_eq!(m.regs.ax,0x1234);
    one(&mut m,step);assert_eq!(m.regs.ip,5);assert_eq!(m.regs.sp,0x1000);
    one(&mut m,step);assert_eq!(m.regs.ip,0x50);assert_eq!(m.steps,8);
    println!("PASS guest MOV driver RET -> CALL FAR len=5 -> RETF -> RET steps=8");
    for bytes in [&[0x90,0x40,0,0x10,0x10][..],&[0x9a,0x40,1,0x10,0x10],&[0x9a,0x40,0,0x11,0x10],&[0x66,0xc3,0x90,0x90,0x90]] {
        let mut m=machine();put(&mut m,0,bytes);reject(&mut m,step);
    }
    let mut m=machine();m.regs.ip=0x777;reject(&mut m,step);
    println!("PASS third opcode/far-target/segment/prefix mutations and unknown CS:IP fault without steps");
    let mut m=machine();m.regs.flags.set(Flags::IF,true);m.enable_irqs().unwrap();m.virtual_elapsed_ns=Some(0);
    m.port_out(0x21,0xfd,8).unwrap();m.write16(A::new(0,9*4),0x100).unwrap();m.write16(A::new(0,9*4+2),0x6000).unwrap();
    m.key_event(0x1c,13,true);step(&mut m).unwrap();
    assert_eq!((m.regs.cs,m.regs.ip,m.steps,m.irq_deliveries),(0x6000,0x100,0,1));
    assert_eq!(m.regs.sp,0xffa);assert_eq!(m.read16(A::new(m.regs.ss,m.regs.sp)).unwrap(),0);
    println!("PASS IRQ boundary precedes variant guard and counts no guest instruction");
    let mut m=machine();m.regs.ip=0x80;put(&mut m,0x85,&[0xef,0xbe]);one(&mut m,step);
    assert_eq!(m.read16(A::new(CS,0x200)).unwrap(),0xbeef);assert_eq!(m.regs.ip,0x87);
    for (offset,value) in [(0,0x3e),(1,0xc6),(2,0x07),(3,1),(4,3)] {
        let mut m=machine();m.regs.ip=0x80;m.write8(A::new(CS,0x80+offset),value).unwrap();reject(&mut m,step);
    }
    println!("PASS mutable MOV current immediate and full prefix/opcode/ModRM/displacement immutable guards");
}
"#;
