// SPDX-License-Identifier: GPL-3.0-or-later
//! Optional native Wayland presentation/input; no CPU or DOS emulation lives here.
//!
//! Include this module only under the `window` feature. Pin winit =0.30.12
//! (`default-features = false`, `wayland`, `wayland-dlopen`, `rwh_06`) and
//! softbuffer =0.4.8 (`default-features = false`, `wayland`, `wayland-dlopen`).
//! wgpu =30.0.1 enables only std/parking_lot/vulkan/wgsl; pollster =0.4.0.
//! Window backends may not acquire X11/KMS features through feature unification.
//! Rendering is independent of guest execution: a redraw never advances the core.
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::platform::wayland::{
    EventLoopBuilderExtWayland, EventLoopExtWayland, WindowAttributesExtWayland,
};
use winit::window::{Window, WindowAttributes, WindowId};

pub use winit::keyboard::KeyCode;
#[path = "keymap.rs"]
pub mod keymap;

use crate::video::{HEIGHT, IndexedFrame, WIDTH, scale_frame};

/// Adapter implemented by the statically translated native core, not an emulator.
pub trait GameHost {
    /// Execute one bounded slice. `true` continues; `false` exits successfully.
    ///
    /// The adapter owns the instruction budget and guest timing. Returning from
    /// this method promptly is necessary for responsive input and close events.
    fn tick(&mut self) -> Result<bool, String>;

    /// Pending host audio debt: retry under event-loop pacing, without guest
    /// execution or scheduling an expensive redraw between producer slices.
    /// None retains the ordinary presentation cadence.
    fn retry_after(&self) -> Option<Duration> {
        None
    }

    /// Waiting-for-key hosts may lower execution wakeups without changing FPS.
    /// Input and resize events still wake the event loop immediately.
    fn idle_wait(&self) -> Option<Duration> {
        None
    }

    /// Read a complete mode-13h indexed frame, committed RGBA palette and mask.
    /// This must not advance or otherwise mutate the guest machine.
    fn frame(&self) -> Result<IndexedFrame, String>;

    /// Physical key transitions, including arrows, Enter and Escape unchanged.
    /// Host repeat events are suppressed; the adapter owns guest key repeat.
    /// Focus loss, suspension and exit release all keys still held. Escape is
    /// guest input, not a hard-coded window-close shortcut.
    fn key(&mut self, key: KeyCode, pressed: bool);

    /// Release logical input even when held physical keys were suppressed by
    /// reload. Called on focus loss, suspension, exit and event-loop failure.
    fn release_input(&mut self) {}
}

/// Host scheduling only; this does not claim to reproduce a DOS timer frequency.
pub const TICK_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// Absolute presentation cadence; arithmetic skip, never a catch-up loop.
fn next_tick_after(previous: Instant, finished: Instant) -> Instant {
    let periods =
        finished.saturating_duration_since(previous).as_nanos() / TICK_INTERVAL.as_nanos() + 1;
    previous + TICK_INTERVAL * u32::try_from(periods).unwrap_or(u32::MAX)
}

fn next_host_tick(_previous: Instant, finished: Instant, retry: Option<Duration>) -> Instant {
    finished
        + retry
            .unwrap_or(Duration::from_millis(1))
            .max(Duration::from_millis(1))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RendererKind {
    #[default]
    Wgpu,
    Softbuffer,
}
impl std::str::FromStr for RendererKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "wgpu" => Ok(Self::Wgpu),
            "softbuffer" => Ok(Self::Softbuffer),
            _ => Err("renderer must be wgpu or softbuffer".into()),
        }
    }
}

/// Run until close, `tick() == Ok(false)`, or an error. There is no X11 fallback.
///
/// This is a blocking, single-use winit event loop. Prefer the process main thread;
/// this Linux-only backend also permits a dedicated event-loop owner thread.
/// Merely compiling this module or running its tests never opens a desktop window.
/// Call this function only after an explicit request for windowed execution.
pub fn run(host: impl GameHost + 'static) -> Result<(), String> {
    run_with_renderer(host, RendererKind::Wgpu)
}

