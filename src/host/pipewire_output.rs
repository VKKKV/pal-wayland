// SPDX-License-Identifier: GPL-3.0-or-later
//! Safe high-level pipewire-rs client, confined to one owned loop thread.
//! No RT_PROCESS: callbacks are serialized on this loop, never share Rust
//! listener user data across PipeWire's realtime and control threads. The PCM
//! callback itself does no allocation, locking, blocking, or source rendering.
use crate::{
    audio_output::{MAX_BUFFER_FRAMES, Shared},
    runtime::Fault,
};
use pipewire::{self as pw, properties::properties, spa};
use std::{
    cell::{Cell, RefCell},
    io::Cursor,
    rc::Rc,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const READY_TIMEOUT: Duration = Duration::from_secs(2);
const ITERATE_TIMEOUT: Duration = Duration::from_millis(5);
type Started = (rtrb::Producer<[u8; 4]>, JoinHandle<()>, Receiver<String>);

pub(crate) fn start(shared: Arc<Shared>) -> Result<Started, Fault> {
    let (producer, consumer) = rtrb::RingBuffer::new(MAX_BUFFER_FRAMES);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (error_tx, errors) = mpsc::sync_channel(1);
    let worker_shared = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("pal-wayland-pipewire".into())
        .spawn(move || {
            if let Err(error) = run(consumer, worker_shared, &ready_tx) {
                let _ = ready_tx.try_send(Err(error.clone()));
                let _ = error_tx.try_send(error);
            }
        })
        .map_err(|e| Fault(format!("PCM PipeWire thread: {e}")))?;
    match ready_rx.recv_timeout(READY_TIMEOUT + Duration::from_millis(100)) {
        Ok(Ok(())) => Ok((producer, worker, errors)),
        result => {
            shared.stop.store(true, Ordering::Release);
            drop(producer);
            let deadline = Instant::now() + Duration::from_millis(250);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            Err(Fault(match result {
                Ok(Err(e)) => e,
                _ => "PCM PipeWire readiness timed out or worker stopped".into(),
            }))
        }
    }
}

struct Callback {
    consumer: rtrb::Consumer<[u8; 4]>,
    shared: Arc<Shared>,
    negotiated: Rc<Cell<bool>>,
    callback_fault: Rc<Cell<Option<&'static str>>>,
}

fn format_matches(info: &spa::param::audio::AudioInfoRaw) -> bool {
    info.format() == spa::param::audio::AudioFormat::S16
        && info.rate() == 44_100
        && info.channels() == 2
        && info.position()[..2]
            == [
                spa::sys::SPA_AUDIO_CHANNEL_FL,
                spa::sys::SPA_AUDIO_CHANNEL_FR,
            ]
}
fn format_pod() -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16);
    info.set_rate(44_100);
    info.set_channels(2);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[..2].copy_from_slice(&[
        spa::sys::SPA_AUDIO_CHANNEL_FL,
        spa::sys::SPA_AUDIO_CHANNEL_FR,
    ]);
    info.set_position(positions);
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map(|v| v.0.into_inner())
    .map_err(|e| format!("PCM PipeWire format serialization: {e:?}"))
}
fn run(
    consumer: rtrb::Consumer<[u8; 4]>,
    shared: Arc<Shared>,
    ready: &SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let fail = |e| format!("PCM PipeWire: {e}");
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(fail)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(fail)?;
    let core = context.connect_rc(None).map_err(fail)?;
    let control_fault = Rc::new(RefCell::new(None));
    let core_fault = Rc::clone(&control_fault);
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            *core_fault.borrow_mut() = Some(format!("PCM PipeWire core error: {message}"));
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "pal-wayland PCM",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Game",
            *pw::keys::APP_NAME => "pal-wayland",
            *pw::keys::NODE_NAME => "pal-wayland-pcm",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => "882/44100",
        },
    )
    .map_err(fail)?;
    let negotiated = Rc::new(Cell::new(false));
    let callback_fault = Rc::new(Cell::new(None));
    let drained = Rc::new(Cell::new(false));
    let drained_event = Rc::clone(&drained);
    let state_fault = Rc::clone(&control_fault);
    let _listener = stream
        .add_local_listener_with_user_data(Callback {
            consumer,
            shared: Arc::clone(&shared),
            negotiated: Rc::clone(&negotiated),
            callback_fault: Rc::clone(&callback_fault),
        })
        .state_changed(move |_, _, _, new| {
            if let pw::stream::StreamState::Error(error) = new {
                *state_fault.borrow_mut() = Some(format!("PCM PipeWire stream error: {error}"));
            }
        })
        .param_changed(|_, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            data.negotiated.set(false);
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if param.is_some_and(|p| info.parse(p).is_ok()) && format_matches(&info) {
                data.negotiated.set(true);
            } else {
                data.callback_fault.set(Some(
                    "PCM PipeWire rejected negotiated format (requires native S16/44100/FL,FR)",
                ));
            }
        })
        .drained(move |_, _| {
            drained_event.set(true);
        })
        .process(|stream, data| {
            if !data.negotiated.get() || data.shared.stop.load(Ordering::Acquire) {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(block) = buffer.datas_mut().first_mut() else {
                data.callback_fault
                    .set(Some("PCM PipeWire missing interleaved buffer"));
                return;
            };
            let Some(bytes) = block.data() else {
                data.callback_fault
                    .set(Some("PCM PipeWire unmapped buffer"));
                return;
            };
            // The entire copy, including callback-in-flight frames, stays reserved
            // until Buffer::drop queues it. No queue entries are skipped on underrun.
            let frames = (bytes.len() / 4)
                .min(if requested == 0 {
                    MAX_BUFFER_FRAMES
                } else {
                    requested
                })
                .min(MAX_BUFFER_FRAMES);
            if frames == 0 {
                data.callback_fault
                    .set(Some("PCM PipeWire zero-size buffer"));
                return;
            }
            let (authored, silence) = fill_pcm(&mut data.consumer, bytes, frames);
            let output_frames = if data.consumer.is_abandoned() {
                authored
            } else {
                frames
            };
            let chunk = block.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = (output_frames * 4) as u32;
            drop(buffer); // maintained safe wrapper queues to PipeWire here
            data.shared
                .sink_written_frames
                .fetch_add(authored as u64, Ordering::Release);
            data.shared.underrun_silence_frames.fetch_add(
                if output_frames == frames {
                    silence as u64
                } else {
                    0
                },
                Ordering::Release,
            );
            data.shared
                .outstanding
                .fetch_sub(authored, Ordering::AcqRel);
        })
        .register()
        .map_err(fail)?;
    let format = format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or("PCM PipeWire invalid format pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(fail)?;
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut announced = false;
    let mut draining = false;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(error) = control_fault.borrow_mut().take() {
            return Err(error);
        }
        if let Some(error) = callback_fault.take() {
            return Err(error.into());
        }
        let state = stream.state();
        if announced && !draining && !matches!(state, pw::stream::StreamState::Streaming) {
            return Err(format!(
                "PCM PipeWire stream stopped unexpectedly: {state:?}"
            ));
        }
        if !announced && negotiated.get() && matches!(state, pw::stream::StreamState::Streaming) {
            let _ = ready.try_send(Ok(()));
            announced = true;
        }
        if !announced && Instant::now() >= deadline {
            return Err("PCM PipeWire negotiation/stream readiness timed out".into());
        }
        // The producer is dropped by finish; a distinct atomic records closure
        // since the consumer is confined to callback user data.
        if announced
            && shared.finish.load(Ordering::Acquire)
            && shared.outstanding.load(Ordering::Acquire) == 0
            && !draining
        {
            draining = true;
            stream.flush(true).map_err(fail)?;
        }
        if draining && drained.get() {
            return Ok(());
        }
        if mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(ITERATE_TIMEOUT))
            < 0
        {
            return Err("PCM PipeWire loop iteration failed".into());
        }
    }
}

