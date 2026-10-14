// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored, decoder-free generated execution: RAM-only stacks and atomic REP units.
#![cfg(feature = "analysis")]
mod common;
use common::FixtureCargo;
use pal_wayland::lift::{Address, Graph, emit_rust, emit_rust_variants, recover};
use std::{fs, process::Command};

const FIXTURES: &[(&str, &[u8])] = &[
    ("PUSH", &[0x50]),
    ("POP", &[0x58]),
    ("PUSHF", &[0x9c]),
    ("POPF", &[0x9d]),
    ("PUSHA", &[0x60]),
    ("POPA", &[0x61]),
    ("ENTER", &[0xc8, 8, 0, 0]),
    ("LEAVE", &[0xc9]),
    ("CALL", &[0xe8, 0, 0]),
    ("CALL_FAR", &[0x9a, 0, 2, 0, 2]),
    ("RET", &[0xc3]),
    ("RET_CLEAN", &[0xc2, 8, 0]),
    ("RETF", &[0xcb]),
    ("RETF_CLEAN", &[0xca, 8, 0]),
    ("PUSH32", &[0x66, 0x50]),
    ("POP32", &[0x66, 0x58]),
    ("PUSH_MEM", &[0xff, 0x37]),
    ("PUSH_MEM32", &[0x66, 0xff, 0x37]),
    ("CALL_MEM", &[0xff, 0x17]),
    ("CALL_MEM_FAR", &[0xff, 0x1f]),
    ("POP_SS", &[0x17]),
    ("POP_MEM", &[0x8f, 0x07]),
    ("POP_MEM32", &[0x66, 0x8f, 0x07]),
    ("MOVSB", &[0xf3, 0xa4]),
    ("MOVSW", &[0xf3, 0xa5]),
    ("MOVSD", &[0xf3, 0x66, 0xa5]),
    ("CMPSW", &[0xf3, 0xa7]),
    ("CMPSD", &[0xf3, 0x66, 0xa7]),
    ("STOSW", &[0xf3, 0xab]),
    ("LODSW", &[0xf3, 0xad]),
    ("SCASW", &[0xf3, 0xaf]),
    ("LES", &[0xc4, 0x07]),
    ("LDS", &[0xc5, 0x07]),
];

fn one(at: Address, bytes: &[u8]) -> Graph {
    let mut memory = vec![0; at.physical() + bytes.len()];
    memory[at.physical()..].copy_from_slice(bytes);
    recover(&memory, &[at], 1).unwrap()
}

