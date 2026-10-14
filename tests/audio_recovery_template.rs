// SPDX-License-Identifier: GPL-3.0-or-later
//! Compile the production caller template with a tiny authored step adapter.
//! Test binary only; never open a window/device or execute the original game.
#![cfg(feature = "analysis")]
mod common;
use common::FixtureCargo;
use std::{fs, process::Command};
#[test]
fn production_window_caller_runs_no_guest_steps_while_audio_is_pending() {
    let dir = common::unique_dir("pal-audio-caller");
    fs::create_dir_all(dir.join("src")).unwrap();
    let manifest = format!(
        "[package]\nname='pal-audio-caller-test'\nversion='0.0.0'\nedition='2024'\n[features]\nwindow=['pal-wayland/window']\n[dependencies]\npal-wayland={{path={:?},default-features=false}}\n",
        env!("CARGO_MANIFEST_DIR")
    );
    fs::write(dir.join("Cargo.toml"), manifest).unwrap();
    fs::write(dir.join("src/translated.rs"), "pub fn playback_boundary(_: &pal_wayland::runtime::Machine)->bool{false} pub fn step(m:&mut pal_wayland::runtime::Machine)->Result<(),pal_wayland::runtime::Fault>{m.steps+=1;m.exit_code=Some(0);Ok(())}\n").unwrap();
    let mut template = include_str!("../templates/native_main.rs.in")
        .replace("@ENTRY_CS@", "0x1010")
        .replace("@ENTRY_IP@", "0");
    template.push_str(r#"
#[cfg(test)]
mod caller_tests {
    use super::*;
    use pal_wayland::wayland::GameHost;
    use pal_wayland::audio_output::{AudioOutput,RECOVERY_RETRY_INTERVAL};
    fn host()->WindowHost {
        let mut b=vec![0;544];
        for (i,w) in [0x5a4d_u16,32,2,0,2,0x20,0xffff,0,0x400,0,0,0,28,0].into_iter().enumerate(){b[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
        let mut m=Machine::from_mz(&b,0x1010).unwrap();
        m.virtual_elapsed_ns=Some(0);m.retain_sfx_pcm=true;
        m.sound_blaster=Some(pal_wayland::sound_blaster::SoundBlaster::default());
        let mut c=std::process::Command::new("python3");c.args(["-c","import sys;sys.stdin.buffer.read()"]);
        let mut o=AudioOutput::spawn_transport(c).unwrap();o.advance_machine(0,&mut m).unwrap();
        WindowHost{m,limit:10,virtual_step_ns:None,audio_config:None,realtime:Some(RealtimeAudio{output:Some(o),pending:false}),controls:ConfigRuntime::new(&pal_wayland::player_config::InitialConfig::load(None,false).unwrap()),keys:pal_wayland::wayland::keymap::KeyRouter::default()}
    }
    #[test]
    fn pending_audio_suspends_translated_steps_and_requests_paced_retry() {
        let mut h=host();h.m.virtual_elapsed_ns=Some(100_000_000);
        // Independent production IRQ polling has already rendered hardware.
        assert!(!h.m.poll_timer_irq().unwrap());
        let before=h.m.regs;
        assert!(h.tick().unwrap());
        assert_eq!(h.m.steps,0);assert_eq!(h.m.regs,before);assert!(h.m.exit_code.is_none());
        assert_eq!(h.retry_after(),Some(RECOVERY_RETRY_INTERVAL));
        assert_eq!(h.realtime.as_ref().unwrap().output.as_ref().unwrap().progress().submitted_frames,2205);
        // Recovery completes before the tiny authored translated adapter can run.
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        loop {std::thread::sleep(RECOVERY_RETRY_INTERVAL);if !h.tick().unwrap(){break;}assert!(std::time::Instant::now()<deadline);}
        assert_eq!(h.m.steps,1);assert_eq!(h.retry_after(),None);
        let o=h.realtime.take().unwrap().output.take().unwrap();
        let p=o.finish(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(p.submitted_frames,4410);assert_eq!(p.pipe_written_frames,4410);
    }
}
"#);
    fs::write(dir.join("src/main.rs"), template).unwrap();
    let output = Command::new(env!("CARGO"))
        .env_remove("CARGO_TARGET_DIR")
        .args(["test", "--offline", "--features", "window"])
        .current_dir(&dir)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DISPLAY")
        .env_remove("PIPEWIRE_RUNTIME_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .fixture_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{output:?}; fixture={}",
        dir.display()
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    fs::remove_dir_all(dir).unwrap();
}
