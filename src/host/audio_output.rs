// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit optional host PCM transport: in-process PipeWire, no SDL fallback.
//! Nothing opens until a constructor is called. Production waits for negotiated
//! streaming readiness, not audible-device readiness. CD position means source
//! drained; sink submission and underrun silence are separately reported.
//!
//! Arm with `advance_to` BEFORE guest execution, using the window's monotonic
//! wall clock (not virtual guest time). Drain before PLAY/STOP/PAUSE transitions
//! so preceding elapsed time belongs to the old state. The first timestamp is
//! the startup baseline, never elapsed debt measured from zero. Do not combine
//! this consumer with WAV capture on the same CdRom.
use crate::{
    audio::{PlaybackState, SAMPLE_RATE},
    cdrom::CdRom,
    runtime::Fault,
};
use std::{
    io::{self, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// 50 ms per producer call; rejected intervals retain all elapsed debt.
pub const MAX_ADVANCE_FRAMES: usize = 2205;
/// 100 ms queued PLUS callback/worker-in-flight PCM; excludes downstream buffers.
pub const MAX_BUFFER_FRAMES: usize = 4410;
/// Retry pacing is shared by the headless caller and window scheduler.
pub const RECOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(1);
/// A stopped reader must fail, not freeze guest execution indefinitely.
pub const RECOVERY_STALL_TIMEOUT: Duration = Duration::from_millis(250);
pub const RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// One bounded producer slice. Pending is scheduling debt, NOT a source fault.
/// While pending, callers must not execute guest instructions/transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioService {
    Ready(OutputProgress),
    Pending {
        progress: OutputProgress,
        debt_frames: u64,
    },
}
impl AudioService {
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready(_))
    }
}
struct Recovery {
    started: Instant,
    progressed: Instant,
    submitted: u64,
    written_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputProgress {
    /// Complete frames accepted into our bounded queue, including silence.
    pub submitted_frames: u64,
    /// Actual CD source frames drained, including muted PCM, not device position.
    pub source_frames: u64,
    pub sfx_frames: u64,
    /// Complete frames written to an explicitly requested helper. NOT played.
    pub pipe_written_frames: u64,
    /// Complete authored frames queued to PipeWire. NOT device-played frames.
    pub sink_written_frames: u64,
    /// Silence inserted by PipeWire processing because no authored PCM was due.
    pub underrun_silence_frames: u64,
    /// No reliable device-consumption clock is exposed by this adapter.
    pub device_played_frames: Option<u64>,
    /// Queue plus in-flight reservation; released after helper batch write or
    /// native callback buffer queueing, never merely on ring dequeue.
    pub buffered_frames: usize,
}
struct Batch {
    bytes: Vec<u8>,
    frames: usize,
}
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) outstanding: AtomicUsize,
    pub(crate) written_bytes: AtomicU64,
    pub(crate) sink_written_frames: AtomicU64,
    pub(crate) underrun_silence_frames: AtomicU64,
    pub(crate) stop: AtomicBool,
    pub(crate) finish: AtomicBool,
}

