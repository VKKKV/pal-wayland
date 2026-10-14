// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]

mod common;
use common::FixtureCargo;
use pal_wayland::lift::{Address, Graph, emit_rust, recover};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

// Real installer instruction: 221f:0d09 = SHLD DX,AX,CL (0f a5 c2).
// All fixtures below are synthetic, not copied executable/assets.
const CS: u16 = 0x221f;

fn run(program: &Path, args: &[&str], dir: &Path) -> Output {
    Command::new(program)
        .args(args)
        // An outer CI/test target dir must not relocate nested fixture binaries.
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(dir)
        .fixture_output()
        .unwrap()
}

#[test]
fn generated_word_double_shifts_execute_against_bit_serial_and_host_oracles() {
    let dir = common::unique_dir("pal-shift16");
    fs::create_dir_all(dir.join("src")).unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    fs::write(dir.join("Cargo.toml"), format!("[package]\nname=\"pal-shift16-fixture\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    // One instruction per root, avoiding artificial sequential code discovery.
    let mut graph = Graph::default();
    let mut fixtures = Vec::new();
    // Leave the exact installer CS:IP isolated from the synthetic cases.
    let mut ip = 0x2000u16;
    for right in [false, true] {
        for form in 0..8u8 {
            for immediate in [false, true] {
                let counts: Vec<u8> = if !immediate {
                    vec![0]
                } else if form == 0 {
                    (0..=255).collect()
                } else {
                    vec![0, 1, 2, 15, 16, 17, 31, 32, 33, 48, 255]
                };
                for count in counts {
                    // DX,AX; CX,AX; DX,CX; CX,CX; AX,AX;
                    // [BX+SI],BX; [BP+DI],CX; ES:[BX+SI],AX.
                    let (prefix, modrm) = match form {
                        0 => (None, 0xc2),
                        1 => (None, 0xc1),
                        2 => (None, 0xca),
                        3 => (None, 0xc9),
                        4 => (None, 0xc0),
                        5 => (None, 0x18),
                        6 => (None, 0x0b),
                        7 => (Some(0x26), 0x00),
                        _ => unreachable!(),
                    };
                    let mut bytes = Vec::new();
                    if let Some(p) = prefix {
                        bytes.push(p);
                    }
                    bytes.extend([
                        0x0f,
                        match (right, immediate) {
                            (false, false) => 0xa5,
                            (false, true) => 0xa4,
                            (true, false) => 0xad,
                            (true, true) => 0xac,
                        },
                        modrm,
                    ]);
                    if immediate {
                        bytes.push(count);
                    }
                    let at = Address { cs: CS, ip };
                    let mut memory = vec![0; at.physical() + bytes.len()];
                    memory[at.physical()..].copy_from_slice(&bytes);
                    let one = recover(&memory, &[at], 1).unwrap();
                    assert_eq!(one.nodes[&at].bytes, bytes);
                    graph.nodes.extend(one.nodes);
                    fixtures.push(format!("Fixture{{ip:{ip},right:{right},form:{form},immediate:{immediate},count:{count},bytes:&{bytes:?}}}"));
                    ip += 8;
                }
            }
        }
    }
    let installer = Address { cs: CS, ip: 0x0d09 };
    let mut memory = vec![0; installer.physical() + 3];
    memory[installer.physical()..].copy_from_slice(&[0x0f, 0xa5, 0xc2]);
    graph
        .nodes
        .extend(recover(&memory, &[installer], 1).unwrap().nodes);
    fixtures.push(
        "Fixture{ip:3337,right:false,form:0,immediate:false,count:0,bytes:&[15,165,194]}".into(),
    );
    // Address-size 32 must remain an explicit rejection, even count zero.
    let bad = Address { cs: CS, ip };
    let mut memory = vec![0; bad.physical() + 5];
    memory[bad.physical()..].copy_from_slice(&[0x67, 0x0f, 0xa4, 0x00, 0x00]);
    graph
        .nodes
        .extend(recover(&memory, &[bad], 1).unwrap().nodes);
    let emitted = emit_rust(&graph).unwrap();
    assert!(!emitted.contains("unsafe"));
    assert!(!emitted.contains("unsupported instruction shld"));
    assert!(!emitted.contains("unsupported instruction shrd"));
    fs::write(dir.join("src/generated.rs"), &emitted).unwrap();
    let harness = HARNESS
        .replace("@FIXTURES@", &fixtures.join(",\n"))
        .replace("@BAD_IP@", &ip.to_string());
    fs::write(dir.join("src/main.rs"), harness).unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo = Path::new(&cargo);
    let build = run(cargo, &["build", "--offline", "--quiet"], &dir);
    assert!(build.status.success(), "build {dir:?}: {build:?}");
    let tree = run(cargo, &["tree", "--offline", "-e", "features"], &dir);
    assert!(tree.status.success(), "{tree:?}");
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        !tree.contains("iced-x86") && !tree.contains("winit") && !tree.contains("softbuffer"),
        "{tree}"
    );
    let binary = dir.join("target/debug/pal-shift16-fixture");
    let cases = run(&binary, &["cases"], &dir);
    assert!(cases.status.success(), "{cases:?}");
    fs::write(dir.join("cases.tsv"), cases.stdout).unwrap();
    // C/asm is confined to TMPDIR; all repository/runtime Rust remains safe.
    if cfg!(target_arch = "x86_64") && cfg!(target_os = "linux") {
        fs::write(dir.join("oracle.c"), HOST_ORACLE).unwrap();
        let build = run(
            Path::new("cc"),
            &[
                "-O2", "-Wall", "-Wextra", "-Werror", "oracle.c", "-o", "oracle",
            ],
            &dir,
        );
        assert!(build.status.success(), "host oracle {build:?}");
        let oracle = run(&dir.join("oracle"), &[], &dir);
        assert!(oracle.status.success(), "host execution {oracle:?}");
        print!("{}", String::from_utf8_lossy(&oracle.stdout));
    }
    let result = run(&binary, &[], &dir);
    assert!(result.status.success(), "execution {dir:?}: {result:?}");
    print!("{}", String::from_utf8_lossy(&result.stdout));
    // Mutants must fail semantic assertions, not fail to compile.
    let original = "let count = (";
    assert!(emitted.contains(original));
    let mutants = [
        ("count-mask", emitted.replace("& 31;", "& 15;")),
        (
            "carry",
            emitted.replace("Flags::CF, carry", "Flags::CF, !carry"),
        ),
        (
            "overflow",
            emitted.replace(
                "(original ^ value) & 0x8000 != 0",
                "(original ^ value) & 0x8000 == 0",
            ),
        ),
        (
            "undefined",
            emitted.replace(
                "return Err(m.fault(\"unsupported undefined 16-bit SHLD/SHRD count > 16\"));",
                "return Ok(());",
            ),
        ),
    ];
    for (name, source) in mutants {
        assert_ne!(source, emitted, "inactive mutant {name}");
        fs::write(dir.join("src/generated.rs"), source).unwrap();
        let build = run(cargo, &["build", "--offline", "--quiet"], &dir);
        assert!(build.status.success(), "mutant build {name}: {build:?}");
        let result = run(&binary, &[], &dir);
        assert!(!result.status.success(), "mutant survived: {name}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("assertion")
                || String::from_utf8_lossy(&result.stderr).contains("undefined count must fault"),
            "{result:?}"
        );
        println!("REJECTED semantic mutant {name}");
    }
    fs::write(dir.join("src/generated.rs"), emitted).unwrap();
    let release = run(cargo, &["build", "--offline", "--release", "--quiet"], &dir);
    assert!(release.status.success(), "release build: {release:?}");
    let result = run(&dir.join("target/release/pal-shift16-fixture"), &[], &dir);
    assert!(result.status.success(), "release execution: {result:?}");
    print!("RELEASE {}", String::from_utf8_lossy(&result.stdout));
}

const HARNESS: &str = r#"
#![forbid(unsafe_code)]
#![allow(dead_code, unused_variables, unused_parens, unreachable_code)]
use pal_wayland::{runtime::Machine,semantics::{SegmentedAddress as A,Flags,Registers,GuestMemory,A20}};
mod generated;
const CS:u16=0x221f;
struct Fixture { ip:u16, right:bool, form:u8, immediate:bool, count:u8, bytes:&'static [u8] }
const FIXTURES:&[Fixture]=&[@FIXTURES@];
fn machine()->Machine {
    let mut mz=vec![0;96];
    for (i,w) in [0x5a4d_u16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate(){mz[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
    let mut m=Machine::from_mz(&mz,CS).unwrap();
    for f in FIXTURES { m.memory.write_bytes(A::new(CS,f.ip),f.bytes).unwrap(); }
    m.memory.write_bytes(A::new(CS,@BAD_IP@),&[0x67,0x0f,0xa4,0x00,0x00]).unwrap();m
}
fn setup(m:&mut Machine,f:&Fixture,raw:u8,a:u16,b:u16,seed:u16)->(u16,u16,A) {
    m.regs=Registers {cs:CS,ip:f.ip,ax:b,dx:a,cx:0x9a00|u16::from(raw),bx:0x2357,si:0x3412,bp:0x4321,di:0x1001,ds:0x6000,ss:0x7000,es:0x8000,sp:0x9000,flags:Flags::from_bits(seed),..Registers::default()};
    let ea=match f.form {6=>A::new(m.regs.ss,m.regs.bp.wrapping_add(m.regs.di)),7=>A::new(m.regs.es,m.regs.bx.wrapping_add(m.regs.si)),_=>A::new(m.regs.ds,m.regs.bx.wrapping_add(m.regs.si))};
    let (dst,src)=match f.form {0=>(a,b),1=>(m.regs.cx,b),2=>(a,m.regs.cx),3=>(m.regs.cx,m.regs.cx),4=>(b,b),5=>(a,m.regs.bx),6=>(a,m.regs.cx),7=>(a,b),_=>unreachable!()};
    // Distinct wrong-segment canaries detect DS/SS/ES selection mistakes.
    for seg in [0x6000,0x7000,0x8000] {m.write16(A::new(seg,ea.offset),0xbabe).unwrap();}
    if f.form>=5 {m.write16(ea,dst).unwrap();}
    (dst,src,ea)
}
// Independent bit-serial oracle, not the emitter's concatenation formula.
fn expected(mut dst:u16,mut src:u16,count:u8,right:bool,mut flags:u16)->(u16,u16) {
    let before=dst;let n=count&31;assert!(n<=16);let mut carry=false;
    for _ in 0..n { if right {carry=dst&1!=0;dst=(dst>>1)|((src&1)<<15);src>>=1;} else {carry=dst&0x8000!=0;dst=(dst<<1)|(src>>15);src<<=1;} }
    if n!=0 {for (bit,on) in [(1,carry),(4,(dst as u8).count_ones()%2==0),(0x40,dst==0),(0x80,dst&0x8000!=0)] {if on {flags|=bit;} else {flags&=!bit;}}
    if n==1 {if (before^dst)&0x8000!=0 {flags|=0x800;} else {flags&=!0x800;}}}
    (dst,flags)
}
fn pairs()->Vec<(u16,u16)> {
    let mut v=vec![(0,0),(0xffff,0),(0,0xffff),(0x8000,1),(1,0x8000),(0x5555,0xaaaa),(0xaaaa,0x5555),(0xffff,0xffff)];
    let mut r=0x12345678u32;for _ in 0..12 {r^=r<<13;r^=r>>17;r^=r<<5;v.push((r as u16,(r>>16) as u16));}v
}
fn main(){
    let cases=std::env::args().nth(1).as_deref()==Some("cases");
    let mut m=machine();let mut total=0;let mut defined=0;let mut faults=0;
    let host=if cases {None} else {std::fs::read_to_string("oracle.tsv").ok()};
    let mut host=host.as_deref().map(str::lines);
    for f in FIXTURES {
      let counts:Vec<u8>=if f.immediate {vec![f.count]} else {(0..=255).collect()};
      for raw in counts {for (a,b) in pairs() {for seed in [0x0202u16,0x0ed7] {
        let (dst,src,ea)=setup(&mut m,f,raw,a,b,seed);let n=raw&31;
        if cases {if n<=16 {println!("{} {dst} {src} {raw} {seed}",u8::from(f.right));}continue;}
        let before=m.regs;let steps=m.steps;
        if n>16 {
            let words:Vec<_>=[0x6000,0x7000,0x8000].map(|seg|m.read16(A::new(seg,ea.offset)).unwrap()).into();
            let error=generated::step(&mut m).expect_err("undefined count must fault");
            assert!(error.0.contains("unsupported undefined 16-bit SHLD/SHRD count > 16"),"{error:?}");
            assert_eq!(m.regs,before,"undefined count architectural mutation");
            assert_eq!([0x6000,0x7000,0x8000].map(|seg|m.read16(A::new(seg,ea.offset)).unwrap()).to_vec(),words,"undefined memory mutation");faults+=1;
        } else {
            let (value,flags)=expected(dst,src,raw,f.right,seed);
            if let Some(lines)=host.as_mut() {
                let h:Vec<u16>=lines.next().expect("host row").split_whitespace().map(|x|x.parse().unwrap()).collect();
                assert_eq!(h[0],value,"host value raw={raw}");
                let mask=if n==0 {0x8d5} else if n==1 {0x8c5} else {0xc5};
                assert_eq!(h[1]&mask,flags&mask,"host defined flags raw={raw}");
            }
            generated::step(&mut m).unwrap();let mut want=before;want.ip=f.ip+f.bytes.len() as u16;want.flags=Flags::from_bits(flags);
            match f.form {0|2=>want.dx=value,1|3=>want.cx=value,4=>want.ax=value,_=>assert_eq!(m.read16(ea).unwrap(),value)};
            assert_eq!(m.regs,want,"register/flag state form={} right={} raw={raw}",f.form,f.right);
            if f.form>=5 {for seg in [0x6000,0x7000,0x8000] {if seg!=ea.segment {assert_eq!(m.read16(A::new(seg,ea.offset)).unwrap(),0xbabe);}}}
            defined+=1;
        }
        assert_eq!(m.steps,steps+1,"charged exactly one instruction");total+=1;
      }}}
    }
    if cases {return;}
    if let Some(mut lines)=host {assert!(lines.next().is_none(),"extra host rows");}
    // EA wrapping, final valid word, segment overrun, physical RAM boundary.
    for f in FIXTURES.iter().filter(|f| !f.immediate && f.form>=5) {
        for raw in [0u8,1,16,17,32,48] {for offset in [0u16,0xfffe,0xffff] {
            setup(&mut m,f,raw,0x8123,0x4567,0xed7);
            if f.form==6 {m.regs.bp=0xfff0;m.regs.di=offset.wrapping_sub(0xfff0);} else {m.regs.bx=0xfff0;m.regs.si=offset.wrapping_sub(0xfff0);}
            let segment=match f.form {6=>m.regs.ss,7=>m.regs.es,_=>m.regs.ds};let ea=A::new(segment,offset);
            if offset!=0xffff {m.write16(ea,0x8123).unwrap();}
            let before=m.regs;let bytes=m.memory.read_linear(0,m.memory.len()).unwrap();
            let r=generated::step(&mut m);
            if raw&31>16 || offset==0xffff {let e=r.unwrap_err();assert_eq!(m.regs,before);assert_eq!(m.memory.read_linear(0,m.memory.len()).unwrap(),bytes);if raw&31>16 {assert!(e.0.contains("undefined"));}else{assert!(e.0.contains("segment"),"{e:?}");}}
            else {r.unwrap();let src=match f.form {5=>0xfff0,6=>before.cx,_=>before.ax};assert_eq!(m.read16(ea).unwrap(),expected(0x8123,src,raw,f.right,0xed7).0);}
        }}
        let mut short=machine();setup(&mut short,f,0,0x1234,0x5678,0xed7);
        let ea=match f.form {6=>A::new(short.regs.ss,short.regs.bp+short.regs.di),7=>A::new(short.regs.es,short.regs.bx+short.regs.si),_=>A::new(short.regs.ds,short.regs.bx+short.regs.si)};
        let len=usize::from(ea.segment)*16+usize::from(ea.offset)+1;
        short.memory=GuestMemory::from_bytes(short.memory.read_linear(0,len).unwrap(),A20::Disabled);
        let before=short.regs;assert!(generated::step(&mut short).is_err());assert_eq!(short.regs,before);
    }
    // Full code guard precedes undefined-count/EA handling, including count zero.
    for f in FIXTURES.iter().filter(|f| !f.immediate || matches!(f.count,0|1|17)) {for raw in [0,1,17] {
      for index in 0..f.bytes.len() {setup(&mut m,f,raw,1,2,0xed7);m.write8(A::new(CS,f.ip+index as u16),f.bytes[index]^1).unwrap();let before=m.regs;let steps=m.steps;
        let e=generated::step(&mut m).unwrap_err();assert!(e.0.contains("code changed"));assert_eq!(m.regs,before);assert_eq!(m.steps,steps);m.write8(A::new(CS,f.ip+index as u16),f.bytes[index]).unwrap();
      }
    }}
    m.regs.ip=@BAD_IP@;let before=m.regs;let e=generated::step(&mut m).unwrap_err();assert!(e.0.contains("address")||e.0.contains("effective address"),"{e:?}");assert_eq!(m.regs,before);
    println!("PASS {total} generated executions: {defined} defined/host-differential, {faults} fail-closed undefined counts; aliases, EA16 wrapping/bounds, code guards, flags, decoder-free runtime");
}
"#;

const HOST_ORACLE: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
static void zero_access(int right) {
    unsigned short *p=mmap(NULL,4096,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(p==MAP_FAILED) exit(2);
    pid_t child=fork(); if(child<0) exit(3);
    if(child==0) {unsigned short src=1; if(right) __asm__ volatile("shrdw $0,%1,%0":"+m"(*p):"r"(src):"cc"); else __asm__ volatile("shldw $0,%1,%0":"+m"(*p):"r"(src):"cc"); _exit(0);}
    int status; if(waitpid(child,&status,0)!=child) exit(4);
    if(!WIFSIGNALED(status)||WTERMSIG(status)!=SIGSEGV) {fprintf(stderr,"count zero did not access memory\n");exit(5);} munmap(p,4096);
}
int main(void) {
    zero_access(0);zero_access(1);
    FILE *in=fopen("cases.tsv","r"),*out=fopen("oracle.tsv","w");if(!in||!out)return 6;
    unsigned right,d,s,count,seed; unsigned long rows=0;
    while(fscanf(in,"%u %u %u %u %u",&right,&d,&s,&count,&seed)==5) {
        uint16_t dst=(uint16_t)d,src=(uint16_t)s;unsigned long old,flags,initial;
        __asm__ volatile("pushfq;popq %0":"=r"(old)); initial=(old&~0x8d5UL)|(seed&0x8d5);
        if(right) __asm__ volatile("pushq %3;popfq;shrdw %%cl,%2,%0;pushfq;popq %1":"+r"(dst),"=&r"(flags):"r"(src),"r"(initial),"c"((uint8_t)count):"cc","memory");
        else __asm__ volatile("pushq %3;popfq;shldw %%cl,%2,%0;pushfq;popq %1":"+r"(dst),"=&r"(flags):"r"(src),"r"(initial),"c"((uint8_t)count):"cc","memory");
        fprintf(out,"%u %u\n",(unsigned)dst,(unsigned)(flags&0xffff));rows++;
    }
    fclose(in);fclose(out);printf("PASS host x86 SHLD/SHRD %lu defined cases; protected-memory count-zero faults for both instructions\n",rows);return 0;
}
"#;
