// SPDX-License-Identifier: GPL-3.0-or-later
//! MSCDEX protocol regression: RBIL 02597/02596/01449/01450 and the
//! MSCDEX specification's STOP/RESUME state machine. IOCTL 0F uses an end
//! address (not count), canonical F/S/M/0; DOSBox Staging's M/S/F/0 quirk
//! is deliberately not an oracle for that field. No host audio output.
use pal_wayland::{
    audio::{FRAMES_PER_SECTOR, PlaybackState, SECTOR_BYTES, StereoFrame},
    cdrom::CdRom,
    runtime::Machine,
    semantics::{A20, GuestMemory, SegmentedAddress},
};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

const AT: SegmentedAddress = SegmentedAddress::new(0, 0x100);
const DST: SegmentedAddress = SegmentedAddress::new(0, 0x1000);
const AUDIO_START: u32 = 18;
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    dir: PathBuf,
    bin: PathBuf,
    cue: PathBuf,
    iso: PathBuf,
    frames: Vec<StereoFrame>,
}
impl Fixture {
    fn new() -> Self {
        let dir = PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required for fixtures"))
            .join(format!(
                "fixture-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("disc.bin");
        let cue = dir.join("disc.cue");
        let iso = dir.join("disc.iso");
        let mut cooked = vec![0; 18 * 2048];
        cooked[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        cooked[17 * 2048] = 42;
        fs::write(&iso, &cooked).unwrap();
        let mut raw = Vec::new();
        for data in cooked.chunks_exact(2048) {
            let mut sector = [0; SECTOR_BYTES];
            sector[15] = 1;
            sector[16..2064].copy_from_slice(data);
            raw.extend(sector);
        }
        let frames: Vec<StereoFrame> = (0..3 * FRAMES_PER_SECTOR as usize)
            .map(|i| [i as i16 * 17 - 15000, 15000 - i as i16 * 17])
            .collect();
        for frame in &frames {
            for sample in frame {
                raw.extend(sample.to_le_bytes());
            }
        }
        fs::write(&bin, raw).unwrap();
        fs::write(&cue, "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\nTRACK 03 AUDIO\nINDEX 01 00:00:20\n").unwrap();
        Self {
            dir,
            bin,
            cue,
            iso,
            frames,
        }
    }
    fn cd(&self) -> CdRom {
        CdRom::mount_bin_cue(&self.bin, &self.cue).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn memory() -> GuestMemory {
    GuestMemory::new(0x10000, A20::Enabled)
}
fn dword(m: &mut GuestMemory, at: SegmentedAddress, n: u32) {
    m.write_bytes(at, &n.to_le_bytes()).unwrap();
}
fn redbook(sector: u32) -> [u8; 4] {
    let n = sector + 150;
    [(n % 75) as u8, ((n / 75) % 60) as u8, (n / 4500) as u8, 0]
}
fn command(cd: &mut CdRom, m: &mut GuestMemory, cmd: u8) -> u16 {
    m.write8(AT.wrapping_add(2), cmd).unwrap();
    cd.request(m, AT).unwrap();
    assert_eq!(m.read8(AT.wrapping_add(1)).unwrap(), 0);
    m.read16(AT.wrapping_add(3)).unwrap()
}
fn play(cd: &mut CdRom, m: &mut GuestMemory, mode: u8, start: u32, count: u32) -> u16 {
    m.write8(AT, 22).unwrap();
    m.write8(AT.wrapping_add(13), mode).unwrap();
    dword(m, AT.wrapping_add(14), start);
    dword(m, AT.wrapping_add(18), count);
    command(cd, m, 0x84)
}
fn ioctl(cd: &mut CdRom, m: &mut GuestMemory, function: u8, count: u16) -> (u16, Vec<u8>) {
    m.write8(AT, 20).unwrap();
    m.write16(AT.wrapping_add(14), DST.offset).unwrap();
    m.write16(AT.wrapping_add(16), DST.segment).unwrap();
    m.write16(AT.wrapping_add(18), count).unwrap();
    m.write8(DST, function).unwrap();
    let status = command(cd, m, 3);
    let len = m.read16(AT.wrapping_add(18)).unwrap() as usize;
    let mut bytes = vec![0; len];
    m.read_bytes(DST, &mut bytes).unwrap();
    (status, bytes)
}
fn assert_status(
    cd: &mut CdRom,
    m: &mut GuestMemory,
    paused: bool,
    start: u32,
    end: u32,
    busy: bool,
) {
    let (status, bytes) = ioctl(cd, m, 15, 0); // actual PAL count=0 compatibility
    let mut expected = vec![15];
    expected.extend(u16::from(paused).to_le_bytes());
    if end == 0 {
        expected.extend([0; 8]);
    } else {
        expected.extend(redbook(start));
        expected.extend(redbook(end));
    }
    assert_eq!(status, if busy { 0x300 } else { 0x100 });
    assert_eq!(bytes, expected);
}

#[test]
fn hsg_pause_resume_stop_and_completion_follow_consumed_pcm() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    let mut m = memory();
    assert_eq!(
        ioctl(&mut cd, &mut m, 6, 0),
        (0x100, vec![6, 0x10, 2, 0, 0])
    );
    assert_eq!(command(&mut cd, &mut m, 0x88), 0x8108);
    assert_eq!(play(&mut cd, &mut m, 0, AUDIO_START, 3), 0x300);
    assert_status(&mut cd, &mut m, false, 18, 21, true);
    assert_eq!(
        ioctl(&mut cd, &mut m, 6, 5),
        (0x300, vec![6, 0x10, 6, 0, 0])
    );
    let mut output = [[99, 99]; 997];
    assert_eq!(cd.render_audio(&mut output), 0); // underrun never advances
    assert_eq!(output, [[0, 0]; 997]);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(cd.prefetch_audio().unwrap(), fixture.frames.len());
    assert_status(&mut cd, &mut m, false, 18, 21, true); // polling does not advance
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(cd.render_audio(&mut output), 997);
    assert_eq!(output.as_slice(), &fixture.frames[..997]);
    assert_eq!(command(&mut cd, &mut m, 0x85), 0x100);
    assert_status(&mut cd, &mut m, true, 19, 21, false);
    assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Paused);
    let remaining = cd.audio_player().unwrap().queued_frames();
    assert_eq!(cd.prefetch_audio().unwrap(), 0);
    assert_eq!(cd.render_audio(&mut output), 0);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 997);
    assert_eq!(cd.audio_player().unwrap().queued_frames(), remaining);
    assert_eq!(command(&mut cd, &mut m, 0x88), 0x300);
    assert_status(&mut cd, &mut m, false, 19, 21, true);
    assert_eq!(cd.render_audio(&mut output), remaining);
    assert_eq!(&output[..remaining], &fixture.frames[997..]);
    assert!(output[remaining..].iter().all(|f| *f == [0, 0]));
    assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Finished);
    assert_status(&mut cd, &mut m, false, 0, 0, false);
    assert_eq!(
        ioctl(&mut cd, &mut m, 6, 0),
        (0x100, vec![6, 0x10, 2, 0, 0])
    );
    assert_eq!(command(&mut cd, &mut m, 0x88), 0x8108);
    assert_eq!(play(&mut cd, &mut m, 0, 18, 1), 0x300);
    assert_eq!(cd.prefetch_audio().unwrap(), 588);
    assert_eq!(command(&mut cd, &mut m, 0x85), 0x100);
    assert_eq!(command(&mut cd, &mut m, 0x85), 0x100); // second STOP is final
    assert_eq!(cd.audio_player().unwrap().queued_frames(), 0);
    assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Stopped);
    assert_eq!(command(&mut cd, &mut m, 0x88), 0x8108);
}