/// Single-producer main-thread adapter with a bounded, nonblocking I/O worker.
/// Failed/overloaded output must stop the run (or be explicitly disabled by the
/// caller), never silently continue rendering/draining CD into a dead sink.
pub struct AudioOutput {
    backend: Backend,
    worker: Option<JoinHandle<()>>,
    errors: Receiver<String>,
    shared: Arc<Shared>,
    origin_ns: Option<u64>,
    last_ns: u64,
    submitted_frames: u64,
    source_frames: u64,
    sfx_frames: u64,
    fault: Option<Fault>,
    observed_ns: u64,
    recovery: Option<Recovery>,
    volume: u8,
    muted: bool,
    // Producer-owned, bounded scratch; never shared with a callback or helper.
    pcm: Vec<[i16; 2]>,
}
enum Backend {
    Helper {
        child: Child,
        sender: Option<SyncSender<Batch>>,
    },
    PipeWire {
        producer: Option<rtrb::Producer<[u8; 4]>>,
    },
}
impl AudioOutput {
    /// Production in-process client. Returns only after fixed-format negotiation
    /// and STREAMING, under a two-second readiness deadline. No server launch,
    /// subprocess, fallback, or reconnect is attempted.
    pub fn start_pipewire() -> Result<Self, Fault> {
        let shared = Arc::new(Shared::default());
        let (producer, worker, errors) = crate::pipewire_output::start(Arc::clone(&shared))?;
        Ok(Self {
            backend: Backend::PipeWire {
                producer: Some(producer),
            },
            worker: Some(worker),
            errors,
            shared,
            origin_ns: None,
            last_ns: 0,
            submitted_frames: 0,
            source_frames: 0,
            sfx_frames: 0,
            fault: None,
            observed_ns: 0,
            recovery: None,
            volume: 100,
            muted: false,
            pcm: Vec::with_capacity(MAX_ADVANCE_FRAMES),
        })
    }
    /// Explicit trusted helper override for silent transports/tests. No shell is
    /// added; caller owns exact argv/environment. Helper must read raw native-
    /// endian 44100-Hz stereo i16 from stdin. Socketpair stdin permits safe-Rust
    /// nonblocking writes and bounded worker cancellation even if it stops
    /// reading. Stdout is discarded; stderr inherited, never an unread pipe.
    /// Constructor success means spawned, not ready/consuming/playing.
    pub fn spawn_transport(mut command: Command) -> Result<Self, Fault> {
        let (writer, reader) = UnixStream::pair().map_err(io_fault)?;
        writer.set_nonblocking(true).map_err(io_fault)?;
        command
            .stdin(Stdio::from(OwnedFd::from(reader)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|e| Fault(format!("PCM helper spawn: {e}")))?;
        let shared = Arc::new(Shared::default());
        // Every nonempty batch reserves >=1 frame until fully written, so this
        // slot count cannot saturate while a frame reservation is available.
        let (sender, receiver) = mpsc::sync_channel(MAX_BUFFER_FRAMES);
        let (error_sender, errors) = mpsc::sync_channel(1);
        let worker_shared = Arc::clone(&shared);
        let worker = match thread::Builder::new()
            .name("pal-wayland-pcm-output".into())
            .spawn(move || {
                if let Err(error) = transport_worker(writer, receiver, &worker_shared) {
                    let _ = error_sender.try_send(error.to_string());
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io_fault(error));
            }
        };
        let mut output = Self {
            backend: Backend::Helper {
                child,
                sender: Some(sender),
            },
            worker: Some(worker),
            errors,
            shared,
            origin_ns: None,
            last_ns: 0,
            submitted_frames: 0,
            source_frames: 0,
            sfx_frames: 0,
            fault: None,
            observed_ns: 0,
            recovery: None,
            volume: 100,
            muted: false,
            pcm: Vec::with_capacity(MAX_ADVANCE_FRAMES),
        };
        output.poll()?;
        Ok(output)
    }

    /// Sink-only gain; DMA/CD consumption and capture remain unchanged.
    /// Already queued frames keep their gain. Never opens an inactive backend.
    pub fn set_gain(&mut self, volume: u8, muted: bool) -> Result<(), Fault> {
        if volume > 100 {
            return Err(Fault("audio volume must be 0..=100".into()));
        }
        self.volume = volume;
        self.muted = muted;
        Ok(())
    }
    pub fn gain(&self) -> (u8, bool) {
        (self.volume, self.muted)
    }
    pub fn progress(&self) -> OutputProgress {
        OutputProgress {
            submitted_frames: self.submitted_frames,
            source_frames: self.source_frames,
            sfx_frames: self.sfx_frames,
            pipe_written_frames: self.shared.written_bytes.load(Ordering::Acquire) / 4,
            sink_written_frames: self.shared.sink_written_frames.load(Ordering::Acquire),
            underrun_silence_frames: self.shared.underrun_silence_frames.load(Ordering::Acquire),
            device_played_frames: None,
            buffered_frames: self.shared.outstanding.load(Ordering::Acquire),
        }
    }
    /// Nonblocking process/worker health check. An unsolicited exit, even code
    /// zero, is terminal while running. Poll even when no PCM is due.
    pub fn poll(&mut self) -> Result<OutputProgress, Fault> {
        self.healthy()?;
        if let Ok(error) = self.errors.try_recv() {
            return self.poison(self.transport_error(error));
        }
        if let Backend::Helper { child, .. } = &mut self.backend {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return self.poison(format!("PCM helper exited unexpectedly: {status}"));
                }
                Err(error) => return self.poison(format!("PCM helper status: {error}")),
                Ok(None) => {}
            }
        }
        if self.worker.as_ref().is_none_or(|w| w.is_finished()) {
            return self.poison("PCM worker stopped unexpectedly".into());
        }
        Ok(self.progress())
    }
    fn transport_error(&self, error: String) -> String {
        match self.backend {
            Backend::Helper { .. } => format!("PCM helper write: {error}"),
            Backend::PipeWire { .. } => error,
        }
    }
    fn healthy(&self) -> Result<(), Fault> {
        match &self.fault {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
    fn poison<T>(&mut self, message: String) -> Result<T, Fault> {
        let fault = Fault(message);
        self.fault = Some(fault.clone());
        Err(fault)
    }

    /// Queue floor((timestamp - startup_baseline) * 44100 / 1e9) frames.
    /// All frame capacity is reserved BEFORE touching CD state. Backpressure,
    /// backward timestamps and excessive catch-up leave source and timeline
    /// untouched, return Fault, and retain debt. No silent truncation/rebase.
    /// Source/worker failures are sticky; already drained source cannot be
    /// rolled back, so callers must not ignore these failures and keep running.
    pub fn advance_to(&mut self, elapsed_ns: u64, cd: &mut CdRom) -> Result<OutputProgress, Fault> {
        self.advance_with(elapsed_ns, |pcm| render_cd(cd, pcm).map(|n| (n, 0)))
    }
    /// Arm at ns=0 before guest execution. One Machine clock drives both sources.
    /// Reservation/backpressure checks precede hardware advance and PCM drain.
    pub fn advance_machine(
        &mut self,
        ns: u64,
        m: &mut crate::runtime::Machine,
    ) -> Result<OutputProgress, Fault> {
        if self.origin_ns.is_none() && ns != 0 {
            return Err(Fault(
                "mixed PCM must be armed at Machine timeline zero before guest execution".into(),
            ));
        }
        let start = self.submitted_frames;
        self.advance_with(ns, |pcm| {
            m.prepare_realtime_audio(start, ns, pcm.len())?;
            m.render_mixed_audio(pcm)
        })
    }
    /// Nonblocking realtime recovery: observe the complete wall-time target but
    /// admit at most 2205 frames and available queue capacity on each call.
    /// No dropping, clock rebasing or historical hardware advance. Repeated
    /// Pending results require paced calls and no intervening guest execution.
    /// An episode is bounded by wall time AND a no-progress watchdog, including
    /// worker partial writes; source/helper/clock failures remain fatal/sticky.
    pub fn service_machine(
        &mut self,
        ns: u64,
        m: &mut crate::runtime::Machine,
    ) -> Result<AudioService, Fault> {
        self.service_machine_at(ns, m, Instant::now())
    }
    fn service_machine_at(
        &mut self,
        ns: u64,
        m: &mut crate::runtime::Machine,
        now: Instant,
    ) -> Result<AudioService, Fault> {
        self.poll()?;
        if self.origin_ns.is_none() {
            let p = self.advance_machine(ns, m)?;
            self.observed_ns = ns;
            return Ok(AudioService::Ready(p));
        }
        if self.origin_ns != Some(0) {
            return self.poison("mixed PCM recovery requires timeline zero".into());
        }
        if ns < self.observed_ns || ns < self.last_ns {
            return self.poison("PCM timestamp moved backwards".into());
        }
        self.observed_ns = ns;
        let written_bytes = self.shared.written_bytes.load(Ordering::Acquire)
            + self.shared.sink_written_frames.load(Ordering::Acquire) * 4;
        if let Some(r) = &mut self.recovery {
            if self.submitted_frames > r.submitted || written_bytes > r.written_bytes {
                r.progressed = now;
                r.submitted = self.submitted_frames;
                r.written_bytes = written_bytes;
            }
            if now.duration_since(r.started) >= RECOVERY_TIMEOUT {
                return self
                    .poison("PCM catch-up recovery timed out; elapsed debt retained".into());
            }
            if now.duration_since(r.progressed) >= RECOVERY_STALL_TIMEOUT {
                return self.poison("PCM catch-up made no progress; elapsed debt retained".into());
            }
        }
        let target = (u128::from(ns) * u128::from(SAMPLE_RATE) / NANOS_PER_SECOND) as u64;
        let pending = target - self.submitted_frames;
        let available = MAX_BUFFER_FRAMES - self.shared.outstanding.load(Ordering::Acquire);
        let frames = pending.min(MAX_ADVANCE_FRAMES as u64).min(available as u64);
        if frames != 0 {
            // Ceil gives the exact cumulative frame boundary; the final slice
            // uses the caller timestamp to retain fractional-frame scheduling.
            let end = if frames == pending {
                ns
            } else {
                (u128::from(self.submitted_frames + frames) * NANOS_PER_SECOND)
                    .div_ceil(u128::from(SAMPLE_RATE)) as u64
            };
            self.advance_machine(end, m)?;
        } else if pending == 0 {
            self.last_ns = ns;
        }
        let debt_frames = target - self.submitted_frames;
        let progress = self.progress();
        if debt_frames == 0 {
            self.recovery = None;
            return Ok(AudioService::Ready(progress));
        }
        self.recovery.get_or_insert(Recovery {
            started: now,
            progressed: now,
            submitted: self.submitted_frames,
            written_bytes,
        });
        Ok(AudioService::Pending {
            progress,
            debt_frames,
        })
    }
    fn advance_with(
        &mut self,
        elapsed_ns: u64,
        mut render: impl FnMut(&mut [[i16; 2]]) -> Result<(usize, usize), Fault>,
    ) -> Result<OutputProgress, Fault> {
        self.poll()?;
        let Some(origin) = self.origin_ns else {
            self.origin_ns = Some(elapsed_ns);
            self.last_ns = elapsed_ns;
            return Ok(self.progress());
        };
        if elapsed_ns < self.last_ns {
            return Err(Fault("PCM timestamp moved backwards".into()));
        }
        let target =
            (u128::from(elapsed_ns - origin) * u128::from(SAMPLE_RATE) / NANOS_PER_SECOND) as u64;
        let pending = target - self.submitted_frames;
        if pending > MAX_ADVANCE_FRAMES as u64 {
            return Err(Fault(
                "PCM per-call catch-up budget exceeded; elapsed debt retained".into(),
            ));
        }
        if pending == 0 {
            self.last_ns = elapsed_ns;
            return Ok(self.progress());
        }
        let frames = pending as usize;
        if self
            .shared
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(frames).filter(|n| *n <= MAX_BUFFER_FRAMES)
            })
            .is_err()
        {
            return Err(Fault(
                "PCM output backpressure; source untouched, elapsed debt retained".into(),
            ));
        }
        if let Backend::PipeWire { producer } = &self.backend
            && producer
                .as_ref()
                .is_none_or(|p| p.is_abandoned() || p.slots() < frames)
        {
            self.shared.outstanding.fetch_sub(frames, Ordering::AcqRel);
            return self.poison(
                "PCM PipeWire ring capacity/connection invariant failed before source render"
                    .into(),
            );
        }
        let result = self.render_batch(frames, &mut render);
        let batch = match result {
            Ok(batch) => batch,
            Err(error) => {
                self.shared.outstanding.fetch_sub(frames, Ordering::AcqRel);
                return self.poison(format!("PCM source: {error}"));
            }
        };
        // Recheck asynchronous failure after rendering; terminal failure is
        // explicit, never reported as a successful submission or device play.
        if let Err(error) = self.poll() {
            self.shared.outstanding.fetch_sub(frames, Ordering::AcqRel);
            return Err(error);
        }
        let sent = match &mut self.backend {
            Backend::Helper { sender, .. } => sender.as_ref().unwrap().try_send(batch).is_ok(),
            Backend::PipeWire { producer } => {
                let producer = producer.as_mut().unwrap();
                // One atomic chunk publication: callback cannot observe a partial
                // batch; ring slots and total in-flight capacity were reserved
                // before rendering source PCM.
                match producer.write_chunk(batch.frames) {
                    Ok(mut chunk) => {
                        let (first, second) = chunk.as_mut_slices();
                        for (slot, bytes) in first
                            .iter_mut()
                            .chain(second.iter_mut())
                            .zip(batch.bytes.chunks_exact(4))
                        {
                            *slot = [bytes[0], bytes[1], bytes[2], bytes[3]];
                        }
                        chunk.commit_all();
                        true
                    }
                    Err(_) => false,
                }
            }
        };
        if !sent {
            self.shared.outstanding.fetch_sub(frames, Ordering::AcqRel);
            return self.poison("PCM worker queue disconnected or invariant violated".into());
        }
        self.submitted_frames = target;
        self.last_ns = elapsed_ns;
        Ok(self.progress())
    }
    fn render_batch(
        &mut self,
        frames: usize,
        render: &mut impl FnMut(&mut [[i16; 2]]) -> Result<(usize, usize), Fault>,
    ) -> Result<Batch, Fault> {
        self.pcm.resize(frames, [0; 2]);
        // The renderer contract allows partial writes (silence/EOF). Clear
        // retained samples even when this batch is smaller than the last one.
        self.pcm.fill([0; 2]);
        let (cd, sfx) = render(&mut self.pcm)?;
        self.source_frames += cd as u64;
        self.sfx_frames += sfx as u64;
        let mut bytes = Vec::with_capacity(frames * 4);
        for frame in &self.pcm {
            for &sample in frame {
                let sample = sink_sample(sample, self.volume, self.muted);
                bytes.extend_from_slice(&sample.to_ne_bytes());
            }
        }
        Ok(Batch { bytes, frames })
    }

    fn close_producer(&mut self) {
        self.shared.finish.store(true, Ordering::Release);
        match &mut self.backend {
            Backend::Helper { sender, .. } => {
                sender.take();
            }
            Backend::PipeWire { producer } => {
                producer.take();
            }
        }
    }
    /// Bounded end-of-run drain, never call from a window tick. PipeWire drains
    /// the submitted FIFO and waits for stream drained; helpers deliver stdin
    /// and exit. Neither mode reports these as speaker/device-consumed frames.
    /// Timeout/error cooperatively cancels the backend through Drop.
    pub fn finish(mut self, timeout: Duration) -> Result<OutputProgress, Fault> {
        self.healthy()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Fault("PCM finish deadline overflow".into()))?;
        self.close_producer();
        loop {
            if let Ok(error) = self.errors.try_recv() {
                return Err(Fault(self.transport_error(error)));
            }
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
            {
                self.worker
                    .take()
                    .unwrap()
                    .join()
                    .map_err(|_| Fault("PCM worker panicked".into()))?;
                if let Ok(error) = self.errors.try_recv() {
                    return Err(Fault(self.transport_error(error)));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(Fault("PCM finish timed out".into()));
            }
            thread::sleep(Duration::from_millis(1));
        }
        let Backend::Helper { child, .. } = &mut self.backend else {
            return Ok(self.progress());
        };
        loop {
            if let Some(status) = child.try_wait().map_err(io_fault)? {
                if !status.success() {
                    return Err(Fault(format!("PCM helper finish exit: {status}")));
                }
                return Ok(self.progress());
            }
            if Instant::now() >= deadline {
                return Err(Fault("PCM helper exit timed out".into()));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for AudioOutput {
    /// Abort (not drain): queued audio may remain undelivered. Never joins a
    /// worker blocked on child stdin: socket writes are nonblocking, cancellation
    /// checked on every retry, and the owned helper is killed/reaped explicitly.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.close_producer();
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        if let Backend::Helper { child, .. } = &mut self.backend {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        // Cooperative native loop has a finite iterate timeout. Never perform
        // an unbounded join if a foreign library misbehaves during teardown.
        let deadline = Instant::now() + Duration::from_millis(250);
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}
fn render_cd(cd: &mut CdRom, pcm: &mut [[i16; 2]]) -> Result<usize, Fault> {
    let mut offset = 0;
    let mut consumed = 0;
    while offset < pcm.len() {
        let mut count = pcm.len() - offset;
        if cd
            .audio_player()
            .is_some_and(|p| p.state() == PlaybackState::Playing)
        {
            cd.prefetch_audio()?;
            let queued = cd.audio_player().unwrap().queued_frames();
            if queued == 0 {
                return Err(Fault("playing CD has no queued PCM".into()));
            }
            count = count.min(queued);
        }
        consumed += cd.render_audio(&mut pcm[offset..offset + count]);
        offset += count;
    }
    Ok(consumed)
}
fn sink_sample(sample: i16, volume: u8, muted: bool) -> i16 {
    if muted {
        0
    } else {
        (i32::from(sample) * i32::from(volume) / 100)
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
    }
}
fn io_fault(error: io::Error) -> Fault {
    Fault(format!("PCM output I/O: {error}"))
}
fn transport_worker(
    mut writer: UnixStream,
    receiver: Receiver<Batch>,
    shared: &Shared,
) -> io::Result<()> {
    while let Ok(batch) = receiver.recv() {
        let mut offset = 0;
        while offset < batch.bytes.len() {
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            match writer.write(&batch.bytes[offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "helper stdin write returned zero",
                    ));
                }
                Ok(n) => {
                    offset += n;
                    shared.written_bytes.fetch_add(n as u64, Ordering::Release);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::park_timeout(Duration::from_millis(2))
                }
                Err(error) => return Err(error),
            }
        }
        shared.outstanding.fetch_sub(batch.frames, Ordering::AcqRel);
    }
    Ok(())
}

#[cfg(test)]
mod recovery_deadline_tests {
    use super::*;

    #[test]
    fn sink_gain_extremes_are_signed_and_identity_at_default() {
        for sample in [i16::MIN, -32767, -1, 0, 1, 32766, i16::MAX] {
            assert_eq!(sink_sample(sample, 100, false), sample);
            assert_eq!(sink_sample(sample, 0, false), 0);
            assert_eq!(sink_sample(sample, 100, true), 0);
            assert_eq!(
                i32::from(sink_sample(sample, 50, false)),
                i32::from(sample) / 2
            );
        }
    }

    #[test]
    fn actual_sink_bytes_change_without_changing_render_or_source_progress() {
        let dir = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join(format!(
            "pal-gain-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("pcm");
        let mut c = Command::new("python3");
        c.args([
            "-c",
            "import sys;open(sys.argv[1],'xb').write(sys.stdin.buffer.read())",
        ]);
        c.arg(&path);
        let mut o = AudioOutput::spawn_transport(c).unwrap();
        assert_eq!(o.gain(), (100, false));
        assert!(o.set_gain(101, false).is_err());
        assert_eq!(o.gain(), (100, false));
        // Reuse one producer buffer across growing/shrinking batches. Partial
        // renderers must see fresh silence, not the previous batch's samples.
        let scratch = o.pcm.as_ptr();
        for count in [MAX_ADVANCE_FRAMES, 3, 80] {
            let batch = o
                .render_batch(count, &mut |pcm| {
                    assert!(pcm.iter().all(|frame| *frame == [0; 2]));
                    pcm[0] = [1234, -5678];
                    Ok((0, 0))
                })
                .unwrap();
            assert_eq!(o.pcm.as_ptr(), scratch);
            assert_eq!(batch.bytes.len(), count * 4);
            let first: Vec<u8> = [1234i16, -5678]
                .into_iter()
                .flat_map(i16::to_ne_bytes)
                .collect();
            assert_eq!(&batch.bytes[..4], first);
            assert!(batch.bytes[4..].iter().all(|byte| *byte == 0));
        }
        let render = |pcm: &mut [[i16; 2]]| {
            pcm.fill([i16::MIN, i16::MAX]);
            Ok((pcm.len(), pcm.len()))
        };
        o.advance_with(0, render).unwrap();
        o.advance_with(1_000_000, render).unwrap();
        o.set_gain(50, false).unwrap();
        o.advance_with(2_000_000, render).unwrap();
        o.set_gain(50, true).unwrap();
        o.advance_with(3_000_000, render).unwrap();
        o.set_gain(100, false).unwrap();
        o.advance_with(4_000_000, render).unwrap();
        let p = o.finish(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (
                p.submitted_frames,
                p.source_frames,
                p.sfx_frames,
                p.pipe_written_frames
            ),
            (176, 176, 176, 176)
        );
        let samples: Vec<i16> = std::fs::read(&path)
            .unwrap()
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();
        for (batch, expected) in samples.chunks_exact(88).zip([
            [i16::MIN, i16::MAX],
            [-16384, 16383],
            [0, 0],
            [i16::MIN, i16::MAX],
        ]) {
            for frame in batch.chunks_exact(2) {
                assert_eq!(frame, expected);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    use crate::{runtime::Machine, sound_blaster::SoundBlaster};

    #[test]
    fn absolute_deadline_bounds_recovery_even_with_recent_worker_progress() {
        let mut bytes = vec![0; 544];
        for (i, word) in [
            0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
        }
        let mut m = Machine::from_mz(&bytes, 0x1010).unwrap();
        m.sound_blaster = Some(SoundBlaster::default());
        m.retain_sfx_pcm = true;
        m.virtual_elapsed_ns = Some(0);
        let mut c = Command::new("python3");
        c.args(["-c", "import time;time.sleep(30)"]);
        let mut o = AudioOutput::spawn_transport(c).unwrap();
        o.advance_machine(0, &mut m).unwrap();
        let now = Instant::now();
        o.recovery = Some(Recovery {
            started: now - RECOVERY_TIMEOUT,
            progressed: now,
            submitted: 0,
            written_bytes: 0,
        });
        let before = m.sound_blaster.as_ref().unwrap().status();
        let e = o.service_machine_at(100_000_000, &mut m, now).unwrap_err();
        assert!(e.0.contains("recovery timed out"));
        assert_eq!(m.sound_blaster.as_ref().unwrap().status(), before);
        assert_eq!(o.progress().submitted_frames, 0);
        assert!(o.poll().is_err());
    }
}
