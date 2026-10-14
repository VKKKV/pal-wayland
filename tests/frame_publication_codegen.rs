// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(feature = "analysis")]
mod common;
use common::FixtureCargo;
use pal_wayland::lift::{Address, emit_rust, recover};
use std::{fs, path::PathBuf, process::Command};
#[test]
fn palette_publication_requires_a_closed_cx_cycle() {
    // Authored 32-bit-address LOOP and open/non-LOOP tails must never acquire
    // a CX-based owner, even though they still contain OUT DX,AL.
    for code in [
        &[0xac, 0xee, 0x67, 0xe2, 0xfb, 0x90][..],
        &[0xac, 0xee, 0x90][..],
    ] {
        let mut memory = vec![0; 0x100000];
        memory[0x10100..0x10100 + code.len()].copy_from_slice(code);
        let graph = recover(&memory, &[Address { cs: 0x1010, ip: 0 }], 6).unwrap();
        let emitted = emit_rust(&graph).unwrap();
        assert!(!emitted.contains("palette_stream_out"));
        assert!(!emitted.contains("finish_palette_stream"));
    }
}
#[test]
fn generated_rep_palette_and_partial_animation_publish_complete_snapshots() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = common::unique_dir("pal-frame-codegen");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("Cargo.toml"),format!("[package]\nname='pal-frame-codegen'\nversion='0.0.0'\nedition='2024'\n[workspace]\n[dependencies]\npal-wayland={{path={root:?},default-features=false}}\n")).unwrap();
    // Authored full fill, row fill, palette loop and INT boundary; no game bytes.
    let code = [
        0xf3, 0xaa, 0x90, 0xac, 0xee, 0xe2, 0xfc, 0xcd, 0x2f, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
        0x90, 0xf3, 0x66, 0xa5,
    ];
    let mut memory = vec![0; 0x100000];
    memory[0x10100..0x10100 + code.len()].copy_from_slice(&code);
    let g = recover(
        &memory,
        &[
            Address { cs: 0x1010, ip: 0 },
            Address { cs: 0x1010, ip: 3 },
            Address { cs: 0x1010, ip: 16 },
        ],
        30,
    )
    .unwrap();
    let emitted = emit_rust(&g).unwrap();
    assert!(emitted.contains("m.begin_vram_rep(1)?"));
    assert!(emitted.contains("m.palette_stream_out(5)?;"));
    assert!(emitted.contains("m.finish_palette_stream(4112, 4, 5, 3);"));
    fs::write(dir.join("src/translated.rs"), emitted).unwrap();
    fs::write(dir.join("src/main.rs"), HARNESS).unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["run", "--offline", "--quiet"])
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(&dir)
        .fixture_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{output:?}; fixture={}",
        dir.display()
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("complete-frame PASS"));
    fs::remove_dir_all(dir).unwrap();
}
const HARNESS: &str = r#"
use pal_wayland::{runtime::Machine,semantics::{Flags,SegmentedAddress as A},video::{IndexedFrame,FRAME_BYTES}};
mod translated;
fn sample(index:u8)->IndexedFrame {IndexedFrame {indices:vec![index;FRAME_BYTES],palette:[[index,index,index,255];256],pixel_mask:0xff}}
fn snapshot(m:&Machine)->IndexedFrame {
 let before=(m.memory.clone(),format!("{:?}",m.regs),format!("{:?}",m.dac),m.steps,m.virtual_elapsed_ns);
 let f=m.presentation_indexed_frame().unwrap();f.validate().unwrap();
 assert_eq!(f.rgb_pixels().unwrap(),m.presentation_frame().unwrap());
 assert_eq!(m.memory,before.0);assert_eq!(format!("{:?}",m.regs),before.1);assert_eq!(format!("{:?}",m.dac),before.2);
 assert_eq!((m.steps,m.virtual_elapsed_ns),(before.3,before.4));f
}
fn machine()->Machine {
 let mut b=vec![0;96];for (i,w) in [0x5a4d_u16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate(){b[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
 let mut m=Machine::from_mz(&b,0x1010).unwrap();m.virtual_elapsed_ns=Some(0);m.video_mode=0x13;
 m.write_bytes(A::new(0x1010,0),&[0xf3,0xaa,0x90,0xac,0xee,0xe2,0xfc,0xcd,0x2f]).unwrap();
 m.dac.write_port(0x3c8,1).unwrap();for v in [63,0,0]{m.dac.write_port(0x3c9,v).unwrap();}m
}
fn palette_machine()->Machine {
 let mut m=machine();m.regs.ip=3;m.regs.ds=0x5000;m.regs.si=0;m.regs.dx=0x3c9;m.regs.cx=768;
 m.write_bytes(A::new(0x5000,0),&vec![63;768]).unwrap();m.port_out(0x3c8,0,8).unwrap();
 for v in [0,0,0,0,0,0]{m.port_out(0x3c9,v,8).unwrap();}m.port_out(0x3c8,0,8).unwrap();
 m.write8(A::new(0xa000,1),1).unwrap();m
}
fn main(){
 // Ordinary redraws never seed the next transaction: begin supplies a fresh
 // before frame, even after repeated draws and a failed current-frame callback.
 let mut publication=pal_wayland::video::FramePublication::default();
 for index in [1,2,3] {assert_eq!(publication.indexed_frame(||Ok(sample(index))).unwrap(),sample(index));}
 assert!(publication.indexed_frame(||Err("authored failure".into())).is_err());
 publication.begin((0x1010,0),sample(4));
 assert_eq!(publication.indexed_frame(||panic!("active must not decode partial VRAM")).unwrap(),sample(4));
 publication.begin((0x1010,1),sample(5));publication.finish((0x1010,1));
 assert_eq!(publication.indexed_frame(||panic!("foreign owner must not publish")).unwrap(),sample(4));
 let mut detached=publication.indexed_frame(||unreachable!()).unwrap();detached.indices[0]=99;detached.palette[0]=[99,99,99,255];detached.pixel_mask=0;
 assert_eq!(publication.indexed_frame(||unreachable!()).unwrap(),sample(4));
 publication.finish((0x1010,0));
 assert_eq!(publication.indexed_frame(||Ok(sample(8))).unwrap(),sample(8));
 let mut m=machine();m.regs.es=0xa000;m.regs.di=0;m.regs.cx=64000;m.regs.ax=1;
 assert!(!translated::playback_boundary(&m));
 translated::step(&mut m).unwrap();assert_eq!(m.steps,64);assert_eq!(m.regs.cx,63936);assert_eq!(m.regs.ip,0);
 assert_eq!(m.frame().unwrap().2[0],0xff0000);assert!(m.presentation_frame().unwrap().iter().all(|&p|p==0));
 // REP guards are checked again on every continuation.
 m.write8(A::new(0x1010,0),0x90).unwrap();let before=m.steps;assert!(translated::step(&mut m).is_err());assert_eq!(m.steps,before);
 m.write8(A::new(0x1010,0),0xf3).unwrap();
 while m.regs.ip==0 {translated::step(&mut m).unwrap();}
 assert_eq!(m.steps,64000);assert_eq!(m.regs.cx,0);assert!(m.presentation_frame().unwrap().iter().all(|&p|p==0xff0000));
 assert!(snapshot(&m).indices.iter().all(|&index|index==1));
 // Palette and mask writes during a full REP belong to the next publication,
 // not the frozen before snapshot. Indices must never be CPU-premasked.
 let mut frozen=machine();frozen.regs.es=0xa000;frozen.regs.di=0;frozen.regs.cx=64000;frozen.regs.ax=1;
 let old=snapshot(&frozen);translated::step(&mut frozen).unwrap();
 frozen.port_out(0x3c8,0,8).unwrap();for v in [0,0,63,0,63,0]{frozen.port_out(0x3c9,v,8).unwrap();}
 frozen.port_out(0x3c6,0,8).unwrap();
 assert_eq!(snapshot(&frozen),old);assert!(frozen.frame().unwrap().2.iter().all(|&p|p==0x0000ff));
 let count=frozen.regs.cx;frozen.regs.cx=0;frozen.finish_vram_rep(0x1010,1).unwrap();frozen.regs.cx=count;
 assert_eq!(snapshot(&frozen),old);
 while frozen.regs.ip==0 {translated::step(&mut frozen).unwrap();}
 let committed=snapshot(&frozen);assert!(committed.indices.iter().all(|&index|index==1));
 assert_eq!(committed.pixel_mask,0);assert_eq!(committed.palette[0],[0,0,255,255]);assert_eq!(committed.palette[1],[0,255,0,255]);
 assert!(committed.rgb_pixels().unwrap().iter().all(|&p|p==0x0000ff));
 frozen.port_out(0x3c6,0xff,8).unwrap();assert!(snapshot(&frozen).rgb_pixels().unwrap().iter().all(|&p|p==0x00ff00));
 for mode in [3,0x12] {frozen.video_mode=mode;assert!(frozen.presentation_indexed_frame().is_err());assert!(frozen.presentation_frame().is_err());}
 // A new partial row REP publishes immediately, without waiting for 200 rows.
 m.regs.ip=0;m.regs.di=0;m.regs.cx=320;m.regs.ax=0;translated::step(&mut m).unwrap();
 let f=m.presentation_frame().unwrap();assert_eq!(f[0],0);assert_eq!(f[64],0xff0000);
 while m.regs.ip==0 {translated::step(&mut m).unwrap();}
 // Unknown scalar VRAM writes remain visible.
 m.write8(A::new(0xa000,60000),0).unwrap();assert_eq!(m.presentation_frame().unwrap()[60000],0);
 // Full palette pass is a closed statically recovered cycle, not a vsync guess.
 m.regs.ip=3;m.regs.ds=0x5000;m.regs.si=0;m.regs.dx=0x3c9;m.regs.cx=768;
 m.write_bytes(A::new(0x5000,0),&vec![63;768]).unwrap();m.port_out(0x3c8,0,8).unwrap();
 for _ in 0..9 {translated::step(&mut m).unwrap();}
 assert_eq!(m.dac.rgb(0),[255,255,255]);assert_eq!(m.presentation_frame().unwrap()[0],0);
 assert_eq!(snapshot(&m).palette[0],[0,0,0,255]);
 while m.regs.ip!=7 {translated::step(&mut m).unwrap();}
 assert_eq!(m.regs.cx,0);assert!(translated::playback_boundary(&m));assert!(m.presentation_frame().unwrap().iter().all(|&p|p==0xffffff));
 assert_eq!(snapshot(&m).palette,[[255;4];256]);
 // Standalone triplets retain local palette animation and raw component state.
 m.port_out(0x3c8,0,8).unwrap();m.port_out(0x3c9,0,8).unwrap();assert_eq!(m.dac.rgb(0),[0,255,255]);assert_eq!(m.presentation_frame().unwrap()[0],0xffffff);
 assert_eq!(snapshot(&m).palette[0],[255;4]);
 m.port_out(0x3c9,0,8).unwrap();m.port_out(0x3c9,0,8).unwrap();assert_eq!(m.presentation_frame().unwrap()[0],0);
 assert_eq!(snapshot(&m).palette[0],[0,0,0,255]);
 // The same generated cycle with only three components is ordinary RGB
 // animation; raw changes are visible diagnostically before host publication.
 let mut short=palette_machine();short.regs.cx=3;
 for _ in 0..2 {translated::step(&mut short).unwrap();}
 assert_eq!(short.dac.rgb(0),[255,0,0]);assert_eq!(short.presentation_frame().unwrap()[0],0);
 while short.regs.ip!=7 {translated::step(&mut short).unwrap();}
 assert_eq!(short.regs.cx,0);assert_eq!(short.presentation_frame().unwrap()[0],0xffffff);
 // Natural early exit: IRQ changes CX after OUT, before its paired LOOP.
 let mut early=palette_machine();
 for _ in 0..8 {translated::step(&mut early).unwrap();}
 assert_eq!(early.regs.ip,5);assert_eq!(early.regs.cx,766);
 assert_eq!(early.dac.rgb(0),[255,255,255]);assert_eq!(early.presentation_frame().unwrap()[0],0);
 early.enable_irqs().unwrap();early.virtual_elapsed_ns=Some(60_000_000);
 translated::step(&mut early).unwrap();assert_eq!(early.regs.cs,0xf000);
 early.regs.cx=1;translated::step(&mut early).unwrap();assert_eq!(early.regs.ip,5);
 assert_eq!(early.presentation_frame().unwrap()[0],0);
 translated::step(&mut early).unwrap();assert_eq!(early.regs.ip,7);assert_eq!(early.regs.cx,0);
 assert_eq!(early.presentation_frame().unwrap()[0],0xffffff);
 // No new 3c8 is required for subsequent ordinary completed triplets.
 for v in [0,63,0]{early.port_out(0x3c9,v,8).unwrap();}
 assert_eq!(early.dac.rgb(1),[0,255,0]);assert_eq!(early.presentation_frame().unwrap()[1],0x00ff00);
 // Counter mismatch after LOOP but before the next OUT releases, not faults.
 let mut changed=palette_machine();for _ in 0..9 {translated::step(&mut changed).unwrap();}
 assert_eq!(changed.regs.ip,3);changed.regs.cx=1;
 translated::step(&mut changed).unwrap();translated::step(&mut changed).unwrap();
 assert_eq!(changed.presentation_frame().unwrap()[0],0xffffff);
 translated::step(&mut changed).unwrap();assert_eq!(changed.regs.ip,7);
 assert_eq!(changed.dac.rgb(1),[255,0,0]);assert_eq!(changed.presentation_frame().unwrap()[1],0);
 for v in [63,63]{changed.port_out(0x3c9,v,8).unwrap();}
 assert_eq!(changed.presentation_frame().unwrap()[1],0xffffff);
 // LOOP's own continuation check also releases a changed nonzero CX.
 let mut changed_loop=palette_machine();for _ in 0..8 {translated::step(&mut changed_loop).unwrap();}
 changed_loop.regs.cx=12;translated::step(&mut changed_loop).unwrap();
 assert_eq!(changed_loop.regs.ip,3);assert_eq!(changed_loop.regs.cx,11);
 assert_eq!(changed_loop.presentation_frame().unwrap()[0],0xffffff);
 // An unrelated IRQ with no DAC interference must leave the scope suspended.
 let mut paused=palette_machine();for _ in 0..8 {translated::step(&mut paused).unwrap();}
 paused.enable_irqs().unwrap();paused.virtual_elapsed_ns=Some(60_000_000);
 translated::step(&mut paused).unwrap();assert_eq!(paused.regs.cs,0xf000);
 assert_eq!(paused.presentation_frame().unwrap()[0],0);
 translated::step(&mut paused).unwrap();assert_eq!(paused.regs.ip,5);
 assert_eq!(paused.presentation_frame().unwrap()[0],0);
 while paused.regs.ip!=7 {translated::step(&mut paused).unwrap();}
 assert_eq!(paused.regs.cx,0);assert_eq!(paused.dac.read_port(0x3c8).unwrap(),0);
 assert!(paused.presentation_frame().unwrap().iter().all(|&p|p==0xffffff));
 // IRQ DAC data interference releases ownership before the foreign write,
 // even without resetting 3c8; returning to the loop cannot freeze publication.
 let mut foreign=palette_machine();for _ in 0..8 {translated::step(&mut foreign).unwrap();}
 foreign.enable_irqs().unwrap();foreign.virtual_elapsed_ns=Some(60_000_000);
 translated::step(&mut foreign).unwrap();assert_eq!(foreign.regs.cs,0xf000);
 for v in [0,63,0]{foreign.port_out(0x3c9,v,8).unwrap();}
 assert_eq!(foreign.presentation_frame().unwrap()[0],0xffffff);
 assert_eq!(foreign.presentation_frame().unwrap()[1],0x00ff00);
 translated::step(&mut foreign).unwrap();
 while foreign.regs.ip!=7 {translated::step(&mut foreign).unwrap();}
 assert_eq!(foreign.regs.cx,0);assert_eq!(foreign.dac.read_port(0x3c8).unwrap(),1);
 assert_eq!(foreign.presentation_frame().unwrap()[1],0x00ff00);
 assert!(foreign.presentation_frame().unwrap().iter().enumerate().all(|(i,&p)|p==if i==1 {0x00ff00} else {0xffffff}));
 // Guards run before owned OUT and LOOP publication side effects.
 for ip in [4,5] {
  let mut guard=palette_machine();for _ in 0..9 {translated::step(&mut guard).unwrap();}
  while guard.regs.ip!=ip {translated::step(&mut guard).unwrap();}
  let before=(guard.regs.clone(),guard.steps,guard.dac.rgb(0),guard.presentation_frame().unwrap());
  guard.write8(A::new(0x1010,ip),0x90).unwrap();assert!(translated::step(&mut guard).is_err());
  assert_eq!(guard.steps,before.1);assert_eq!(guard.regs.cx,before.0.cx);assert_eq!(guard.regs.ip,before.0.ip);
  assert_eq!(guard.dac.rgb(0),before.2);assert_eq!(guard.presentation_frame().unwrap(),before.3);
 }
 // A dword copy also remains atomic across 64-element resumptions and DF=1.
 m.write_bytes(A::new(0x1010,16),&[0xf3,0x66,0xa5]).unwrap();
 m.write_bytes(A::new(0x5000,0),&vec![1;64000]).unwrap();
 m.regs.cs=0x1010;m.regs.ip=16;m.regs.ds=0x5000;m.regs.es=0xa000;
 m.regs.si=63996;m.regs.di=63996;m.regs.cx=16000;m.regs.flags.set(Flags::DF,true);
 let old=snapshot(&m);let steps=m.steps;
 translated::step(&mut m).unwrap();assert_eq!(m.steps-steps,64);assert_eq!(m.regs.ip,16);
 assert_eq!(snapshot(&m),old);
 // An IRQ can defer the REP continuation without ending its publication scope.
 m.enable_irqs().unwrap();m.virtual_elapsed_ns=Some(60_000_000);
 translated::step(&mut m).unwrap();assert_eq!(m.regs.cs,0xf000);assert_eq!(snapshot(&m),old);
 m.port_out(0x3c8,0,8).unwrap();for v in [0,0,63,0,63,0]{m.port_out(0x3c9,v,8).unwrap();}m.port_out(0x3c6,0,8).unwrap();
 assert_eq!(snapshot(&m),old);
 translated::step(&mut m).unwrap();assert_eq!(m.regs.ip,16);
 while m.regs.ip==16 {translated::step(&mut m).unwrap();}
 assert_eq!(m.regs.cx,0);assert_eq!(m.regs.di,65532);
 let final_frame=snapshot(&m);assert!(final_frame.indices.iter().all(|&index|index==1));assert_eq!(final_frame.pixel_mask,0);
 assert_eq!(final_frame.palette[0],[0,0,255,255]);assert_eq!(final_frame.palette[1],[0,255,0,255]);
 assert!(final_frame.rgb_pixels().unwrap().iter().all(|&p|p==0x0000ff));
 // Zero-count REP completes without touching VRAM or opening a transaction.
 let before=m.memory.clone();m.regs.ip=16;translated::step(&mut m).unwrap();assert_eq!(m.memory,before);
 println!("complete-frame PASS");
}
"#;