pub fn run_with_renderer(
    host: impl GameHost + 'static,
    renderer: RendererKind,
) -> Result<(), String> {
    // Catch initialization panics (for example a missing dynamically loaded
    // system library), not host panics. Do not hide them behind a fallback.
    let event_loop = catch_unwind(AssertUnwindSafe(|| {
        EventLoop::builder()
            .with_wayland()
            .with_any_thread(true)
            .build()
    }))
    .map_err(|payload| {
        let detail = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("unknown initialization panic");
        format!("Wayland initialization failed (no X11 fallback): {detail}")
    })?
    .map_err(|error| format!("Wayland initialization failed (no X11 fallback): {error}"))?;
    if !event_loop.is_wayland() {
        return Err("refusing non-Wayland event loop; no X11 fallback".into());
    }
    let mut app = App::new(host, renderer);
    let result = event_loop.run_app(&mut app);
    // Also release input if winit returns an error without an exiting callback.
    app.keys.release_all(&mut app.host);
    match app.failure {
        Some(error) => Err(error),
        None => result.map_err(|error| format!("Wayland event loop failed: {error}")),
    }
}

#[derive(Default)]
struct HeldKeys(BTreeSet<KeyCode>);

impl HeldKeys {
    fn transition(&mut self, host: &mut impl GameHost, key: KeyCode, pressed: bool) {
        let changed = if pressed {
            self.0.insert(key)
        } else {
            self.0.remove(&key)
        };
        if changed {
            host.key(key, pressed);
        }
    }

    fn release_all(&mut self, host: &mut impl GameHost) {
        for key in std::mem::take(&mut self.0) {
            host.key(key, false);
        }
        host.release_input();
    }
}

enum Backend {
    Wgpu(Box<crate::gpu::GpuDisplay>),
    Softbuffer {
        surface: Surface<Arc<Window>, Arc<Window>>,
        _context: Context<Arc<Window>>,
    },
}
struct DisplayState {
    backend: Backend,
    window: Arc<Window>,
    #[cfg(feature = "profiling")]
    presentation_clock: crate::host_profile::PresentationClock,
}

const APP_ID: &str = "pal-wayland";
fn window_attributes() -> WindowAttributes {
    Window::default_attributes()
        .with_name(APP_ID, APP_ID)
        .with_title("pal-wayland — PAL DOS AOT")
        .with_inner_size(PhysicalSize::new((WIDTH * 3) as u32, (HEIGHT * 3) as u32))
        .with_min_inner_size(PhysicalSize::new(WIDTH as u32, HEIGHT as u32))
}

impl DisplayState {
    fn new(event_loop: &ActiveEventLoop, renderer: RendererKind) -> Result<Self, String> {
        let attributes = window_attributes();
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("Wayland window creation failed: {error}"))?,
        );
        let backend = match renderer {
            RendererKind::Wgpu => {
                Backend::Wgpu(Box::new(crate::gpu::GpuDisplay::new(window.clone())?))
            }
            RendererKind::Softbuffer => {
                let context = Context::new(window.clone())
                    .map_err(|e| format!("Wayland softbuffer context failed: {e}"))?;
                let surface = Surface::new(&context, window.clone())
                    .map_err(|e| format!("Wayland softbuffer surface failed: {e}"))?;
                eprintln!("renderer=softbuffer");
                Backend::Softbuffer {
                    surface,
                    _context: context,
                }
            }
        };
        Ok(Self {
            backend,
            window,
            #[cfg(feature = "profiling")]
            presentation_clock: Default::default(),
        })
    }

    fn draw(&mut self, host: &impl GameHost) -> Result<(), String> {
        let size = self.window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            // A minimized/zero-sized surface cannot receive a buffer.
            return Ok(());
        };
        let source = {
            #[cfg(feature = "profiling")]
            let _decode_span = crate::host_profile::Span::new("frame_decode");
            host.frame()?
        };
        let Backend::Softbuffer { surface, .. } = &mut self.backend else {
            if let Backend::Wgpu(gpu) = &mut self.backend {
                return gpu.draw(&source);
            }
            unreachable!();
        };
        // Validate the real frame and bound destination allocation before asking
        // softbuffer to resize. Integer nearest-neighbor, centered black borders;
        // the shared video helper handles forced sub-native compositor sizes.
        let pixels = {
            #[cfg(feature = "profiling")]
            let _scale_span = crate::host_profile::Span::new("softbuffer_cpu_scale");
            let rgb = source.rgb_pixels()?;
            scale_frame(&rgb, size.width as usize, size.height as usize)?
        };
        surface
            .resize(width, height)
            .map_err(|error| format!("Wayland surface resize failed: {error}"))?;
        let mut buffer = {
            #[cfg(feature = "profiling")]
            let _acquire_span = crate::host_profile::Span::new("swapchain_acquire");
            surface
                .buffer_mut()
                .map_err(|error| format!("Wayland buffer acquisition failed: {error}"))?
        };
        if buffer.len() != pixels.len() {
            return Err("Wayland buffer size does not match the framebuffer".into());
        }
        // Fully repaint: never assume that a recycled buffer retains its pixels.
        {
            #[cfg(feature = "profiling")]
            let _copy_span = crate::host_profile::Span::new("softbuffer_cpu_copy");
            buffer.copy_from_slice(&pixels);
        }
        self.window.pre_present_notify();
        {
            #[cfg(feature = "profiling")]
            let _present_span = crate::host_profile::Span::new("present_cpu");
            buffer
                .present()
                .map_err(|error| format!("Wayland presentation failed: {error}"))?;
        }
        #[cfg(feature = "profiling")]
        self.presentation_clock.observe(true);
        Ok(())
    }
}

