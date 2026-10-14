// SPDX-License-Identifier: GPL-3.0-or-later
use pal_wayland::audio::*;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    directory: PathBuf,
    bin: PathBuf,
    cue: PathBuf,
    frames: Vec<StereoFrame>,
}
impl Fixture {
    fn new() -> Self {
        let scratch = PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required"));
        let directory = scratch.join(format!(
            "pal-audio-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let bin = directory.join("fixture.bin");
        let cue = directory.join("fixture.cue");
        let mut raw = vec![0x5a; SECTOR_BYTES]; // recognizable data, never valid audio
        let frames: Vec<StereoFrame> = (0..3 * FRAMES_PER_SECTOR as usize)
            .map(|i| match i {
                0 => [i16::MIN, i16::MAX],
                1 => [0x1234, -0x1234],
                2 => [1, -1],
                _ => [(i as i16 * 17) - 15_000, 15_000 - i as i16 * 17],
            })
            .collect();
        for frame in &frames {
            for sample in frame {
                raw.extend(sample.to_le_bytes());
            }
        }
        fs::write(&bin, raw).unwrap();
        fs::write(&cue, Self::cue_text()).unwrap();
        Self {
            directory,
            bin,
            cue,
            frames,
        }
    }
    fn cue_text() -> &'static str {
        "FILE \"fixture.bin\" BINARY\n TRACK 01 MODE1/2352\n INDEX 01 00:00:00\n TRACK 02 AUDIO\n INDEX 01 00:00:01\n TRACK 03 AUDIO\n INDEX 01 00:00:03\n"
    }
    fn image(&self) -> CdAudioImage {
        CdAudioImage::mount_bin_cue(&self.bin, &self.cue).unwrap()
    }
    fn player(&self, capacity: usize) -> CdPlayer {
        CdPlayer::new(self.image(), capacity).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn sector_decoder_preserves_signed_little_endian_stereo_exactly() {
    let mut raw = vec![0; SECTOR_BYTES];
    raw[..12].copy_from_slice(&[
        0x00, 0x80, 0xff, 0x7f, 0x34, 0x12, 0xcc, 0xed, 1, 0, 0xff, 0xff,
    ]);
    let mut output = vec![[42, 42]; 588];
    decode_sectors(&raw, &mut output).unwrap();
    assert_eq!(
        &output[..3],
        &[[i16::MIN, i16::MAX], [0x1234, -0x1234], [1, -1]]
    );
    assert!(output[3..].iter().all(|f| *f == [0, 0]));
    let before = output.clone();
    assert!(decode_sectors(&raw[..2351], &mut output).is_err());
    assert_eq!(output, before);
    assert!(decode_sectors(&raw, &mut output[..587]).is_err());
    assert_eq!(output, before);
    decode_sectors(&[], &mut []).unwrap();
}

#[test]
fn cue_track_bounds_and_exact_duration_are_file_relative() {
    let fixture = Fixture::new();
    let mut image = fixture.image();
    assert_eq!(image.sectors(), 4);
    assert_eq!(
        FRAMES_PER_SECTOR * u64::from(SECTORS_PER_SECOND),
        u64::from(SAMPLE_RATE)
    );
    assert_eq!(
        image.tracks(),
        &[
            CdTrack {
                number: 1,
                audio: false,
                start_sector: 0,
                end_sector: 1
            },
            CdTrack {
                number: 2,
                audio: true,
                start_sector: 1,
                end_sector: 3
            },
            CdTrack {
                number: 3,
                audio: true,
                start_sector: 3,
                end_sector: 4
            },
        ]
    );
    assert_eq!(image.track(2).unwrap().frames(), 1176);
    assert_eq!(
        image.track(2).unwrap().duration(),
        Duration::new(0, 26_666_666)
    );
    assert_eq!(frames_duration(44_100), Duration::from_secs(1));
    assert_eq!(
        frames_duration(44_100 * 123 + 1),
        Duration::new(123, 22_675)
    );
    for number in [0, 4, 255] {
        assert!(image.track(number).is_err());
    }
    // Read across a sector and an adjacent audio track, with a non-sector offset.
    let mut got = vec![[0, 0]; 700];
    image.read_frames(588 + 500, &mut got).unwrap();
    assert_eq!(got, fixture.frames[500..1200]);
    // Last frame belongs to track 3, not an invented sentinel or pregap.
    let mut last = [[0, 0]];
    image.read_frames(4 * 588 - 1, &mut last).unwrap();
    assert_eq!(last[0], *fixture.frames.last().unwrap());
}

#[test]
fn rejects_data_overflow_leadout_and_truncated_reads_without_output_changes() {
    let fixture = Fixture::new();
    let mut image = fixture.image();
    let mut got = [[71, -71]; 2];
    for start in [0, 587, 4 * 588 - 1, 4 * 588, u64::MAX] {
        assert!(image.read_frames(start, &mut got).is_err());
        assert_eq!(got, [[71, -71]; 2]);
    }
    image.read_frames(4 * 588, &mut []).unwrap();
    // Mutate only this authored fixture after mount to exercise real I/O failure.
    File::options()
        .write(true)
        .open(&fixture.bin)
        .unwrap()
        .set_len(SECTOR_BYTES as u64 + 2)
        .unwrap();
    assert_eq!(
        image.read_frames(588, &mut got).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(got, [[71, -71]; 2]);
}

#[test]
fn strict_cue_parser_rejects_unsupported_and_ambiguous_formats() {
    let fixture = Fixture::new();
    let valid = Fixture::cue_text();
    let invalid_cues = [
        valid.replace("BINARY", "WAVE"),
        valid.replace("fixture.bin", "other.bin"),
        valid.replace("MODE1/2352", "MODE1/2048"),
        valid.replace("TRACK 02", "TRACK 04"),
        valid.replace("00:00:01", "00:60:01"),
        valid.replace("00:00:01", "00:00:75"),
        valid.replace("00:00:01", "00:0:01"),
        valid.replace("00:00:01", "+0:00:01"),
        valid.replace("00:00:01", "00:00:00"),
        valid.replace("00:00:03", "00:00:04"),
        valid.replace("INDEX 01 00:00:03", ""),
        valid.replace("INDEX 01 00:00:03", "INDEX 00 00:00:03"),
        valid.replace("TRACK 02 AUDIO", "TRACK 02 AUDIO\nFLAGS PRE"),
        valid.replace("TRACK 02 AUDIO", "PREGAP 00:02:00\nTRACK 02 AUDIO"),
        format!("{valid}FILE \"fixture.bin\" BINARY\n"),
        format!("{valid}INDEX 01 00:00:03\n"),
        "".into(),
        format!("REM {}", "x".repeat(65_536)),
    ];
    for cue in invalid_cues {
        fs::write(&fixture.cue, &cue).unwrap();
        assert!(
            CdAudioImage::mount_bin_cue(&fixture.bin, &fixture.cue).is_err(),
            "accepted malformed cue: {cue:.200}"
        );
    }
    fs::write(&fixture.cue, valid.replace('\n', "\r\n")).unwrap();
    assert!(CdAudioImage::mount_bin_cue(&fixture.bin, &fixture.cue).is_ok());
    fs::write(&fixture.bin, [0; 2351]).unwrap();
    assert!(CdAudioImage::mount_bin_cue(&fixture.bin, &fixture.cue).is_err());
    fs::write(&fixture.bin, []).unwrap();
    assert!(CdAudioImage::mount_bin_cue(&fixture.bin, &fixture.cue).is_err());
}

#[test]
fn queue_pause_resume_stop_underrun_and_restart_have_precise_positions() {
    let fixture = Fixture::new();
    let mut player = fixture.player(17);
    assert_eq!(player.state(), PlaybackState::Stopped);
    assert_eq!(player.image().tracks().len(), 3);
    assert_eq!(player.volume(), StereoVolume::default());
    assert_eq!(player.prefetch().unwrap(), 0);
    player.play_track(2).unwrap();
    assert_eq!(player.duration_frames(), 1176);
    assert_eq!(player.duration(), Duration::new(0, 26_666_666));
    let mut output = [[42, 42]; 20];
    // Empty queue: silence, not fabricated advancement.
    assert_eq!(player.render_into(&mut output), 0);
    assert_eq!(player.position_frames(), 0);
    assert_eq!(output, [[0, 0]; 20]);
    assert_eq!(player.prefetch().unwrap(), 17);
    assert_eq!(player.prefetch().unwrap(), 0);
    assert_eq!(player.position_frames(), 0);
    player.pause();
    player.pause();
    assert_eq!(player.state(), PlaybackState::Paused);
    assert_eq!(player.render_into(&mut output), 0);
    assert_eq!(player.queued_frames(), 17);
    assert_eq!(player.position_frames(), 0);
    player.resume();
    player.resume();
    assert_eq!(player.render_into(&mut output), 17);
    assert_eq!(&output[..17], &fixture.frames[..17]);
    assert_eq!(&output[17..], &[[0, 0]; 3]);
    assert_eq!(player.position_frames(), 17);
    assert_eq!(player.absolute_frame(), 588 + 17);
    assert_eq!(player.position_duration(), frames_duration(17));
    assert_eq!(player.prefetch().unwrap(), 17);
    // Rejected commands must not destroy an existing selection or queue.
    for (start, count) in [(0, 1), (0, 2), (1, 0), (3, 2), (u64::MAX, 1), (1, u64::MAX)] {
        assert!(player.play_sectors(start, count).is_err());
        assert_eq!(player.position_frames(), 17);
        assert_eq!(player.queued_frames(), 17);
    }
    assert!(player.play_track(1).is_err());
    assert!(player.play_track(0).is_err());
    player.play_track(3).unwrap();
    assert_eq!(player.queued_frames(), 0);
    assert_eq!(player.position_frames(), 0);
    assert_eq!(player.absolute_frame(), 3 * 588);
    player.prefetch().unwrap();
    assert_eq!(player.render_into(&mut output), 17);
    assert_eq!(&output[..17], &fixture.frames[1176..1193]);
    player.stop();
    player.resume();
    assert_eq!(player.state(), PlaybackState::Stopped);
    assert_eq!(player.queued_frames(), 0);
    assert_eq!(player.duration_frames(), 0);
    assert_eq!(player.render_into(&mut output), 0);
    assert_eq!(output, [[0, 0]; 20]);
}

#[test]
fn block_size_independent_queue_exhaustion_has_no_skips_duplicates_or_padding() {
    let fixture = Fixture::new();
    for (capacity, pull) in [(1, 1), (13, 7), (100, 157), (588, 588), (2000, 1767)] {
        let mut player = fixture.player(capacity);
        player.play_sectors(1, 3).unwrap();
        let mut actual = Vec::new();
        while player.state() != PlaybackState::Finished {
            player.prefetch().unwrap();
            assert!(player.queued_frames() <= capacity);
            let mut output = vec![[9, 9]; pull];
            let consumed = player.render_into(&mut output);
            assert!(consumed > 0);
            actual.extend_from_slice(&output[..consumed]);
            assert!(output[consumed..].iter().all(|f| *f == [0, 0]));
        }
        assert_eq!(actual, fixture.frames);
        assert_eq!(player.position_frames(), 1764);
        assert_eq!(player.duration_frames(), 1764);
        assert_eq!(player.absolute_frame(), 2352);
        assert_eq!(player.queued_frames(), 0);
        player.pause();
        player.resume();
        assert_eq!(player.state(), PlaybackState::Finished);
        assert_eq!(player.prefetch().unwrap(), 0);
        assert_eq!(player.render_into(&mut [[5, 5]; 1]), 0);
    }
}

#[test]
fn stereo_volume_saturating_mix_and_interleaved_output_are_exact() {
    let fixture = Fixture::new();
    let mut player = fixture.player(8);
    player.play_track(2).unwrap();
    player.prefetch().unwrap();
    let mut mixed = [[-100, 100], [30_000, -30_000]];
    assert_eq!(player.mix_into(&mut mixed), 2);
    assert_eq!(mixed, [[i16::MIN, i16::MAX], [i16::MAX, i16::MIN]]);
    player.play_track(2).unwrap();
    player.prefetch().unwrap();
    player.set_volume(StereoVolume {
        left: 128,
        right: 0,
    });
    assert_eq!(
        player.volume(),
        StereoVolume {
            left: 128,
            right: 0
        }
    );
    let mut output = [[0, 9]; 3];
    assert_eq!(player.mix_into(&mut output), 3);
    assert_eq!(output, [[-16448, 9], [2339, 9], [0, 9]]);
    player.play_track(2).unwrap();
    player.prefetch().unwrap();
    player.set_volume(StereoVolume::default());
    let before = player.queued_frames();
    let mut odd = [9; 3];
    assert!(player.render_interleaved(&mut odd).is_err());
    assert_eq!(odd, [9; 3]);
    assert_eq!(player.position_frames(), 0);
    assert_eq!(player.queued_frames(), before);
    let mut flat = [9; 20];
    assert_eq!(player.render_interleaved(&mut flat).unwrap(), 8);
    let expected: Vec<i16> = fixture.frames[..8].iter().flatten().copied().collect();
    assert_eq!(&flat[..16], &expected);
    assert_eq!(&flat[16..], &[0; 4]);
    player.pause();
    let mut mixed = [[12, -13]; 2];
    assert_eq!(player.mix_into(&mut mixed), 0);
    assert_eq!(mixed, [[12, -13]; 2]);
}

#[test]
fn prefetch_errors_do_not_queue_partial_audio_or_advance_playback() {
    let fixture = Fixture::new();
    assert!(CdPlayer::new(fixture.image(), 0).is_err());
    assert!(CdPlayer::new(fixture.image(), 441_001).is_err());
    let mut player = fixture.player(10);
    player.play_track(2).unwrap();
    player.prefetch().unwrap();
    assert_eq!(player.render_into(&mut [[0, 0]; 5]), 5);
    File::options()
        .write(true)
        .open(&fixture.bin)
        .unwrap()
        .set_len(2352 + 4 * 12)
        .unwrap(); // next refill reads two frames, then fails before all five
    assert!(player.prefetch().is_err());
    assert_eq!(player.position_frames(), 5);
    assert_eq!(player.queued_frames(), 5);
    assert_eq!(player.state(), PlaybackState::Playing);
    let mut got = [[0, 0]; 5];
    assert_eq!(player.render_into(&mut got), 5);
    assert_eq!(got, fixture.frames[5..10]);
    // Restore only the authored fixture, then reuse staging after a partial
    // read failure. No stale prefix/tail may leak into the published queue.
    let mut restored = vec![0x5a; SECTOR_BYTES];
    for frame in &fixture.frames {
        for sample in frame {
            restored.extend(sample.to_le_bytes());
        }
    }
    fs::write(&fixture.bin, restored).unwrap();
    assert_eq!(player.prefetch().unwrap(), 10);
    let mut recovered = [[42, 42]; 10];
    assert_eq!(player.render_into(&mut recovered), 10);
    assert_eq!(recovered, fixture.frames[10..20]);
    assert_eq!(player.position_frames(), 20);
}

/// Opt-in real-asset read, never synthesized music and never host output.
/// Run: cargo test --test audio_pipeline real_gc050 -- --ignored --nocapture
#[test]
#[ignore = "requires local copyrighted assets/GC050.BIN; read-only and no audio device"]
fn real_gc050_tracks_round_trip_raw_bytes_and_are_nonzero() {
    let assets = std::env::var_os("PAL_CD_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("assets").to_path_buf());
    let bin = assets.join("GC050.BIN");
    assert_eq!(fs::metadata(&bin).unwrap().len(), 237_206_256);
    let mut image = CdAudioImage::mount_bin_cue(&bin, &assets.join("GC050.cue")).unwrap();
    assert_eq!(image.sectors(), 100_853);
    assert_eq!(image.tracks().len(), 9);
    assert_eq!(image.tracks().iter().filter(|t| t.audio).count(), 8);
    let mut independent = File::open(&bin).unwrap();
    for track in image.tracks().to_vec().into_iter().filter(|t| t.audio) {
        let mut position = track.start_sector * FRAMES_PER_SECTOR;
        let end = track.end_sector * FRAMES_PER_SECTOR;
        let mut minimum = i16::MAX;
        let mut maximum = i16::MIN;
        let mut nonzero = 0u64;
        while position < end {
            let count = (end - position).min(4096) as usize;
            let mut frames = vec![[0, 0]; count];
            image.read_frames(position, &mut frames).unwrap();
            let mut reference = vec![0; count * 4];
            independent.seek(SeekFrom::Start(position * 4)).unwrap();
            independent.read_exact(&mut reference).unwrap();
            let mut round_trip = Vec::with_capacity(count * 4);
            for frame in frames {
                for sample in frame {
                    round_trip.extend(sample.to_le_bytes());
                    minimum = minimum.min(sample);
                    maximum = maximum.max(sample);
                    nonzero += u64::from(sample != 0);
                }
            }
            assert_eq!(round_trip, reference);
            position += count as u64;
        }
        assert!(
            nonzero > 0,
            "real track {} unexpectedly silent",
            track.number
        );
        assert!(minimum < 0 && maximum > 0);
        let expected_stats = [
            (-26571, 26076, 13811642),
            (-27422, 32767, 11866727),
            (-32768, 30895, 12020560),
            (-25032, 26539, 13503896),
            (-23059, 25360, 12721568),
            (-28729, 29962, 12185416),
            (-32080, 32213, 9656521),
            (-30071, 32089, 15279231),
        ];
        assert_eq!(
            (minimum, maximum, nonzero),
            expected_stats[track.number as usize - 2]
        );
        // Exercise the actual output queue too, against independent raw bytes.
        let mut player = CdPlayer::new(
            CdAudioImage::mount_bin_cue(&bin, &assets.join("GC050.cue")).unwrap(),
            8192,
        )
        .unwrap();
        player.play_track(track.number).unwrap();
        assert_eq!(player.duration_frames(), track.frames());
        assert_eq!(player.prefetch().unwrap(), 8192);
        let mut queued = vec![[0, 0]; 997];
        assert_eq!(player.render_into(&mut queued), 997);
        let mut expected = vec![0; 997 * 4];
        independent
            .seek(SeekFrom::Start(track.start_sector * SECTOR_BYTES as u64))
            .unwrap();
        independent.read_exact(&mut expected).unwrap();
        let queue_bytes: Vec<u8> = queued
            .into_iter()
            .flatten()
            .flat_map(i16::to_le_bytes)
            .collect();
        assert_eq!(queue_bytes, expected);
        assert_eq!(player.position_frames(), 997);
        println!(
            "track={} sectors={}..{} pcm_frames={} duration={:?} min={} max={} nonzero_samples={}",
            track.number,
            track.start_sector,
            track.end_sector,
            track.frames(),
            track.duration(),
            minimum,
            maximum,
            nonzero
        );
    }
}