#[test]
fn redbook_and_invalid_ranges_are_atomic_and_busy_errors_stay_busy() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    let mut m = memory();
    assert_eq!(
        play(&mut cd, &mut m, 1, u32::from_le_bytes(redbook(18)), 3),
        0x300
    );
    cd.prefetch_audio().unwrap();
    let mut head = [[0, 0]; 5];
    assert_eq!(cd.render_audio(&mut head), 5);
    assert_eq!(head.as_slice(), &fixture.frames[..5]);
    let queued = cd.audio_player().unwrap().queued_frames();
    for (mode, start, count) in [
        (0, 0, 1),
        (0, 17, 2),
        (0, 18, 0),
        (0, 20, 2),
        (0, u32::MAX, 1),
        (0, 18, 65536),
        (0, 18, u32::MAX),
        (2, 18, 1),
        (1, 0, 1),
        (1, 0x00023c00, 1),
        (1, 0x0002024b, 1),
        (1, 0x01000200, 1),
    ] {
        assert_eq!(play(&mut cd, &mut m, mode, start, count), 0x8308);
        assert_eq!(cd.audio_player().unwrap().position_frames(), 5);
        assert_eq!(cd.audio_player().unwrap().queued_frames(), queued);
        assert_status(&mut cd, &mut m, false, 18, 21, true);
    }
    // Replacement selection clears queued samples from the previous range.
    assert_eq!(play(&mut cd, &mut m, 0, 20, 1), 0x300);
    assert_eq!(cd.audio_player().unwrap().queued_frames(), 0);
    cd.prefetch_audio().unwrap();
    assert_eq!(cd.render_audio(&mut head), 5);
    assert_eq!(head.as_slice(), &fixture.frames[1176..1181]);
}