struct App<H> {
    host: H,
    display: Option<DisplayState>,
    keys: HeldKeys,
    next_tick: Instant,
    next_frame: Instant,
    renderer: RendererKind,
    failure: Option<String>,
}

impl<H: GameHost> App<H> {
    fn new(host: H, renderer: RendererKind) -> Self {
        Self {
            host,
            display: None,
            keys: HeldKeys::default(),
            next_tick: Instant::now(),
            next_frame: Instant::now(),
            renderer,
            failure: None,
        }
    }

    fn stop(&mut self, event_loop: &ActiveEventLoop, failure: Option<String>) {
        if self.failure.is_none() {
            self.failure = failure;
        }
        self.keys.release_all(&mut self.host);
        event_loop.exit();
    }
}

impl<H: GameHost> ApplicationHandler for App<H> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.display.is_some() || event_loop.exiting() {
            return;
        }
        match DisplayState::new(event_loop, self.renderer) {
            Ok(display) => {
                display.window.request_redraw();
                self.display = Some(display);
                self.next_tick = Instant::now();
                self.next_frame = self.next_tick;
            }
            Err(error) => self.stop(event_loop, Some(error)),
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.keys.release_all(&mut self.host);
        self.display = None;
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if event_loop.exiting() || self.display.as_ref().map(|d| d.window.id()) != Some(window_id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => self.stop(event_loop, None),
            WindowEvent::Focused(false) => self.keys.release_all(&mut self.host),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    self.keys
                        .transition(&mut self.host, key, event.state == ElementState::Pressed);
                    self.next_tick = Instant::now();
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(display) = &self.display {
                    display.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                #[cfg(feature = "profiling")]
                let _span = crate::host_profile::Span::new("frame");
                if let Some(display) = &mut self.display
                    && let Err(error) = display.draw(&self.host)
                {
                    self.stop(event_loop, Some(error));
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() || self.display.is_none() {
            return;
        }
        if Instant::now() >= self.next_tick {
            #[cfg(feature = "profiling")]
            let _span = crate::host_profile::Span::new("tick");
            match self.host.tick() {
                Ok(true) => {}
                Ok(false) => {
                    self.stop(event_loop, None);
                    return;
                }
                Err(error) => {
                    self.stop(event_loop, Some(error));
                    return;
                }
            }
            // Never run an unbounded catch-up loop after a slow slice. Redraws
            // (including expose/resize) cannot execute extra guest instructions.
            // A slow slice misses its deadline without an unbounded catch-up
            // loop; normal slices do not add an extra full period after work.
            let finished = Instant::now();
            let retry = self.host.retry_after().or_else(|| self.host.idle_wait());
            self.next_tick = next_host_tick(self.next_tick, finished, retry);
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = next_tick_after(self.next_frame, now);
            if self.host.retry_after().is_none()
                && let Some(display) = &self.display
            {
                display.window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_tick.min(self.next_frame)));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.keys.release_all(&mut self.host);
        self.display = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires explicitly isolated headless Wayland compositor; never use desktop"]
    fn isolated_surface_lifecycle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        assert_eq!(
            std::env::var("PAL_PRIVATE_WAYLAND_TEST").as_deref(),
            Ok("1")
        );
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
        assert_eq!(
            std::env::var("PAL_PRIVATE_WAYLAND_RUNTIME").as_deref(),
            Ok(runtime.as_str()),
            "explicit private runtime required"
        );
        use std::os::unix::fs::PermissionsExt;
        assert!(std::path::Path::new(&runtime).is_absolute());
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700,
            "private runtime permissions required"
        );
        assert!(std::env::var_os("DISPLAY").is_none());
        struct Probe {
            ticks: Arc<AtomicUsize>,
            frames: Arc<AtomicUsize>,
        }
        impl GameHost for Probe {
            fn tick(&mut self) -> Result<bool, String> {
                self.ticks.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            }
            fn frame(&self) -> Result<IndexedFrame, String> {
                self.frames.fetch_add(1, Ordering::SeqCst);
                Ok(IndexedFrame {
                    indices: (0..WIDTH * HEIGHT).map(|i| (i * 13) as u8).collect(),
                    palette: std::array::from_fn(|i| [i as u8, (i * 7) as u8, (i * 11) as u8, 255]),
                    pixel_mask: 0xff,
                })
            }
            fn key(&mut self, _: KeyCode, _: bool) {}
        }
        struct Driver {
            app: App<Probe>,
            stage: usize,
            started: Instant,
        }
        impl ApplicationHandler for Driver {
            fn resumed(&mut self, e: &ActiveEventLoop) {
                self.app.resumed(e);
            }
            fn window_event(&mut self, e: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
                self.app.window_event(e, id, event);
            }
            fn about_to_wait(&mut self, e: &ActiveEventLoop) {
                let frames = self.app.host.frames.load(Ordering::SeqCst);
                if frames >= 3 && self.stage == 0 {
                    let _ = self
                        .app
                        .display
                        .as_ref()
                        .unwrap()
                        .window
                        .request_inner_size(PhysicalSize::new(641, 401));
                    self.stage = 1;
                }
                if frames >= 6 && self.stage == 1 {
                    if let Backend::Wgpu(gpu) = &mut self.app.display.as_mut().unwrap().backend {
                        gpu.resize(0, 0).unwrap();
                    }
                    self.app.suspended(e);
                    assert!(self.app.display.is_none());
                    self.app.resumed(e);
                    self.stage = 2;
                }
                if frames >= 12 || self.started.elapsed() > Duration::from_secs(5) {
                    self.app.stop(e, None);
                } else {
                    self.app.about_to_wait(e);
                }
            }
            fn exiting(&mut self, e: &ActiveEventLoop) {
                self.app.exiting(e);
            }
        }
        for renderer in [RendererKind::Wgpu, RendererKind::Softbuffer] {
            // winit event loops are single-use; select one per child process.
            let requested = std::env::var("PAL_TEST_RENDERER")
                .unwrap_or_else(|_| "wgpu".into())
                .parse::<RendererKind>()
                .unwrap();
            if renderer != requested {
                continue;
            }
            let ticks = Arc::new(AtomicUsize::new(0));
            let frames = Arc::new(AtomicUsize::new(0));
            let host = Probe {
                ticks: ticks.clone(),
                frames: frames.clone(),
            };
            let event_loop = EventLoop::builder()
                .with_wayland()
                .with_any_thread(true)
                .build()
                .unwrap();
            let mut driver = Driver {
                app: App::new(host, renderer),
                stage: 0,
                started: Instant::now(),
            };
            event_loop.run_app(&mut driver).unwrap();
            assert!(driver.app.failure.is_none(), "{:?}", driver.app.failure);
            assert!(driver.app.display.is_none());
            assert_eq!(driver.stage, 2);
            assert!(frames.load(Ordering::SeqCst) >= 12);
            assert!(ticks.load(Ordering::SeqCst) > frames.load(Ordering::SeqCst) * 2);
            eprintln!(
                "lifecycle renderer={renderer:?} ticks={} frames={} elapsed={:?} resize=yes suspend-recreate=yes cleanup=yes",
                ticks.load(Ordering::SeqCst),
                frames.load(Ordering::SeqCst),
                driver.started.elapsed()
            );
        }
    }

    #[test]
    fn production_window_attributes_set_stable_wayland_app_id() {
        // winit exposes platform settings through Debug, not a public getter.
        // Inspect the same attributes consumed by create_window without a GUI.
        let attributes = window_attributes();
        let debug = format!("{attributes:?}");
        assert!(
            debug.contains(
                r#"name: Some(ApplicationName { general: "pal-wayland", instance: "pal-wayland" })"#
            ),
            "{debug}"
        );
    }

    #[derive(Default)]
    struct InputOnly(Vec<(KeyCode, bool)>);

    impl GameHost for InputOnly {
        fn tick(&mut self) -> Result<bool, String> {
            panic!("input handling must not advance the core")
        }
        fn frame(&self) -> Result<IndexedFrame, String> {
            panic!("input handling must not fabricate or fetch a frame")
        }
        fn key(&mut self, key: KeyCode, pressed: bool) {
            self.0.push((key, pressed));
        }
    }

    #[test]
    fn pending_audio_retries_are_paced_without_a_full_presentation_period() {
        let start = Instant::now();
        let finished = start + Duration::from_millis(60);
        assert_eq!(
            next_host_tick(start, finished, Some(Duration::from_millis(1))),
            finished + Duration::from_millis(1)
        );
        assert_eq!(
            next_host_tick(start, finished, Some(Duration::ZERO)),
            finished + Duration::from_millis(1)
        );
        assert_eq!(
            next_host_tick(start, finished, None),
            finished + Duration::from_millis(1)
        );
    }

    #[test]
    fn bounded_tick_schedule_keeps_period_without_catch_up() {
        let start = Instant::now();
        let short = start + Duration::from_millis(5);
        assert_eq!(next_tick_after(start, short), start + TICK_INTERVAL);
        let slow = start + Duration::from_millis(30);
        assert_eq!(next_tick_after(start, slow), start + TICK_INTERVAL * 2);
        let exact = start + TICK_INTERVAL;
        assert_eq!(next_tick_after(start, exact), exact + TICK_INTERVAL);
    }
    #[test]
    fn physical_arrows_enter_and_escape_are_forwarded_without_remapping() {
        let mut host = InputOnly::default();
        let mut keys = HeldKeys::default();
        for key in [
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::KeyW,
        ] {
            keys.transition(&mut host, key, true);
            keys.transition(&mut host, key, true); // compositor auto-repeat
            keys.transition(&mut host, key, false);
            keys.transition(&mut host, key, false); // redundant synthetic release
            assert_eq!(&host.0[host.0.len() - 2..], &[(key, true), (key, false)]);
        }
        assert_eq!(host.0.len(), 14);
        assert!(keys.0.is_empty());
    }

    #[test]
    fn focus_loss_or_exit_releases_held_keys_exactly_once() {
        struct ReleaseOnly {
            calls: usize,
        }
        impl GameHost for ReleaseOnly {
            fn tick(&mut self) -> Result<bool, String> {
                unreachable!()
            }
            fn frame(&self) -> Result<IndexedFrame, String> {
                unreachable!()
            }
            fn key(&mut self, _: KeyCode, _: bool) {
                unreachable!()
            }
            fn release_input(&mut self) {
                self.calls += 1;
            }
        }
        let mut logical = ReleaseOnly { calls: 0 };
        HeldKeys::default().release_all(&mut logical);
        assert_eq!(
            logical.calls, 1,
            "logical cleanup runs even without physical holds"
        );
        let mut host = InputOnly::default();
        let mut keys = HeldKeys::default();
        for key in [KeyCode::ArrowLeft, KeyCode::Enter] {
            keys.transition(&mut host, key, true);
        }
        keys.release_all(&mut host);
        keys.release_all(&mut host);
        assert_eq!(host.0.len(), 4);
        assert!(keys.0.is_empty());
        assert!(host.0[2..].contains(&(KeyCode::ArrowLeft, false)));
        assert!(host.0[2..].contains(&(KeyCode::Enter, false)));
    }
}
