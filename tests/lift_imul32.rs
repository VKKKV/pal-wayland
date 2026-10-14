// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]

mod common;
use common::FixtureCargo;
use iced_x86::{Code, Decoder, DecoderOptions};
use pal_wayland::lift::{Address, Graph, emit_rust, recover};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

// Original PAL.raw.exe/source-map: 33c4:1081 = 66 6b db 4b, IMUL EBX,EBX,75.
// Fixtures are authored instructions; no proprietary file is read by this test.
const CS: u16 = 0x33c4;

fn run(program: &Path, args: &[&str], dir: &Path) -> Output {
    Command::new(program)
        .args(args)
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(dir)
        .fixture_output()
        .unwrap()
}

#[test]
fn imul32_rejects_wrong_width_one_operand_and_unverified_prefixes() {
    for bytes in [
        &[0x0f, 0xaf, 0xdb][..],
        &[0x6b, 0xdb, 0xff],
        &[0x69, 0xdb, 0xff, 0xff],
        &[0x66, 0xf7, 0xeb], // one-operand EDX:EAX IMUL is deliberately outside this slice
        &[0x67, 0x66, 0x6b, 0x18, 0xff], // address-size32 belongs to lift_addr32
        &[0x64, 0x66, 0x6b, 0x1f, 0xff],
        &[0x65, 0x66, 0x6b, 0x1f, 0xff],
        &[0xf3, 0x66, 0x6b, 0xdb, 0xff],
        &[0xf2, 0x66, 0x6b, 0xdb, 0xff],
        &[0xf0, 0x66, 0x6b, 0x1f, 0xff],
    ] {
        let i = Decoder::with_ip(16, bytes, 0x1081, DecoderOptions::NONE).decode();
        assert!(pal_wayland::lift32::emit(&i).is_err(), "{bytes:02x?}");
    }
}