#[test]
fn legacy_fixed13_is_explicit_and_still_checks_real_memory() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    let mut m = memory();
    ioctl(&mut cd, &mut m, 10, 7);
    m.write8(AT, 13).unwrap();
    assert_eq!(command(&mut cd, &mut m, 3), 0x8105);
    cd.enable_legacy_header13();
    assert_eq!(command(&mut cd, &mut m, 3), 0x100);
    assert_eq!(m.read8(DST.wrapping_add(2)).unwrap(), 3);
    play(&mut cd, &mut m, 0, AUDIO_START, 1);
    m.write8(AT, 13).unwrap();
    assert_eq!(command(&mut cd, &mut m, 0x84), 0x300);
    let edge = SegmentedAddress::new(0, 0xfffb);
    m.write8(edge, 13).unwrap();
    m.write8(edge.wrapping_add(2), 0x84).unwrap();
    assert!(cd.request(&mut m, edge).is_err());
}
#[test]
fn short_control_buffers_and_headers_do_not_touch_payload_or_state() {
    let fixture = Fixture::new();
    let mut cd = fixture.cd();
    let mut m = memory();
    assert_eq!(play(&mut cd, &mut m, 0, 18, 1), 0x300);
    for (function, required) in [(6, 5), (7, 4), (8, 5), (9, 2), (10, 7), (11, 7), (15, 11)] {
        m.write8(AT, 20).unwrap();
        m.write16(AT.wrapping_add(14), 0xffff).unwrap();
        m.write16(AT.wrapping_add(16), 0).unwrap();
        m.write8(SegmentedAddress::new(0, 0xffff), function)
            .unwrap();
        for count in 1..required {
            m.write16(AT.wrapping_add(18), count).unwrap();
            assert_eq!(command(&mut cd, &mut m, 3), 0x8305);
            assert_eq!(m.read8(SegmentedAddress::new(0, 0xffff)).unwrap(), function);
            assert_eq!(m.read16(AT.wrapping_add(18)).unwrap(), count);
        }
    }
    // Five valid header bytes near memory end; declared 13-byte PLAY header
    // must be rejected before reading missing mode/start/count fields.
    let edge = SegmentedAddress::new(0, 0xfffb);
    m.write8(edge, 13).unwrap();
    m.write8(edge.wrapping_add(2), 0x84).unwrap();
    cd.request(&mut m, edge).unwrap();
    assert_eq!(m.read16(edge.wrapping_add(3)).unwrap(), 0x8305);
    assert_eq!(cd.audio_player().unwrap().position_frames(), 0);
    assert_eq!(cd.audio_player().unwrap().duration_frames(), 588);
}

#[test]
fn play_full_header_cannot_wrap_or_leave_playing_after_status_fault() {
    let fixture = Fixture::new();
    for offset in [0xfff2, 0xfffc] {
        let mut cd = fixture.cd();
        let mut m = memory();
        let at = SegmentedAddress::new(0, offset);
        m.write8(at, 22).unwrap();
        m.write8(at.wrapping_add(2), 0x84).unwrap();
        m.write8(at.wrapping_add(13), 0).unwrap();
        dword(&mut m, at.wrapping_add(14), AUDIO_START);
        dword(&mut m, at.wrapping_add(18), 1);
        let before = m.clone();
        assert!(cd.request(&mut m, at).is_err());
        assert_eq!(cd.audio_player().unwrap().state(), PlaybackState::Stopped);
        assert_eq!(m, before);
    }
}
#[test]
fn iso_mount_remains_data_only_even_with_audio_toc_elsewhere() {
    let fixture = Fixture::new();
    let mut cd = CdRom::mount(&fixture.iso).unwrap();
    let mut m = memory();
    assert!(cd.audio_player().is_none());
    assert_eq!(ioctl(&mut cd, &mut m, 6, 0), (0x100, vec![6, 0, 2, 0, 0]));
    assert_status(&mut cd, &mut m, false, 0, 0, false);
    assert_eq!(play(&mut cd, &mut m, 0, 18, 1), 0x8108);
    assert_eq!(command(&mut cd, &mut m, 0x85), 0x100);
    assert_eq!(command(&mut cd, &mut m, 0x88), 0x8108);
    let mut pcm = [[1, 2]; 10];
    assert_eq!(cd.prefetch_audio().unwrap(), 0);
    assert_eq!(cd.render_audio(&mut pcm), 0);
    assert_eq!(pcm, [[0, 0]; 10]);
    m.write8(AT, 25).unwrap();
    m.write8(AT.wrapping_add(13), 0).unwrap();
    m.write16(AT.wrapping_add(14), DST.offset).unwrap();
    m.write16(AT.wrapping_add(16), 0).unwrap();
    m.write16(AT.wrapping_add(18), 1).unwrap();
    dword(&mut m, AT.wrapping_add(20), 17);
    m.write8(AT.wrapping_add(24), 0).unwrap();
    assert_eq!(command(&mut cd, &mut m, 0x80), 0x100);
    assert_eq!(m.read8(DST).unwrap(), 42);
}