#[test]
fn generated_planar_stack_and_rep_execute_debug_and_optimized() {
    let dir = common::unique_dir("pal-planar-stack");
    fs::create_dir_all(dir.join("src")).unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    fs::write(dir.join("Cargo.toml"), format!("[package]\nname=\"pal-planar-stack-fixture\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    let mut graph = Graph::default();
    let mut constants = String::new();
    let mut encoded = Vec::new();
    for (index, (name, bytes)) in FIXTURES.iter().enumerate() {
        let ip = 0x100 + index as u16 * 0x10;
        graph
            .nodes
            .extend(one(Address { cs: 0x200, ip }, bytes).nodes);
        constants.push_str(&format!("const {name}: usize = {index};\n"));
        encoded.push(format!("({ip}, &{bytes:?})"));
    }
    // Mutable guards and the special Borland sync alternative must not read
    // planar instruction bytes before the RAM-only instruction gate.
    let mut mutable = one(
        Address {
            cs: 0x9fff,
            ip: 0x000f,
        },
        &[0xb8, 1, 0],
    );
    let node = mutable.nodes.values_mut().next().unwrap();
    node.mutable_immediate =
        Some(pal_wayland::mutable_immediate::analyze(&node.instruction, &node.bytes).unwrap());
    graph.nodes.extend(mutable.nodes);
    graph.nodes.extend(
        one(
            Address {
                cs: 0xa000,
                ip: 0x300,
            },
            &[0xcd, 0x3d],
        )
        .nodes,
    );
    let generated = emit_rust(&graph).unwrap();
    assert!(!generated.contains("m.regs.pop16(&m.memory)"));
    assert!(!generated.contains("m.xms.call("));
    let at = Address {
        cs: 0x9fff,
        ip: 0x10,
    };
    let variants = emit_rust_variants(&[one(at, &[0x50]), one(at, &[0xb8, 1, 0])]).unwrap();
    fs::write(dir.join("src/generated.rs"), &generated).unwrap();
    fs::write(dir.join("src/variants.rs"), variants).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        HARNESS
            .replace("@CONSTANTS@", &constants)
            .replace("@FIXTURES@", &encoded.join(",")),
    )
    .unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    for release in [false, true] {
        let profile = if release { "release" } else { "debug" };
        let mut build = Command::new(&cargo);
        build.args(["build", "--offline", "--quiet"]);
        if release {
            build.arg("--release");
        }
        let output = build
            .current_dir(&dir)
            .env_remove("CARGO_TARGET_DIR")
            .env("RUSTFLAGS", "-C overflow-checks=yes")
            .fixture_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{profile} build: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new(dir.join(format!("target/{profile}/pal-planar-stack-fixture")))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{profile} execution: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        println!("{profile}: {}", String::from_utf8_lossy(&output.stdout));
    }
    // Remove only the complete-frame preflights; per-word helper checks remain.
    // This mutant compiles and must fail on partial PUSHA effects.
    let old = "for unit in 0..8u16 { m.validate_ram_span(SegmentedAddress::new(m.regs.ss,sp.wrapping_sub(2 + unit * 2)),2)?; }";
    assert!(generated.contains(old));
    fs::write(dir.join("src/generated.rs"), generated.replace(old, "")).unwrap();
    let output = Command::new(&cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(&dir)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTFLAGS", "-C overflow-checks=yes")
        .fixture_output()
        .unwrap();
    assert!(
        output.status.success(),
        "mutant build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(dir.join("target/debug/pal-planar-stack-fixture"))
        .output()
        .unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("atomic registers"),
        "mutant escaped/failed outside semantic assertion: {output:?}"
    );
    println!("whole-frame PUSHA preflight mutant rejected by execution assertion");
    // MOVS mutation must expose latch changes, not just a returned fault.
    fs::write(dir.join("src/generated.rs"), &generated).unwrap();
    let old = "m.validate_span(dest,2)?;";
    assert!(generated.contains(old));
    fs::write(dir.join("src/generated.rs"), generated.replace(old, "")).unwrap();
    let output = Command::new(&cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(&dir)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTFLAGS", "-C overflow-checks=yes")
        .fixture_output()
        .unwrap();
    assert!(
        output.status.success(),
        "REP mutant build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(dir.join("target/debug/pal-planar-stack-fixture"))
        .output()
        .unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("atomic planes/latches"),
        "REP mutant escaped/failed outside semantic assertion: {output:?}"
    );
    println!("MOVS destination preflight mutant rejected by latch assertion");
    fs::remove_dir_all(dir).unwrap();
}

const HARNESS: &str = r#"
mod generated;
mod variants;
use pal_wayland::{runtime::Machine, semantics::*};
@CONSTANTS@
const FIXTURES: &[(u16,&[u8])] = &[@FIXTURES@];
fn machine(index:usize,mode:u8)->Machine {
    let mut bytes=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {
        bytes[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());
    }
    let mut m=Machine::from_mz(&bytes,0x1010).unwrap();
    m.video_mode=mode;
    m.regs=Registers{cs:0x200,ip:FIXTURES[index].0,ds:0x6000,es:0x7000,ss:0x8000,sp:0x8000,bp:0x8000,ax:0x1234,bx:0x100,cx:1,dx:0x2345,si:0x100,di:0x200,..Registers::default()};
    m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,m.regs.ip),FIXTURES[index].1).unwrap();
    // Nonzero data on all planes makes unintended reads observable; writes do
    // not load latches, so the fault snapshot intentionally starts at zero.
    m.planar.borrow_mut().write_linear(0xa0000,&[0x71,0x72,0x73,0x74,0x75,0x76,0x77,0x78]).unwrap();
    assert_eq!(m.planar.borrow().latches(),[0;4]);
    m
}
fn fault(m:&mut Machine) {
    let regs=m.regs;let ram=m.memory.clone();let video=m.planar.borrow().clone();
    let upper:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));let shadow=m.irq_shadow;
    assert!(generated::step(m).is_err(),"expected fail-closed fault at {:04x}:{:04x}, SS:SP {:04x}:{:04x}, BP {:04x}",regs.cs,regs.ip,regs.ss,regs.sp,regs.bp);
    assert_eq!(m.regs,regs,"atomic registers");
    assert_eq!(m.memory,ram,"atomic RAM");
    assert_eq!(*m.planar.borrow(),video,"atomic planes/latches");
    assert_eq!(m.irq_shadow,shadow);
    for r in 0..8 {assert_eq!(m.get32(r),upper[r as usize]);}
}
fn main() {
    // Each word alone is legal, but the far pointer straddles RAM/device RAM.
    // Exercise the emitted instruction, including preserved read latches.
    for index in [LES,LDS] {
        let mut m=machine(index,0x12);m.regs.ds=0x9fff;m.regs.bx=0xe;fault(&mut m);
        let mut m=machine(index,0x12);m.regs.bx=0xfffd;fault(&mut m);
        let mut m=machine(index,0x13);m.regs.ds=0x9fff;m.regs.bx=0xe;
        m.memory.write_bytes(SegmentedAddress::new(m.regs.ds,m.regs.bx),&[0x34,0x12,0x78,0x56]).unwrap();
        generated::step(&mut m).unwrap();assert_eq!(m.regs.ax,0x1234);
        assert_eq!(if index==LES {m.regs.es} else {m.regs.ds},0x5678);
        assert_eq!(m.planar.borrow().latches(),[0;4]);
    }
    let stacks=[PUSH,POP,PUSHF,POPF,PUSHA,POPA,ENTER,LEAVE,CALL,CALL_FAR,RET,RET_CLEAN,RETF,RETF_CLEAN,PUSH32,POP32,PUSH_MEM,PUSH_MEM32,CALL_MEM,CALL_MEM_FAR,POP_SS,POP_MEM,POP_MEM32];
    for index in stacks {
        let push=matches!(index,PUSH|PUSHF|PUSHA|ENTER|CALL|CALL_FAR|PUSH32|PUSH_MEM|PUSH_MEM32|CALL_MEM|CALL_MEM_FAR);
        let width=if matches!(index,PUSH32|PUSH_MEM32) {4} else {2};
        for (ss,at) in [(0x9fff,0x10u16),(0xa000,0),(0xafff,0),(0x9fff,0x0f),(0xafff,0x0f)] {
            let mut m=machine(index,0x12);m.regs.ss=ss;m.regs.sp=if push {at.wrapping_add(width)} else {at};m.regs.bp=at;
            // PUSH/CALL memory sources are also planar: failing stack checks
            // must happen before reading the source and loading its latches.
            m.regs.ds=0xa000;m.regs.bx=0;
            fault(&mut m);
        }
    }
    // Early valid RAM words then aperture on a later word: full-frame preflight.
    for (index,ss,sp) in [(PUSHA,0xafff,0x1c),(POPA,0x9fff,8),(CALL_FAR,0xafff,0x12),(RETF,0x9fff,0x0e),(RETF_CLEAN,0x9fff,0x0e)] {
        let mut m=machine(index,0x12);m.regs.ss=ss;m.regs.sp=sp;fault(&mut m);
    }
    // Legal SP wrap is wordwise, not an invalid contiguous 16-byte segment span.
    for sp in [0u16,2,8,0xfffe] {
        let mut m=machine(PUSHA,0x12);m.regs.sp=sp;let before=m.regs;
        generated::step(&mut m).unwrap();assert_eq!(m.regs.sp,sp.wrapping_sub(16));
        m.regs.ip=FIXTURES[POPA].0;m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,m.regs.ip),FIXTURES[POPA].1).unwrap();
        generated::step(&mut m).unwrap();let mut expected=before;expected.ip=FIXTURES[POPA].0+1;
        assert_eq!(m.regs,expected,"wordwise wrapped PUSHA/POPA");
    }
    // RETF and CALL far accept two words spanning SP wrap, rejecting odd words.
    let mut m=machine(CALL_FAR,0x12);m.regs.sp=2;let return_ip=m.regs.ip+5;
    generated::step(&mut m).unwrap();assert_eq!(m.regs.sp,0xfffe);
    m.regs.cs=0x200;m.regs.ip=FIXTURES[RETF].0;m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,m.regs.ip),FIXTURES[RETF].1).unwrap();
    generated::step(&mut m).unwrap();assert_eq!((m.regs.sp,m.regs.cs,m.regs.ip),(2,0x200,return_ip));
    for (index,sp) in [(PUSHA,1),(POPA,0xffff),(CALL_FAR,1),(RETF,0xffff),(PUSH32,1),(POP32,0xfffd),(ENTER,1),(LEAVE,0xffff)] {
        let mut m=machine(index,0x12);m.regs.sp=sp;m.regs.bp=sp;fault(&mut m);
    }
    // Mode13 keeps the aperture as RAM for every generated stack family.
    for index in stacks {
        let mut m=machine(index,0x13);m.regs.ss=0x9fff;m.regs.sp=0x20;m.regs.bp=0x20;
        m.memory.write16(SegmentedAddress::new(m.regs.ds,m.regs.bx),0x1234).unwrap();
        m.memory.write16(SegmentedAddress::new(m.regs.ds,m.regs.bx+2),0x200).unwrap();
        generated::step(&mut m).unwrap();assert_eq!(m.planar.borrow().latches(),[0;4]);
    }
    // Failed first REP units never change SI/DI/CX, memory or latches.
    for (index,width) in [(MOVSB,1u16),(MOVSW,2),(MOVSD,4),(CMPSW,2),(CMPSD,4)] {
        for (es,di) in [(0x9fff,0x10-width+1),(0xafff,0x10-width+1),(0x7000,0xffff)] {
            if width==1 && es!=0x7000 {continue;}
            let mut m=machine(index,0x12);m.regs.ds=0xa000;m.regs.si=0;m.regs.es=es;m.regs.di=di;m.regs.cx=2;
            if width==1 {m.memory=GuestMemory::new(0x70001,A20::Disabled);m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,m.regs.ip),FIXTURES[index].1).unwrap();}
            fault(&mut m);
        }
    }
    for index in [STOSW,LODSW,SCASW] {
        let mut m=machine(index,0x12);m.regs.ds=0x9fff;m.regs.si=0xf;m.regs.es=0x9fff;m.regs.di=0xf;fault(&mut m);
    }
    // One completed unit remains; the next mixed-aperture unit does not load
    // its source latch. Compare against an independent single-unit execution.
    for (index,width) in [(MOVSW,2u16),(MOVSD,4)] {
        let mut m=machine(index,0x12);m.regs.ds=0xa000;m.regs.si=0;m.regs.es=0x9fff;m.regs.di=0x10-width-1;m.regs.cx=1;
        generated::step(&mut m).unwrap();let video=m.planar.borrow().clone();let ram=m.memory.clone();let regs=m.regs;
        let mut two=machine(index,0x12);two.regs.ds=0xa000;two.regs.si=0;two.regs.es=0x9fff;two.regs.di=0x10-width-1;two.regs.cx=2;
        assert!(generated::step(&mut two).is_err());let mut expected=regs;expected.cx=1;expected.ip=FIXTURES[index].0;
        assert_eq!(two.regs,expected);assert_eq!(two.memory,ram);assert_eq!(*two.planar.borrow(),video,"atomic planes/latches after completed unit");
    }
    // Successful full-planar MOVS is still routed and updates the last-byte latch.
    for (index,width) in [(MOVSB,1u16),(MOVSW,2),(MOVSD,4)] {
        let mut m=machine(index,0x12);m.regs.ds=0xa000;m.regs.si=0;m.regs.es=0xa000;m.regs.di=0x100;
        generated::step(&mut m).unwrap();assert_eq!(m.regs.cx,0);assert_eq!(m.regs.si,width);assert_eq!(m.planar.borrow().latches(),[0x70+width as u8;4]);
        let mut copied=vec![0;width as usize];m.planar.borrow_mut().read_linear(0xa0100,&mut copied).unwrap();assert_eq!(copied,(0..width).map(|i|0x71+i as u8).collect::<Vec<_>>());
    }
    // The service must not allocate XMS before discovering a bad return frame.
    let mut m=machine(PUSH,0x12);m.regs.cs=0xf000;m.regs.ip=0x100;m.xms_entry_registered=true;
    m.regs.ax=0x0900;m.regs.dx=1;m.regs.ss=0x9fff;m.regs.sp=0x0e;fault(&mut m);
    m.regs.ax=0x0900;m.regs.dx=1;m.xms_call().unwrap();assert_eq!(m.regs.dx,1,"XMS allocation happened before rejected return frame");
    let mut m=machine(PUSH,0x12);m.regs.cs=0xf000;m.regs.ip=0x100;m.xms_entry_registered=true;m.regs.ax=0;
    m.memory.write16(SegmentedAddress::new(m.regs.ss,m.regs.sp),0x1234).unwrap();
    m.memory.write16(SegmentedAddress::new(m.regs.ss,m.regs.sp+2),0x4567).unwrap();
    generated::step(&mut m).unwrap();assert_eq!((m.regs.cs,m.regs.ip,m.regs.sp,m.regs.ax),(0x4567,0x1234,0x8004,0x300));
    // Both variant selector and mutable/sync paths reject instruction fetches
    // before reading device memory, changing regs or charging a step.
    for (cs,ip,bytes,variant) in [(0x9fff,0x10,&[0x50][..],true),(0x9fff,0xf,&[0xb8,1,0][..],false),(0xa000,0x300,&[0xcd,0x3d][..],false)] {
        let mut m=machine(PUSH,0x12);m.regs.cs=cs;m.regs.ip=ip;m.memory.write_bytes(SegmentedAddress::new(cs,ip),bytes).unwrap();let before=m.regs;let steps=m.steps;let video=m.planar.borrow().clone();
        let result=if variant {variants::step(&mut m)} else {generated::step(&mut m)};
        assert!(result.is_err());assert_eq!(m.regs,before);assert_eq!(m.steps,steps);assert_eq!(*m.planar.borrow(),video);
    }
    println!("23 stack encodings: alias/mixed/frame faults atomic, SP wrapping and mode13 RAM passed; REP byte/word/dword latch atomicity and prior units passed; XMS frame and code guards passed; no decoder/GUI/audio/assets");
}
"#;
