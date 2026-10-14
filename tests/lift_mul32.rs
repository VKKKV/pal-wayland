// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]

mod common;
use common::FixtureCargo;
use iced_x86::{Code, Decoder, DecoderOptions};
use pal_wayland::lift::{Address, Graph, emit_rust, recover};
use std::{fs, process::Command};

// Authored instruction fixtures only. No original game asset is read or embedded.
// The EBX fixture uses the reported stopping address, not copied asset content.
const CS: u16 = 0x3add;

#[test]
fn mul32_rejects_other_widths_signed_implicit_and_unverified_prefixes() {
    for bytes in [
        &[0xf7, 0xe3][..],         // word MUL
        &[0xf6, 0xe3],             // byte MUL
        &[0x66, 0xf7, 0xeb],       // implicit signed IMUL remains unsupported
        &[0x67, 0x66, 0xf7, 0x20], // address32 memory belongs to lift_addr32
        &[0x64, 0x66, 0xf7, 0x27],
        &[0x65, 0x66, 0xf7, 0x27],
        &[0xf0, 0x66, 0xf7, 0x27],
        &[0xf2, 0x66, 0xf7, 0xe3],
        &[0xf3, 0x66, 0xf7, 0xe3],
    ] {
        let i = Decoder::with_ip(16, bytes, 0x02b0, DecoderOptions::NONE).decode();
        assert!(pal_wayland::lift32::emit(&i).is_err(), "{bytes:02x?}");
    }
}

#[test]
fn generated_mul32_executes_focused_unsigned_oracle_and_atomic_faults() {
    let dir = common::unique_dir("pal-mul32");
    fs::create_dir_all(dir.join("src")).unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    fs::write(dir.join("Cargo.toml"), format!("[package]\nname=\"pal-mul32-fixture\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    let mut graph = Graph::default();
    let mut fixtures = Vec::new();
    let mut add = |kind: u8, segment: u8, ip: u16, bytes: Vec<u8>| {
        let at = Address { cs: CS, ip };
        let mut memory = vec![0; at.physical() + bytes.len()];
        memory[at.physical()..].copy_from_slice(&bytes);
        let one = recover(&memory, &[at], 1).unwrap();
        assert_eq!(one.nodes[&at].instruction.code(), Code::Mul_rm32);
        graph.nodes.extend(one.nodes);
        fixtures.push(format!(
            "Fixture{{ip:{ip},kind:{kind},segment:{segment},bytes:&{bytes:?}}}"
        ));
    };
    for reg in 0..8 {
        add(
            reg,
            0,
            if reg == 3 {
                0x02b0
            } else {
                0x2000 + u16::from(reg) * 8
            },
            vec![0x66, 0xf7, 0xe0 | reg],
        );
    }
    for rm in 0..8 {
        add(
            8 + rm,
            0,
            0x2100 + u16::from(rm) * 8,
            vec![0x66, 0xf7, 0x60 | rm, 0xfe],
        );
    }
    add(16, 0, 0x2200, vec![0x66, 0xf7, 0x26, 1, 2]);
    for (segment, prefix) in [(1, 0x26), (2, 0x2e), (3, 0x36), (4, 0x3e)] {
        add(
            10,
            segment,
            0x2300 + u16::from(segment) * 8,
            vec![prefix, 0x66, 0xf7, 0x62, 0xfe],
        );
    }
    add(3, 0, 0xfffd, vec![0x66, 0xf7, 0xe3]);
    let emitted = emit_rust(&graph).unwrap();
    assert!(
        !emitted.contains("unsupported 32-bit operand instruction Mul_rm32"),
        "MUL32 remains unsupported"
    );
    assert!(
        !emitted.contains("iced_x86")
            && !emitted.contains("Decoder")
            && !emitted.contains("unsafe")
    );
    fs::write(dir.join("src/generated.rs"), emitted).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        HARNESS.replace("@FIXTURES@", &fixtures.join(",\n")),
    )
    .unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let run = |args: &[&str]| {
        Command::new(&cargo)
            .args(args)
            .env_remove("CARGO_TARGET_DIR")
            .current_dir(&dir)
            .fixture_output()
            .unwrap()
    };
    let tree = run(&["tree", "--offline", "-e", "features"]);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        !tree.contains("iced-x86") && !tree.contains("winit") && !tree.contains("softbuffer"),
        "{tree}"
    );
    let result = run(&["run", "--offline", "--quiet"]);
    assert!(result.status.success(), "generated execution: {result:?}");
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("MUL32: 22 encodings; 704 u128-oracle executions; 8 atomic read faults; 7 exact-byte guard faults"), "{stdout}");
    print!("{stdout}");
}

