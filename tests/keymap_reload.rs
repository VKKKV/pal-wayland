// SPDX-License-Identifier: GPL-3.0-or-later
//! Production template caller and actual generated authored-guest ELF. No GUI,
//! user saves, audio server or proprietary game resources are used.
#![cfg(all(feature = "analysis", feature = "window"))]
mod common;
use common::FixtureCargo;
use pal_wayland::lift::{Address, emit_rust, recover};
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

fn clean(command: &mut Command) -> &mut Command {
    for name in [
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "DISPLAY",
        "PIPEWIRE_RUNTIME_DIR",
        "XDG_RUNTIME_DIR",
    ] {
        command.env_remove(name);
    }
    command
}
#[test]
fn generated_template_applies_reload_and_retains_invalid_input_and_gain() {
    let root = common::unique_dir("pal-keymap-caller");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"),format!("[package]\nname='pal-keymap-caller-test'\nversion='0.0.0'\nedition='2024'\n[features]\nwindow=['pal-wayland/window']\nprofiling=['pal-wayland/profiling']\n[dependencies]\npal-wayland={{path={:?},default-features=false}}\n",env!("CARGO_MANIFEST_DIR"))).unwrap();
    // Authored DOS clock loop: exit normally after three observed second edges.
    // Real lifted instructions, no synthesized game state or timing adapter.
    let code = [
        0xb4, 0x2c, 0xcd, 0x21, 0x88, 0xf3, 0xbe, 3, 0, 0xb4, 0x2c, 0xcd, 0x21, 0x38, 0xde, 0x74,
        0xf8, 0x88, 0xf3, 0x4e, 0x75, 0xf3, 0xb8, 0, 0x4c, 0xcd, 0x21,
    ];
    let mut mz = vec![0; 544];
    for (i, w) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        mz[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    mz[32..32 + code.len()].copy_from_slice(&code);
    let m = pal_wayland::runtime::Machine::from_mz(&mz, 0x1010).unwrap();
    let graph = recover(m.memory.as_bytes(), &[Address { cs: 0x1010, ip: 0 }], 40).unwrap();
    // INT 21h exit has no statically known terminal successor; the bounded
    // graph may report its unreachable zero-filled continuation.
    assert!(graph.nodes.contains_key(&Address { cs: 0x1010, ip: 25 }));
    fs::write(root.join("guest.exe"), &mz).unwrap();
    fs::write(root.join("src/translated.rs"), emit_rust(&graph).unwrap()).unwrap();
    let mut template = include_str!("../templates/native_main.rs.in")
        .replace("@ENTRY_CS@", "0x1010")
        .replace("@ENTRY_IP@", "0");
    template.push_str(r#"
#[cfg(test)] mod caller_tests {
 use super::*;
 use pal_wayland::{wayland::{GameHost,KeyCode,keymap::KeyRouter},player_config::InitialConfig};
 use std::{fs,time::Duration};
 fn machine()->Machine{
  Machine::from_mz(&fs::read("guest.exe").unwrap(),0x1010).unwrap()
 }
 fn replace(p:&Path,text:&str){let stage=p.with_extension("stage");fs::write(&stage,text).unwrap();fs::rename(stage,p).unwrap();}
 #[test] fn production_windowhost_alias_reload_focus_gain_and_error_retention(){
  let dir=std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join(format!("pal-window-reload-{}",std::process::id()));fs::create_dir(&dir).unwrap();let p=dir.join("config.toml");
  replace(&p,"version=1\n[reload]\ninterval_ms=100\n");
  let mut initial=InitialConfig::load(Some(&p),true).unwrap();initial.cli_overrides.insert("--renderer".into(),"wgpu".into());
  let mut m=machine();m.sound_blaster=Some(pal_wayland::sound_blaster::SoundBlaster::default());m.retain_sfx_pcm=true;m.virtual_elapsed_ns=Some(0);
  let mut c=std::process::Command::new("python3");c.args(["-c","import sys;sys.stdin.buffer.read()"]);
  let mut o=pal_wayland::audio_output::AudioOutput::spawn_transport(c).unwrap();o.advance_machine(0,&mut m).unwrap();
  let mut h=WindowHost{m,limit:u64::MAX,virtual_step_ns:None,audio_config:None,realtime:Some(RealtimeAudio{output:Some(o),pending:false}),controls:ConfigRuntime::new(&initial),keys:KeyRouter::new(initial.document.resolved_keymap())};
  // The actual template must tolerate redraw before the first guest tick,
  // without changing memory, registers or instruction/time state.
  let before=(h.m.memory.clone(),format!("{:?}",h.m.regs),h.m.steps,h.m.virtual_elapsed_ns);
  assert_eq!(h.m.video_mode,3);assert!(h.m.presentation_indexed_frame().is_err());
  let startup=h.frame().unwrap();startup.validate().unwrap();assert!(startup.rgb_pixels().unwrap().iter().all(|&p|p==0));
  assert_eq!(h.m.memory,before.0);assert_eq!(format!("{:?}",h.m.regs),before.1);assert_eq!((h.m.steps,h.m.virtual_elapsed_ns),(before.2,before.3));
  h.m.video_mode=0x13;assert_eq!(h.frame().unwrap(),h.m.presentation_indexed_frame().unwrap());
  h.m.video_mode=0x12;assert!(h.frame().unwrap_err().contains("mode12"));h.m.video_mode=3;
  h.key(KeyCode::KeyW,true);h.key(KeyCode::ArrowUp,true);h.key(KeyCode::KeyW,true);assert!(h.m.key_down[0x48]);assert!(!h.m.key_down[0x11]);
  h.key(KeyCode::KeyW,false);assert!(h.m.key_down[0x48]);h.key(KeyCode::KeyW,true);
  replace(&p,"version=1\n[video]\nrenderer='softbuffer'\n[audio]\nvolume=50\nmuted=true\n[keymap]\nKeyW='KeyW'\n[reload]\ninterval_ms=100\n");
  std::thread::sleep(Duration::from_millis(110));assert!(h.tick().unwrap());
  assert!(!h.m.key_down[0x48]);assert_eq!(h.realtime.as_ref().unwrap().output.as_ref().unwrap().gain(),(50,true));
  h.key(KeyCode::KeyW,true);assert!(!h.m.key_down[0x11]);h.key(KeyCode::KeyW,false);h.key(KeyCode::KeyW,true);assert!(h.m.key_down[0x11]);h.key(KeyCode::ArrowUp,false);assert!(h.m.key_down[0x11]);
  replace(&p,"version=1\n[audio]\nvolume=101\n");std::thread::sleep(Duration::from_millis(110));h.tick().unwrap();
  assert!(h.m.key_down[0x11]);assert_eq!(h.realtime.as_ref().unwrap().output.as_ref().unwrap().gain(),(50,true));
  h.release_input();assert!(!h.m.key_down.iter().any(|b|*b));h.release_input();
  replace(&p,"version=1\n[audio]\nvolume=0\nmuted=false\n[keymap]\nKeyW='None'\n[reload]\ninterval_ms=100\n");std::thread::sleep(Duration::from_millis(110));h.tick().unwrap();h.key(KeyCode::KeyW,true);assert!(!h.m.key_down[0x11]);assert_eq!(h.realtime.as_ref().unwrap().output.as_ref().unwrap().gain(),(0,false));
  let candidate=pal_wayland::player_config::Config::parse(b"version=1\n[video]\nrenderer='softbuffer'\n[audio]\noutput='off'\n",&p).unwrap();assert_eq!(initial.restart_fields(&candidate),["audio.output"]);
  drop(h);fs::remove_dir_all(dir).unwrap();
 }
}
"#);
    fs::write(root.join("src/main.rs"), template).unwrap();
    let target = root.join("target");
    let output = clean(Command::new(env!("CARGO")).current_dir(&root).args([
        "test",
        "--offline",
        "--features",
        "window",
    ]))
    .fixture_output()
    .unwrap();
    assert!(
        output.status.success(),
        "fixture={} {output:?}",
        root.display()
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    let output = clean(Command::new(env!("CARGO")).current_dir(&root).args([
        "build",
        "--offline",
        "--features",
        "window",
    ]))
    .fixture_output()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = clean(
        Command::new(env!("CARGO"))
            .current_dir(&root)
            .args(["build", "--offline"]),
    )
    .fixture_output()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let elf = target.join("debug/pal-keymap-caller-test");
    let config = root.join("live.toml");
    fs::write(
        &config,
        "version=1\n[audio]\nvolume=100\n[reload]\ninterval_ms=100\n",
    )
    .unwrap();
    let stderr = fs::File::create(root.join("stderr")).unwrap();
    let stdout = fs::File::create(root.join("stdout")).unwrap();
    let mut child = clean(
        Command::new(&elf)
            .arg(root.join("guest.exe"))
            .args(["--max-steps", "1000000000", "--config"])
            .arg(&config),
    )
    .stderr(stderr)
    .stdout(stdout)
    .spawn()
    .unwrap();
    let replace = |text: &str| {
        let p = config.with_extension("stage");
        fs::write(&p, text).unwrap();
        fs::rename(p, &config).unwrap();
    };
    std::thread::sleep(Duration::from_millis(180));
    replace(
        "version=1\n[audio]\noutput='off'\nvolume=50\nmuted=true\n[keymap]\nKeyW='ArrowUp'\nKeyA='ArrowLeft'\nKeyS='ArrowDown'\nKeyD='ArrowRight'\n[reload]\ninterval_ms=100\n",
    );
    let wait_for = |text: &str| {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = fs::read_to_string(root.join("stderr")).unwrap();
            if s.contains(text) {
                break;
            }
            assert!(Instant::now() < deadline, "missing {text:?}: {s}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for("volume=50 muted=true");
    replace("version=1\n[audio]\nvolume=101\n");
    wait_for("status=rejected retained=last-valid");
    replace(
        "version=1\n[audio]\noutput='pipewire'\nvolume=0\nmuted=false\n[reload]\ninterval_ms=100\n",
    );
    wait_for("volume=0 muted=false");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("authored guest timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    let err = fs::read_to_string(root.join("stderr")).unwrap();
    assert!(err.contains("audio-output backend=off"));
    assert!(!err.contains("transport="));
    assert!(
        err.contains("volume=50 muted=true restart-required=none"),
        "{err}"
    );
    assert!(
        err.contains("volume=0 muted=false restart-required=audio.output"),
        "{err}"
    );
    assert_eq!(err.matches("status=rejected").count(), 1);
    assert!(
        fs::read_to_string(root.join("stdout"))
            .unwrap()
            .contains("stop_reason: DOS-exit")
    );
    // Controls do not read the nonexistent original EXE or initialize devices.
    let controls = clean(
        Command::new(&elf)
            .args(["--controls", "--config"])
            .arg(&config),
    )
    .output()
    .unwrap();
    assert!(controls.status.success(), "{controls:?}");
    assert!(
        String::from_utf8_lossy(&controls.stdout)
            .contains("KeyW -> ArrowUp (48, 00) [source=default]")
    );
    fs::remove_dir_all(root).unwrap();
}