#[test]
fn actual_int2f_1510_reaches_audio_service() {
    let fixture = Fixture::new();
    let mut mz = vec![0; 64];
    for (offset, value) in [
        (0, 0x5a4d_u16),
        (2, 64),
        (4, 1),
        (8, 2),
        (10, 0x10),
        (12, 0xffff),
        (16, 0x100),
        (24, 0x1c),
    ] {
        mz[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    let mut machine = Machine::from_mz(&mz, 0x1010).unwrap();
    machine.cdrom = Some(fixture.cd());
    machine.memory.write8(AT, 22).unwrap();
    machine.memory.write8(AT.wrapping_add(2), 0x84).unwrap();
    dword(&mut machine.memory, AT.wrapping_add(14), 18);
    dword(&mut machine.memory, AT.wrapping_add(18), 1);
    machine.regs.ax = 0x1510;
    machine.regs.cx = 3;
    machine.regs.es = 0;
    machine.regs.bx = AT.offset;
    machine.interrupt(0x2f).unwrap();
    assert_eq!(machine.memory.read16(AT.wrapping_add(3)).unwrap(), 0x300);
    let cd = machine.cdrom.as_mut().unwrap();
    assert_eq!(cd.prefetch_audio().unwrap(), 588);
    let mut pcm = [[0, 0]; 5];
    assert_eq!(cd.render_audio(&mut pcm), 5);
    assert_eq!(pcm.as_slice(), &fixture.frames[..5]);

    // Every video mode must reject an incomplete fixed prefix before any
    // wrapped command read or unit/status store, even for a short declaration.
    for mode in [0, 3, 0x12, 0x13] {
        machine.video_mode = mode;
        for offset in [0xfffc, 0xfffe] {
            let at = SegmentedAddress::new(0, offset);
            machine.memory.write8(at, 1).unwrap();
            machine.memory.write8(at.wrapping_add(1), 0xee).unwrap();
            machine.memory.write8(at.wrapping_add(2), 0x85).unwrap();
            machine.regs.ax = 0x1510;
            machine.regs.bx = offset;
            let memory = machine.memory.clone();
            let regs = machine.regs;
            let planar = machine.planar.borrow().clone();
            let cd = format!("{:?}", machine.cdrom);
            assert!(machine.interrupt(0x2f).is_err(), "mode {mode:02x}, {at}");
            assert_eq!(machine.memory, memory);
            assert_eq!(machine.regs, regs);
            assert_eq!(*machine.planar.borrow(), planar);
            assert_eq!(format!("{:?}", machine.cdrom), cd);
        }
        // A complete five-byte prefix may report a protocol error without
        // requiring the command's full header or changing the playing state.
        let at = SegmentedAddress::new(0, 0xfffb);
        machine
            .memory
            .write_bytes(at, &[1, 0xee, 0x85, 0x5a, 0xa5])
            .unwrap();
        machine.regs.ax = 0x1510;
        machine.regs.bx = at.offset;
        machine.interrupt(0x2f).unwrap();
        assert_eq!(machine.memory.read8(at.wrapping_add(1)).unwrap(), 0);
        assert_eq!(machine.memory.read16(at.wrapping_add(3)).unwrap(), 0x8305);
        assert_eq!(
            machine
                .cdrom
                .as_ref()
                .unwrap()
                .audio_player()
                .unwrap()
                .position_frames(),
            5
        );
    }
    // A nonwrapping prefix can still exceed physical RAM; no partial stores.
    machine.memory = GuestMemory::new(0xfff4, A20::Enabled);
    let at = SegmentedAddress::new(0, 0xfff0);
    machine
        .memory
        .write_bytes(at, &[1, 0xee, 0x85, 0x5a])
        .unwrap();
    machine.regs.ax = 0x1510;
    machine.regs.bx = at.offset;
    let memory = machine.memory.clone();
    let regs = machine.regs;
    let planar = machine.planar.borrow().clone();
    let cd = format!("{:?}", machine.cdrom);
    assert!(machine.interrupt(0x2f).is_err());
    assert_eq!(machine.memory, memory);
    assert_eq!(machine.regs, regs);
    assert_eq!(*machine.planar.borrow(), planar);
    assert_eq!(format!("{:?}", machine.cdrom), cd);
}

/// Read-only copyrighted asset acceptance. Tests all eight tracks end-to-end
/// through MSCDEX requests and the actual PCM queue, never an audio device.
#[test]
#[ignore = "requires local GC050 BIN/CUE; read-only and no host audio"]
fn real_gc050_all_audio_tracks_via_mscdex_match_independent_raw_bytes() {
    let assets = std::env::var_os("PAL_CD_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"));
    let bin = assets.join("GC050.BIN");
    assert_eq!(fs::metadata(&bin).unwrap().len(), 237_206_256);
    let mut cd = CdRom::mount_bin_cue(&bin, &assets.join("GC050.cue")).unwrap();
    let tracks = cd.audio_player().unwrap().image().tracks().to_vec();
    assert_eq!(tracks.len(), 9);
    assert_eq!(tracks.iter().filter(|t| t.audio).count(), 8);
    let mut raw = File::open(&bin).unwrap();
    let mut m = memory();
    let mut pcm = vec![[0, 0]; 4096];
    for track in tracks.into_iter().filter(|t| t.audio) {
        let start = track.start_sector as u32;
        let count = track.sectors() as u32;
        let end = track.end_sector as u32;
        // Verify Red Book decoding first, then stream the full HSG selection.
        assert_eq!(
            play(
                &mut cd,
                &mut m,
                1,
                u32::from_le_bytes(redbook(start)),
                count
            ),
            0x300
        );
        assert_status(&mut cd, &mut m, false, start, end, true);
        assert_eq!(play(&mut cd, &mut m, 0, start, count), 0x300);
        raw.seek(SeekFrom::Start(track.start_sector * SECTOR_BYTES as u64))
            .unwrap();
        let mut consumed = 0u64;
        let mut nonzero = 0u64;
        let mut paused_once = false;
        while consumed < track.frames() {
            cd.prefetch_audio().unwrap();
            if !paused_once && consumed > FRAMES_PER_SECTOR {
                assert_eq!(command(&mut cd, &mut m, 0x85), 0x100);
                assert_status(
                    &mut cd,
                    &mut m,
                    true,
                    start + (consumed / FRAMES_PER_SECTOR) as u32,
                    end,
                    false,
                );
                assert_eq!(cd.render_audio(&mut pcm), 0);
                assert_eq!(cd.audio_player().unwrap().position_frames(), consumed);
                assert_eq!(command(&mut cd, &mut m, 0x88), 0x300);
                paused_once = true;
            }
            let got = cd.render_audio(&mut pcm);
            assert!(got > 0);
            let mut reference = vec![0; got * 4];
            raw.read_exact(&mut reference).unwrap();
            let bytes: Vec<u8> = pcm[..got]
                .iter()
                .flatten()
                .flat_map(|n| n.to_le_bytes())
                .collect();
            assert_eq!(
                bytes, reference,
                "track {} frame {}",
                track.number, consumed
            );
            nonzero += pcm[..got].iter().flatten().filter(|&&n| n != 0).count() as u64;
            consumed += got as u64;
            assert_eq!(cd.audio_player().unwrap().position_frames(), consumed);
        }
        assert_eq!(consumed, track.frames());
        assert!(nonzero > 0);
        assert_status(&mut cd, &mut m, false, 0, 0, false);
        assert_eq!(command(&mut cd, &mut m, 0x88), 0x8108);
        println!(
            "MSCDEX track={} sectors={}..{} exact_pcm_frames={} nonzero_samples={} paused/resumed=true",
            track.number, start, end, consumed, nonzero
        );
    }
}
