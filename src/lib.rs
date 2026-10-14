// SPDX-License-Identifier: GPL-3.0-or-later
//! Foundations for ahead-of-time translation, not a DOS/CPU interpreter.
#![forbid(unsafe_code)]

#[path = "devices/audio.rs"]
pub mod audio;
#[path = "host/audio_capture.rs"]
pub mod audio_capture;
#[path = "host/audio_output.rs"]
pub mod audio_output;
#[path = "devices/cdrom.rs"]
pub mod cdrom;
#[path = "devices/dos_files.rs"]
pub mod dos_files;
pub mod fpu;
#[cfg(feature = "analysis")]
#[path = "analysis/fpu_macro.rs"]
pub mod fpu_macro;
#[cfg(feature = "analysis")]
#[path = "analysis/lift.rs"]
pub mod lift;
#[cfg(feature = "analysis")]
#[path = "analysis/lift32.rs"]
pub mod lift32;
#[cfg(feature = "analysis")]
#[path = "analysis/lift_addr32.rs"]
pub mod lift_addr32;
#[cfg(feature = "analysis")]
#[path = "analysis/mutable_immediate.rs"]
pub mod mutable_immediate;
pub mod mz;
#[path = "host/native_startup.rs"]
pub mod native_startup;
#[path = "host/pipewire_output.rs"]
mod pipewire_output;
#[path = "devices/planar_video.rs"]
pub mod planar_video;
#[path = "host/player_config.rs"]
pub mod player_config;
pub mod runtime;
pub mod semantics;
#[path = "devices/sound_blaster.rs"]
pub mod sound_blaster;
#[path = "devices/text_video.rs"]
pub mod text_video;
#[path = "devices/video.rs"]
pub mod video;
#[cfg(feature = "window")]
#[path = "host/wayland.rs"]
pub mod wayland;
#[path = "devices/xms.rs"]
pub mod xms;

#[cfg(feature = "window")]
#[path = "host/gpu.rs"]
mod gpu;
#[cfg(feature = "profiling")]
#[path = "host/host_profile.rs"]
pub mod host_profile;