/// Bounded, allocation-free FIFO transfer; silence does not consume future PCM.
fn fill_pcm(
    consumer: &mut rtrb::Consumer<[u8; 4]>,
    bytes: &mut [u8],
    frames: usize,
) -> (usize, usize) {
    // Snapshot available FIFO entries once. Concurrent publication must wait
    // for the next buffer, so underrun silence is always a suffix, never an
    // interleaved gap that finish could trim over queued authored PCM.
    let authored = consumer.slots().min(frames);
    for frame in bytes[..authored * 4].chunks_exact_mut(4) {
        if let Ok(pcm) = consumer.pop() {
            frame.copy_from_slice(&pcm);
        }
    }
    bytes[authored * 4..frames * 4].fill(0);
    (authored, frames - authored)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_fifo_underrun_does_not_discard_later_pcm() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(4);
        producer.push([1, 2, 3, 4]).unwrap();
        producer.push([5, 6, 7, 8]).unwrap();
        let mut bytes = [99; 16];
        assert_eq!(fill_pcm(&mut consumer, &mut bytes, 4), (2, 2));
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        producer.push([9, 10, 11, 12]).unwrap();
        assert_eq!(fill_pcm(&mut consumer, &mut bytes, 1), (1, 0));
        assert_eq!(bytes[..4], [9, 10, 11, 12]);
    }
    #[test]
    fn exact_format_roundtrip_and_mismatch_rejection() {
        let bytes = format_pod().unwrap();
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.parse(spa::pod::Pod::from_bytes(&bytes).unwrap())
            .unwrap();
        assert!(format_matches(&info));
        info.set_rate(48_000);
        assert!(!format_matches(&info));
        info.set_rate(44_100);
        info.set_channels(1);
        assert!(!format_matches(&info));
    }
}