#[test]
fn generated_imul32_executes_debug_release_and_kills_semantic_mutants() {
    let dir = common::unique_dir("pal-imul32");
    fs::create_dir_all(dir.join("src")).unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    fs::write(dir.join("Cargo.toml"),format!("[package]\nname=\"pal-imul32-fixture\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    let mut graph = Graph::default();
    let mut fixtures = Vec::new();
    let mut ip = 0x2000u16;
    let mut add = |dst: u8,
                   src: u8,
                   form: u8,
                   imm: u32,
                   segment: u8,
                   bytes: Vec<u8>,
                   actual: bool| {
        let at = Address {
            cs: CS,
            ip: if actual { 0x1081 } else { ip },
        };
        let mut memory = vec![0; at.physical() + bytes.len()];
        memory[at.physical()..].copy_from_slice(&bytes);
        let one = recover(&memory, &[at], 1).unwrap();
        let code = one.nodes[&at].instruction.code();
        assert!(matches!(
            code,
            Code::Imul_r32_rm32 | Code::Imul_r32_rm32_imm8 | Code::Imul_r32_rm32_imm32
        ));
        graph.nodes.extend(one.nodes);
        fixtures.push(format!("Fixture{{ip:{},dst:{dst},src:{src},form:{form},imm:{imm},segment:{segment},bytes:&{bytes:?}}}",at.ip));
        if !actual {
            ip += 8;
        }
    };
    add(3, 3, 1, 75, 0, vec![0x66, 0x6b, 0xdb, 75], true);
    // Every destination/source pair, including all eight self aliases.
    for dst in 0..8 {
        for src in 0..8 {
            let modrm = 0xc0 | dst << 3 | src;
            add(dst, src, 0, 0, 0, vec![0x66, 0x0f, 0xaf, modrm], false);
            add(dst, src, 1, 255, 0, vec![0x66, 0x6b, modrm, 255], false);
            add(
                dst,
                src,
                2,
                0x8000_0001,
                0,
                vec![0x66, 0x69, modrm, 1, 0, 0, 128],
                false,
            );
        }
    }
    // All imm8 bits are independently decoded/emitted, not patched at runtime.
    for imm in 0..=255u8 {
        add(
            3,
            3,
            1,
            u32::from(imm),
            0,
            vec![0x66, 0x6b, 0xdb, imm],
            false,
        );
    }
    for imm in [
        0u32,
        1,
        127,
        128,
        255,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_fffe,
        u32::MAX,
    ] {
        let mut bytes = vec![0x66, 0x69, 0xc1];
        bytes.extend(imm.to_le_bytes());
        add(0, 1, 2, imm, 0, bytes, false);
    }
    // EA16: each ModR/M form, every destination (including EA register aliases),
    // default DS/SS, and ES/CS/SS/DS overrides. Displacement -2 wraps in u16.
    for dst in 0..8 {
        for rm in 0..8 {
            for form in 0..3 {
                let mut bytes = vec![0x66];
                if form == 0 {
                    bytes.extend([0x0f, 0xaf]);
                } else {
                    bytes.push(if form == 1 { 0x6b } else { 0x69 });
                }
                bytes.extend([0x40 | dst << 3 | rm, 0xfe]);
                if form == 1 {
                    bytes.push(128);
                } else if form == 2 {
                    bytes.extend(u32::MAX.to_le_bytes());
                }
                add(
                    dst,
                    8 + rm,
                    form,
                    if form == 1 { 128 } else { u32::MAX },
                    0,
                    bytes,
                    false,
                );
            }
        }
    }
    for (segment, prefix) in [(1, 0x26), (2, 0x2e), (3, 0x36), (4, 0x3e)] {
        for dst in 0..8 {
            for rm in [0, 2, 6, 7] {
                add(
                    dst,
                    8 + rm,
                    1,
                    127,
                    segment,
                    vec![prefix, 0x66, 0x6b, 0x40 | dst << 3 | rm, 0xfe, 127],
                    false,
                );
            }
        }
    }
    for form in 0..3 {
        let mut bytes = vec![0x66];
        if form == 0 {
            bytes.extend([0x0f, 0xaf]);
        } else {
            bytes.push(if form == 1 { 0x6b } else { 0x69 });
        }
        bytes.extend([0x1e, 1, 2]);
        if form == 1 {
            bytes.push(255);
        } else if form == 2 {
            bytes.extend(u32::MAX.to_le_bytes());
        }
        add(
            3,
            16,
            form,
            if form == 1 { 255 } else { u32::MAX },
            0,
            bytes,
            false,
        );
    }
    // Final instruction byte at FFFF; successful emitter IP wraps to zero.
    let wrap = Address { cs: CS, ip: 0xfffc };
    let bytes = [0x66, 0x6b, 0xc0, 0xff];
    let mut memory = vec![0; wrap.physical() + bytes.len()];
    memory[wrap.physical()..].copy_from_slice(&bytes);
    graph
        .nodes
        .extend(recover(&memory, &[wrap], 1).unwrap().nodes);
    fixtures.push(
        "Fixture{ip:65532,dst:0,src:0,form:1,imm:255,segment:0,bytes:&[102,107,192,255]}".into(),
    );
    let emitted = emit_rust(&graph).unwrap();
    assert!(
        !emitted.contains("unsupported 32-bit operand instruction Imul"),
        "IMUL32 not implemented"
    );
    assert!(
        !emitted.contains("iced_x86")
            && !emitted.contains("Decoder")
            && !emitted.contains("unsafe")
    );
    fs::write(dir.join("src/generated.rs"), &emitted).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        HARNESS.replace("@FIXTURES@", &fixtures.join(",\n")),
    )
    .unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo = Path::new(&cargo);
    let tree = run(cargo, &["tree", "--offline", "-e", "features"], &dir);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        !tree.contains("iced-x86") && !tree.contains("winit") && !tree.contains("softbuffer"),
        "{tree}"
    );
    let mutations = [
        (
            "narrow",
            "let product = i64::from(a as i32) * i64::from(b as i32);",
            "let product = i64::from((a as i32).wrapping_mul(b as i32));",
        ),
        (
            "unsigned",
            "i64::from(a as i32) * i64::from(b as i32)",
            "(u64::from(a) * u64::from(b)) as i64",
        ),
        (
            "overflow",
            "product < i64::from(i32::MIN) || product > i64::from(i32::MAX)",
            "false",
        ),
        ("signed-imm", "let b = 4294967295u32;", "let b = 255u32;"),
    ];
    for release in [false, true] {
        let profile = if release { "release" } else { "debug" };
        let args = if release {
            vec!["build", "--offline", "--release", "--quiet"]
        } else {
            vec!["build", "--offline", "--quiet"]
        };
        let binary = dir.join(format!("target/{profile}/pal-imul32-fixture"));
        for (name, old, new) in mutations {
            assert!(emitted.contains(old), "inactive mutation {name}");
            fs::write(dir.join("src/generated.rs"), emitted.replace(old, new)).unwrap();
            let build = run(cargo, &args, &dir);
            assert!(build.status.success(), "mutant {name} build: {build:?}");
            let result = run(&binary, &[name], &dir);
            assert!(
                !result.status.success()
                    && String::from_utf8_lossy(&result.stderr).contains("IMUL32 semantic mismatch"),
                "mutant {name} escaped or failed outside semantic assertion: {result:?}"
            );
            println!("{profile}: REJECTED semantic mutant {name}");
        }
        // Verify the original last, leaving this profile's artifact restored.
        fs::write(dir.join("src/generated.rs"), &emitted).unwrap();
        let build = run(cargo, &args, &dir);
        assert!(build.status.success(), "{profile} build: {build:?}");
        let result = run(&binary, &[], &dir);
        assert!(result.status.success(), "{profile} execution: {result:?}");
        print!("{profile}: {}", String::from_utf8_lossy(&result.stdout));
    }
    println!("verified fixture artifacts: {}", dir.display());
}