const HARNESS: &str = r#"
#![forbid(unsafe_code)]
#![allow(dead_code,unused_variables,unused_parens,unreachable_code)]
use pal_wayland::{runtime::Machine,semantics::{SegmentedAddress as A,Flags,Registers,GuestMemory,A20},planar_video::PlanarVideo};
mod generated;
const CS:u16=0x3add;
struct Fixture {ip:u16,kind:u8,segment:u8,bytes:&'static [u8]}
const FIXTURES:&[Fixture]=&[@FIXTURES@];
fn machine()->Machine {
    let mut mz=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {mz[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
    let mut m=Machine::from_mz(&mz,CS).unwrap();m.memory=GuestMemory::new(0x60000,A20::Disabled);m
}
fn ea(m:&Machine,f:&Fixture)->A {
    // Independent unsigned sum/modulus oracle, not emitted wrapping_add code.
    let [bx,bp,si,di]=[m.regs.bx,m.regs.bp,m.regs.si,m.regs.di].map(u32::from);
    let sum=match f.kind {8=>bx+si,9=>bx+di,10=>bp+si,11=>bp+di,12=>si,13=>di,14=>bp,15=>bx,16=>0x203,_=>unreachable!()};
    let offset=((sum+65536-2)%65536) as u16;
    let segment=match f.segment {1=>m.regs.es,2=>m.regs.cs,3=>m.regs.ss,4=>m.regs.ds,_=>if matches!(f.kind,10|11|14) {m.regs.ss} else {m.regs.ds}};
    A::new(segment,offset)
}
fn setup(m:&mut Machine,f:&Fixture,a:u32,b:u32,seed:u16)->(u32,u32) {
    m.regs=Registers{cs:CS,ip:f.ip,ds:0x200,ss:0x2200,es:0x4200,flags:Flags::from_bits(seed),..Registers::default()};
    for r in 0..8 {m.set32(r,0x9234_2468+u32::from(r)*0x10000);}
    m.set32(0,a);m.set32(2,0x7654_3210);
    if f.kind<8 {m.set32(f.kind,b);} else {
        m.set32(3,0xcafe_fff0);m.set32(5,0xbeef_fff0);m.set32(6,0xdead_0031);m.set32(7,0xabcd_0031);
        let at=ea(m,f);
        for segment in [m.regs.ds,m.regs.ss,m.regs.es,m.regs.cs] {m.write32(A::new(segment,at.offset),0x1357_2468).unwrap();}
        m.write32(at,b).unwrap();
    }
    m.memory.write_bytes(A::new(CS,f.ip),f.bytes).unwrap();
    (m.get32(0),if f.kind<8 {m.get32(f.kind)} else {b})
}
// Wide unsigned arithmetic with division/remainder splitting, independent of
// production u64 casts/shifts. Only CF/OF are defined; preserve all other bits.
fn oracle(a:u32,b:u32,seed:u16)->(u32,u32,u16) {
    let base=1u128<<32;let product=u128::from(a)*u128::from(b);
    ((product%base) as u32,(product/base) as u32,(seed&!0x801)|if product>=base {0x801} else {0})
}
fn verify(m:&mut Machine,f:&Fixture,expected:Result<(u32,u32,u16),&str>,charged:bool,read_planes:Option<PlanarVideo>) {
    let regs=m.regs;let gprs:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));
    let memory=m.memory.clone();let planes=read_planes.unwrap_or_else(||m.planar.borrow().clone());let steps=m.steps;
    let result=generated::step(m);
    match expected {
        Ok((low,high,flags))=>{
            assert!(result.is_ok(),"MUL32 semantic mismatch: unexpected {result:?}, ip={:x}",f.ip);
            let mut expected_regs=regs;expected_regs.ax=low as u16;expected_regs.dx=high as u16;
            expected_regs.ip=f.ip.wrapping_add(f.bytes.len() as u16);expected_regs.flags=Flags::from_bits(flags);
            assert_eq!(m.regs.flags.bits()&0x801,flags&0x801,"MUL32 semantic mismatch: defined CF/OF");
            assert_eq!(m.regs.flags.bits()&!0x801,regs.flags.bits()&!0x801,"MUL32 semantic mismatch: undefined/unaffected flags");
            assert_eq!(m.regs,expected_regs,"MUL32 semantic mismatch: full registers/IP");
            for r in 0..8 {assert_eq!(m.get32(r),match r {0=>low,2=>high,_=>gprs[r as usize]},"MUL32 semantic mismatch: GPR/upper {r}");}
        },
        Err(reason)=>{
            assert!(matches!(result,Err(ref fault) if fault.0.contains(reason)),"MUL32 semantic mismatch: expected {reason}, got {result:?}");
            assert_eq!(m.regs,regs,"MUL32 semantic mismatch: fault registers/IP/flags");
            for r in 0..8 {assert_eq!(m.get32(r),gprs[r as usize],"MUL32 semantic mismatch: fault upper/GPR");}
        },
    }
    assert_eq!(m.memory,memory,"MUL32 semantic mismatch: memory mutation");
    assert_eq!(*m.planar.borrow(),planes,"MUL32 semantic mismatch: planar/latches");
    assert_eq!(m.steps,steps+u64::from(charged),"MUL32 semantic mismatch: caller-owned steps");
}
fn main() {
    let mut m=machine();let mut total=0;
    // Short representative products: zero, fitting maximum, high-bit unsigned,
    // carry into EDX, full maximum product, and asymmetric low/high words.
    let products=[(0,u32::MAX),(u32::MAX,0),(1,u32::MAX),(0x8000_0000,1),(0x8000_0000,2),(u32::MAX,u32::MAX),(0x1234_5678,0x8765_4321),(0x10000,0x10000)];
    for f in FIXTURES {for (a,b) in products {for seed in [0,0xffff,0xa555,0x5aaa] {
        let (a,b)=setup(&mut m,f,a,b,seed);verify(&mut m,f,Ok(oracle(a,b,seed)),true,None);total+=1;
    }}}
    let f=FIXTURES.iter().find(|f|f.kind==15).unwrap();let mut faults=0;
    for offset in [0xfffcu16,0xfffd,0xfffe,0xffff] {
        setup(&mut m,f,3,7,0xa555);m.set32(3,0xcafe_0000|u32::from(offset.wrapping_add(2)));
        if offset==0xfffc {m.write32(ea(&m,f),7).unwrap();verify(&mut m,f,Ok(oracle(3,7,0xa555)),true,None);} else {verify(&mut m,f,Err("segment boundary"),true,None);faults+=1;}
    }
    for (a20,ds,offset) in [(A20::Disabled,0x5fff,0x0fu16),(A20::Enabled,0xffff,0x20),(A20::Disabled,0xffff,0x0f)] {
        m=machine();setup(&mut m,f,3,7,0xa555);m.memory.set_a20(a20);m.regs.ds=ds;
        m.set32(3,0xcafe_0000|u32::from(offset.wrapping_add(2)));
        verify(&mut m,f,Err("out of bounds"),true,None);faults+=1;
    }
    m=machine();setup(&mut m,f,3,7,0xa555);m.regs.ds=0xffff;m.set32(3,0xcafe_0022);
    m.write32(ea(&m,f),7).unwrap();verify(&mut m,f,Ok(oracle(3,7,0xa555)),true,None);
    m=machine();setup(&mut m,f,3,7,0xa555);m.memory=GuestMemory::new(0x110000,A20::Disabled);
    m.memory.write_bytes(A::new(CS,f.ip),f.bytes).unwrap();m.video_mode=0x12;
    m.planar.borrow_mut().write32(0xa0000,0x9876_5432).unwrap();
    m.planar.borrow_mut().read8(0xa0001).unwrap();
    for ds in [0x9fff,0xafff] {
        m.regs.ds=ds;m.set32(3,0xcafe_0011);
        verify(&mut m,f,Err("mixed RAM/planar"),true,None);faults+=1;
    }
    // Valid device read: expected latches come from one full read of a cloned
    // device; compare real emitted instruction's arithmetic and complete device.
    m.regs.ds=0xa000;m.set32(3,0xcafe_0002);
    let mut planes=m.planar.borrow().clone();let operand=planes.read32(0xa0000).unwrap();
    verify(&mut m,f,Ok(oracle(3,operand,0xa555)),true,Some(planes));
    let mut guards=0;
    for f in [&FIXTURES[3],f] {for n in 0..f.bytes.len() {
        m=machine();setup(&mut m,f,7,3,0xa555);
        m.memory.write8(A::new(CS,f.ip+n as u16),f.bytes[n]^1).unwrap();
        verify(&mut m,f,Err("code changed"),false,None);guards+=1;
    }}
    println!("MUL32: {} encodings; {total} u128-oracle executions; {faults} atomic read faults; {guards} exact-byte guard faults; EA16/segments/A20/IP-wrap/device reads; full GPR/upper/FLAGS/memory/planar/IP/steps; decoder-free",FIXTURES.len());
}
"#;