const HARNESS: &str = r#"
#![forbid(unsafe_code)]
#![allow(dead_code,unused_variables,unused_parens,unreachable_code)]
use pal_wayland::{runtime::Machine,semantics::{SegmentedAddress as A,Flags,Registers,GuestMemory,A20}};
mod generated;
const CS:u16=0x33c4;
struct Fixture {ip:u16,dst:u8,src:u8,form:u8,imm:u32,segment:u8,bytes:&'static [u8]}
const FIXTURES:&[Fixture]=&[@FIXTURES@];
fn machine()->Machine {
    let mut mz=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {mz[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
    let mut m=Machine::from_mz(&mz,CS).unwrap();m.memory=GuestMemory::new(0x50000,A20::Disabled);m
}
fn setup(m:&mut Machine,f:&Fixture,a:u32,b:u32,seed:u16)->(u32,u32) {
    m.regs=Registers{cs:CS,ip:f.ip,ds:0x200,ss:0x1200,es:0x2200,flags:Flags::from_bits(seed),..Registers::default()};
    for r in 0..8 {m.set32(r,0x9234_fff0+u32::from(r)*0x10000);}
    m.set32(f.dst,a);
    if f.src<8 {m.set32(f.src,b);} else {
        // All EA registers retain nonzero upper halves; pair sums wrap to low EA.
        m.set32(3,0xcafe_fff0);m.set32(5,0xbeef_fff0);
        m.set32(6,0xdead_0031);m.set32(7,0xabcd_0031);
        let at=ea(m,f);
        for seg in [m.regs.ds,m.regs.ss,m.regs.es,m.regs.cs] {m.write32(A::new(seg,at.offset),0x1357_2468).unwrap();}
        m.write32(at,b).unwrap();
    }
    m.memory.write_bytes(A::new(CS,f.ip),f.bytes).unwrap();
    let src=if f.src<8 {m.get32(f.src)} else {m.read32(ea(m,f)).unwrap()};
    let multiplier=if f.form==0 {src} else if f.form==1 {
        // Independent arithmetic sign extension, not production decoder cast.
        if f.imm>=128 {f.imm.wrapping_sub(256)} else {f.imm}
    } else {f.imm};
    (if f.form==0 {m.get32(f.dst)} else {src},multiplier)
}
fn ea(m:&Machine,f:&Fixture)->A {
    let offset=match f.src {
        8=>m.regs.bx.wrapping_add(m.regs.si),9=>m.regs.bx.wrapping_add(m.regs.di),
        10=>m.regs.bp.wrapping_add(m.regs.si),11=>m.regs.bp.wrapping_add(m.regs.di),
        12=>m.regs.si,13=>m.regs.di,14=>m.regs.bp,15=>m.regs.bx,16=>0x203,_=>unreachable!(),
    }.wrapping_sub(2);
    let seg=match f.segment {1=>m.regs.es,2=>m.regs.cs,3=>m.regs.ss,4=>m.regs.ds,_=>if matches!(f.src,10|11|14) {m.regs.ss} else {m.regs.ds}};
    A::new(seg,offset)
}
// i128 mathematical oracle uses subtraction of the modulus, Euclidean low
// bits, and round-trip equality rather than production i32 casts/range checks.
fn oracle(a:u32,b:u32,seed:u16)->(u32,u16) {
    let base=1i128<<32;
    let signed=|x:u32| if x>=0x8000_0000 {i128::from(x)-base} else {i128::from(x)};
    let product=signed(a)*signed(b);let low=product.rem_euclid(base) as u32;
    let overflow=signed(low)!=product;
    (low,(seed&!0x801)|if overflow {0x801} else {0})
}
fn verify(m:&mut Machine,f:&Fixture,expected:Result<(u32,u16),&str>,charged:bool) {
    let regs=m.regs;let gprs:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));
    let memory=m.memory.clone();let planes=m.planar.borrow().clone();let steps=m.steps;
    let result=generated::step(m);
    match expected {
        Ok((value,flags))=>{
            assert!(result.is_ok(),"IMUL32 semantic mismatch: unexpected {result:?}, ip={:x}",f.ip);
            let mut expected_regs=regs;expected_regs.ip=f.ip.wrapping_add(f.bytes.len() as u16);expected_regs.flags=Flags::from_bits(flags);
            match f.dst {0=>expected_regs.ax=value as u16,1=>expected_regs.cx=value as u16,2=>expected_regs.dx=value as u16,3=>expected_regs.bx=value as u16,4=>expected_regs.sp=value as u16,5=>expected_regs.bp=value as u16,6=>expected_regs.si=value as u16,7=>expected_regs.di=value as u16,_=>unreachable!()}
            assert_eq!(m.regs,expected_regs,"IMUL32 semantic mismatch: full registers");
            for r in 0..8 {assert_eq!(m.get32(r),if r==f.dst {value} else {gprs[r as usize]},"IMUL32 semantic mismatch: GPR/upper {r}");}
        },
        Err(reason)=>{
            assert!(matches!(result,Err(ref fault) if fault.0.contains(reason)),"IMUL32 semantic mismatch: expected {reason}, got {result:?}");
            assert_eq!(m.regs,regs,"IMUL32 semantic mismatch: fault registers/IP/flags");
            for r in 0..8 {assert_eq!(m.get32(r),gprs[r as usize],"IMUL32 semantic mismatch: fault upper/GPR");}
        },
    }
    assert_eq!(m.memory,memory,"IMUL32 semantic mismatch: memory mutation");
    assert_eq!(*m.planar.borrow(),planes,"IMUL32 semantic mismatch: planar/latch mutation");
    assert_eq!(m.steps,steps+u64::from(charged),"IMUL32 semantic mismatch: caller-owned steps");
}
fn case(m:&mut Machine,f:&Fixture,a:u32,b:u32,seed:u16) {
    let (a,b)=setup(m,f,a,b,seed);verify(m,f,Ok(oracle(a,b,seed)),true);
}
fn main() {
    let mode=std::env::args().nth(1).unwrap_or_default();let mut m=machine();
    if !mode.is_empty() {
        let f=&FIXTURES[4];
        match mode.as_str() {
            "narrow"|"overflow"=>case(&mut m,f,0x8000_0000,u32::MAX,0),
            "unsigned"=>case(&mut m,f,u32::MAX,u32::MAX,0xffff),
            "signed-imm"=>case(&mut m,&FIXTURES[2],2,2,0),_=>unreachable!(),
        }return;
    }
    let edges=[0,1,2,127,128,255,0x10000,0x7fff_ffff,0x8000_0000,0x8000_0001,0xffff_fffe,u32::MAX];
    let mut total=0u64;
    for f in FIXTURES {
    // setup() overwrites a in self aliases and EA-register destinations;
    // those twelve a values produce the exact same input state.
    let inputs=if f.dst==f.src || f.src>=8 && matches!(f.dst,3|5|6|7) {&edges[..1]} else {&edges[..]};
    for &a in inputs {for b in edges {for seed in [0,0xffff,0xa555,0x5aaa] {
        case(&mut m,f,a,b,seed);total+=1;
    }}}}
    let mut random=0x59ae_3671u32;
    for f in FIXTURES {for _ in 0..32 {
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let a=random;
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let b=random;
        case(&mut m,f,a,b,(random>>16) as u16);total+=1;
    }}
    // Last complete dword, three segment-crossing starts, truncated physical
    // memory, high-memory A20 enabled, and A20 disabled crossing an alias edge.
    let mut faults=0;
    for f in FIXTURES.iter().filter(|f|f.src==15 && f.dst==3 && f.segment==0) {
        for offset in [0xfffcu16,0xfffd,0xfffe,0xffff] {
            setup(&mut m,f,3,7,0xa555);m.set32(3,0xcafe_0000|u32::from(offset.wrapping_add(2)));
            if offset==0xfffc {m.write32(ea(&m,f),7).unwrap();let a=if f.form==0 {m.get32(3)} else {7};let b=if f.form==0 {7} else if f.form==1 {0xffff_ff80} else {u32::MAX};verify(&mut m,f,Ok(oracle(a,b,0xa555)),true);total+=1;} else {verify(&mut m,f,Err("segment boundary"),true);faults+=1;}
        }
        for (a20,ds,offset) in [(A20::Disabled,0x4fff,0x0fu16),(A20::Enabled,0xffff,0x20),(A20::Disabled,0xffff,0x0f)] {
            m=machine();setup(&mut m,f,3,7,0xa555);m.memory.set_a20(a20);
            m.regs.ds=ds;m.set32(3,0xcafe_0000|u32::from(offset.wrapping_add(2)));
            verify(&mut m,f,Err("out of bounds"),true);faults+=1;
        }
        // Disabled A20 successful HMA alias: each byte maps into low RAM.
        m=machine();setup(&mut m,f,3,7,0xa555);m.regs.ds=0xffff;m.set32(3,0xcafe_0022);
        m.write32(ea(&m,f),7).unwrap();
        let a=if f.form==0 {m.get32(3)} else {7};let b=if f.form==0 {7} else if f.form==1 {0xffff_ff80} else {u32::MAX};
        verify(&mut m,f,Ok(oracle(a,b,0xa555)),true);total+=1;
        m=machine();setup(&mut m,f,3,7,0xa555);
        m.memory=GuestMemory::new(0x110000,A20::Disabled);
        m.memory.write_bytes(A::new(CS,f.ip),f.bytes).unwrap();
        // Rejected mixed bus reads cannot change nonzero planes or latches.
        m.planar.borrow_mut().write32(0xa0000,0x9876_5432).unwrap();
        m.video_mode=0x12;m.regs.ds=0x9fff;m.set32(3,0xcafe_0010);
        verify(&mut m,f,Err("mixed RAM/planar"),true);faults+=1;
        m.regs.ds=0xafff;m.set32(3,0xcafe_0010);
        verify(&mut m,f,Err("mixed RAM/planar"),true);faults+=1;
        m=machine();
    }
    // Guard every byte (prefix/opcode/ModR/M/displacement/immediate); no charge.
    let mut guards=0;
    for f in [ &FIXTURES[0],FIXTURES.iter().find(|f|f.src==15 && f.form==2).unwrap() ] {
        for n in 0..f.bytes.len() {
            setup(&mut m,f,7,3,0xa555);
            m.memory.write8(A::new(CS,f.ip+n as u16),f.bytes[n]^1).unwrap();
            verify(&mut m,f,Err("code changed"),false);guards+=1;
        }
    }
    println!("IMUL32: {} encodings; {total} i128-oracle executions; {faults} atomic read faults; {guards} exact-byte guard faults; full GPR/upper/FLAGS/memory/planar/IP/step snapshots; decoder-free",FIXTURES.len());
}
"#;
