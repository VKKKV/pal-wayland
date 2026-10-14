// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored BIN/CUE only; real INT 2Fh/1510 entry, no GUI/audio devices.
//! RBIL 01449 + MSCDEX specification QInfo: 11 bytes; TNO is BCD,
//! times binary M/S/F, relative unbiased and absolute HSG+150.
//! Sources: https://www.delorie.com/djgpp/doc/rbinter/it/49/14.html
//! https://gist.githubusercontent.com/abrasive/7a615e6dde0c1da962f9930cc63ee43d/raw
//! DOSBox Staging 05e2004729f9b31d33deaa442f4933844ef87d02:
//! src/dos/dos_mscdex.cpp (0Ch layout/BCD TNO), cdrom_image.cpp
//! (relative unbiased, absolute +150, pause/stop preserve position).
use pal_wayland::{cdrom::CdRom, runtime::Machine, semantics::SegmentedAddress as Addr};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

const AT: Addr = Addr::new(0, 0x100);
const DST: Addr = Addr::new(0, 0x2000);
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Disc {
    dir: PathBuf,
}
impl Disc {
    fn new() -> Self {
        let dir =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "qchannel-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&dir).unwrap();
        let mut raw = vec![0; 9210 * 2352];
        for sector in raw[..4529 * 2352].chunks_exact_mut(2352) {
            sector[15] = 1;
        }
        raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
        for frame in raw[4529 * 2352..].chunks_exact_mut(4) {
            frame.copy_from_slice(&[0x34, 0x12, 0x78, 0x56]);
        }
        fs::write(dir.join("disc.bin"), raw).unwrap();
        fs::write(
            dir.join("disc.cue"),
            concat!(
                "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\n",
                "TRACK 02 AUDIO\nINDEX 01 01:00:29\n",
                "TRACK 03 AUDIO\nINDEX 01 02:02:50\n",
                "TRACK 04 AUDIO\nINDEX 01 02:02:51\n",
                "TRACK 05 AUDIO\nINDEX 01 02:02:52\n",
                "TRACK 06 AUDIO\nINDEX 01 02:02:53\n",
                "TRACK 07 AUDIO\nINDEX 01 02:02:54\n",
                "TRACK 08 AUDIO\nINDEX 01 02:02:55\n",
                "TRACK 09 AUDIO\nINDEX 01 02:02:56\n",
                "TRACK 10 AUDIO\nINDEX 01 02:02:57\n"
            ),
        )
        .unwrap();
        Self { dir }
    }
    fn machine(&self) -> Machine {
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
        let mut m = Machine::from_mz(&mz, 0x1010).unwrap();
        m.cdrom = Some(
            CdRom::mount_bin_cue(&self.dir.join("disc.bin"), &self.dir.join("disc.cue")).unwrap(),
        );
        m
    }
}
impl Drop for Disc {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn header(m: &mut Machine, command: u8, length: u8) {
    let mut bytes = [0; 25];
    bytes[0] = length;
    bytes[1] = 0xee;
    bytes[2] = command;
    bytes[3..5].copy_from_slice(&0xa55au16.to_le_bytes());
    m.memory.write_bytes(AT, &bytes).unwrap();
}
fn call(m: &mut Machine) -> Result<u16, pal_wayland::runtime::Fault> {
    m.regs.ax = 0x1510;
    m.regs.cx = 3;
    m.regs.es = 0;
    m.regs.bx = AT.offset;
    m.interrupt(0x2f)?;
    Ok(m.memory.read16(AT.wrapping_add(3)).unwrap())
}
fn ioctl(m: &mut Machine, dst: Addr, function: u8, count: u16) {
    header(m, 3, 20);
    m.memory.write16(AT.wrapping_add(14), dst.offset).unwrap();
    m.memory.write16(AT.wrapping_add(16), dst.segment).unwrap();
    m.memory.write16(AT.wrapping_add(18), count).unwrap();
    m.memory.write8(dst, function).unwrap();
}
fn q(m: &mut Machine, status: u16, expected: [u8; 10]) {
    m.memory.write_bytes(DST, &[0xa5; 13]).unwrap();
    ioctl(m, DST, 12, 0); // actual PAL count=0 compatibility
    assert_eq!(call(m).unwrap(), status);
    assert_eq!(m.memory.read8(AT.wrapping_add(1)).unwrap(), 0);
    assert_eq!(m.memory.read16(AT.wrapping_add(18)).unwrap(), 11);
    let bytes = m.memory.read_linear(DST.linear(), 13).unwrap();
    assert_eq!(bytes[0], 12);
    assert_eq!(&bytes[1..11], &expected);
    assert_eq!(&bytes[11..], &[0xa5; 2], "exactly 11 bytes written");
}
fn play(m: &mut Machine, mode: u8, start: u32, count: u32) -> u16 {
    header(m, 0x84, 22);
    m.memory.write8(AT.wrapping_add(13), mode).unwrap();
    m.memory
        .write_bytes(AT.wrapping_add(14), &start.to_le_bytes())
        .unwrap();
    m.memory
        .write_bytes(AT.wrapping_add(18), &count.to_le_bytes())
        .unwrap();
    call(m).unwrap()
}
fn control(m: &mut Machine, command: u8) -> u16 {
    header(m, command, 13);
    call(m).unwrap()
}
fn consume(m: &mut Machine, n: usize) {
    let cd = m.cdrom.as_mut().unwrap();
    cd.prefetch_audio().unwrap();
    let mut output = vec![[0, 0]; n];
    assert_eq!(cd.render_audio(&mut output), n);
    assert!(output.iter().all(|f| *f == [0x1234, 0x5678]));
}
#[test]
fn stationary_mount_and_data_read_report_real_head() {
    let disc = Disc::new();
    let mut m = disc.machine();
    q(&mut m, 0x100, [0x41, 1, 1, 0, 0, 0, 0, 0, 2, 0]);
    header(&mut m, 0x80, 25);
    m.memory.write16(AT.wrapping_add(14), DST.offset).unwrap();
    m.memory.write16(AT.wrapping_add(18), 2).unwrap();
    m.memory
        .write_bytes(AT.wrapping_add(20), &17u32.to_le_bytes())
        .unwrap();
    assert_eq!(call(&mut m).unwrap(), 0x100);
    q(&mut m, 0x100, [0x41, 1, 1, 0, 0, 18, 0, 0, 2, 18]);
    assert_eq!(control(&mut m, 0x85), 0x100);
    q(&mut m, 0x100, [0x41, 1, 1, 0, 0, 18, 0, 0, 2, 18]);
    // A cooked ISO has a valid data head without an audio player too.
    let mut iso = vec![0; 18 * 2048];
    iso[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
    let path = disc.dir.join("disc.iso");
    fs::write(&path, iso).unwrap();
    m.cdrom = Some(CdRom::mount(&path).unwrap());
    q(&mut m, 0x100, [0x41, 1, 1, 0, 0, 0, 0, 0, 2, 0]);
}
#[test]
fn hsg_redbook_and_poll_prefetch_underrun_never_fabricate_progress() {
    let disc = Disc::new();
    let mut m = disc.machine();
    for (mode, start) in [(0, 4529), (1, u32::from_le_bytes([29, 2, 1, 0]))] {
        assert_eq!(play(&mut m, mode, start, 3), 0x300);
        let expected = [1, 2, 1, 0, 0, 0, 0, 1, 2, 29];
        q(&mut m, 0x300, expected);
        assert_eq!(
            m.cdrom.as_mut().unwrap().render_audio(&mut [[9; 2]; 600]),
            0
        );
        q(&mut m, 0x300, expected);
        m.cdrom.as_mut().unwrap().prefetch_audio().unwrap();
        q(&mut m, 0x300, expected);
        consume(&mut m, 587);
        q(&mut m, 0x300, expected);
        consume(&mut m, 1);
        q(&mut m, 0x300, [1, 2, 1, 0, 0, 1, 0, 1, 2, 30]);
    }
    assert_eq!(play(&mut m, 0, 9111, 2), 0x300);
    q(&mut m, 0x300, [1, 2, 1, 1, 1, 7, 0, 2, 3, 36]);
}
#[test]
fn pause_double_stop_resume_and_failed_replacement_preserve_position() {
    let disc = Disc::new();
    let mut m = disc.machine();
    assert_eq!(play(&mut m, 0, 4529, 3), 0x300);
    consume(&mut m, 997);
    let position = [1, 2, 1, 0, 0, 1, 0, 1, 2, 30];
    assert_eq!(control(&mut m, 0x85), 0x100);
    q(&mut m, 0x100, position);
    assert_eq!(
        m.cdrom.as_mut().unwrap().render_audio(&mut [[9; 2]; 999]),
        0
    );
    q(&mut m, 0x100, position);
    assert_eq!(control(&mut m, 0x88), 0x300);
    consume(&mut m, 179); // 997 + 179 = two full sectors; resume retains sub-sector precision
    q(&mut m, 0x300, [1, 2, 1, 0, 0, 2, 0, 1, 2, 31]);
    assert_eq!(play(&mut m, 0, 0, 1), 0x8308);
    q(&mut m, 0x300, [1, 2, 1, 0, 0, 2, 0, 1, 2, 31]);
    assert_eq!(control(&mut m, 0x85), 0x100);
    assert_eq!(control(&mut m, 0x85), 0x100);
    assert_eq!(control(&mut m, 0x88), 0x8108);
    q(&mut m, 0x100, [1, 2, 1, 0, 0, 2, 0, 1, 2, 31]);
}
#[test]
fn track_crossing_bcd_tno_completion_and_leadout() {
    let disc = Disc::new();
    let mut m = disc.machine();
    assert_eq!(play(&mut m, 0, 9199, 2), 0x300);
    consume(&mut m, 588);
    q(&mut m, 0x300, [1, 3, 1, 0, 0, 0, 0, 2, 4, 50]);
    consume(&mut m, 588); // finished selection ends at start of track 4
    q(&mut m, 0x100, [1, 4, 1, 0, 0, 0, 0, 2, 4, 51]);
    assert_eq!(play(&mut m, 0, 9207, 3), 0x300);
    q(&mut m, 0x300, [1, 0x10, 1, 0, 0, 0, 0, 2, 4, 57]);
    consume(&mut m, 1764);
    q(&mut m, 0x100, [1, 0xaa, 1, 0, 0, 0, 0, 2, 4, 60]);
    assert_eq!(control(&mut m, 0x85), 0x100);
    q(&mut m, 0x100, [1, 0xaa, 1, 0, 0, 0, 0, 2, 4, 60]);
}
#[test]
fn all_short_counts_reject_before_nested_span_and_keep_busy() {
    let disc = Disc::new();
    let mut m = disc.machine();
    assert_eq!(play(&mut m, 0, 4529, 3), 0x300);
    consume(&mut m, 5);
    for count in 1..11 {
        ioctl(&mut m, Addr::new(0, 0xffff), 12, count);
        let before = m.memory.clone();
        let cd = format!("{:?}", m.cdrom);
        assert_eq!(call(&mut m).unwrap(), 0x8305);
        let mut expected = before;
        expected.write8(AT.wrapping_add(1), 0).unwrap();
        expected.write16(AT.wrapping_add(3), 0x8305).unwrap();
        assert_eq!(m.memory, expected);
        assert_eq!(format!("{:?}", m.cdrom), cd);
    }
}
#[test]
fn accepted_counts_exact_segment_backing_and_legacy_header_edges() {
    let disc = Disc::new();
    let mut m = disc.machine();
    for count in [0, 11, 65535] {
        ioctl(&mut m, Addr::new(0, 0xfff5), 12, count);
        assert_eq!(call(&mut m).unwrap(), 0x100);
        assert_eq!(m.memory.read16(AT.wrapping_add(18)).unwrap(), 11);
    }
    for dst in [Addr::new(0, 0xfff6), Addr::new(0xffff, 0xffff)] {
        ioctl(&mut m, dst, 12, 0);
        let before = m.memory.clone();
        let cd = format!("{:?}", m.cdrom);
        assert!(call(&mut m).is_err());
        assert_eq!(m.memory, before);
        assert_eq!(format!("{:?}", m.cdrom), cd);
    }
    ioctl(&mut m, DST, 12, 0);
    m.memory.write8(AT, 13).unwrap();
    assert_eq!(call(&mut m).unwrap(), 0x8105);
    m.cdrom.as_mut().unwrap().enable_legacy_header13();
    assert_eq!(call(&mut m).unwrap(), 0x100);
    // Physical RAM edge independently of the 64-KiB segmented edge.
    m.memory = pal_wayland::semantics::GuestMemory::new(
        DST.linear() as usize + 10,
        pal_wayland::semantics::A20::Enabled,
    );
    ioctl(&mut m, DST, 12, 11);
    let before = m.memory.clone();
    let cd = format!("{:?}", m.cdrom);
    assert!(call(&mut m).is_err());
    assert_eq!(m.memory, before);
    assert_eq!(format!("{:?}", m.cdrom), cd);
}
#[test]
fn actual_mode12_preflight_covers_full_qchannel_without_device_effects() {
    let disc = Disc::new();
    let mut m = disc.machine();
    m.video_mode = 0x12;
    m.planar.borrow_mut().write8(0xa0000, 0x65).unwrap();
    m.planar.borrow_mut().read8(0xa0000).unwrap();
    m.planar.borrow_mut().write8(0xa0001, 0x93).unwrap();
    assert_eq!(play(&mut m, 0, 4529, 3), 0x300);
    consume(&mut m, 5);
    for count in [0, 11, 65535] {
        for dst in [
            Addr::new(0xa000, 0),
            Addr::new(0x9fff, 0x10),
            Addr::new(0x9fff, 6),
            Addr::new(0xafff, 0xf),
        ] {
            ioctl(&mut m, dst, 12, count);
            let before = m.memory.clone();
            let planar = m.planar.borrow().clone();
            let cd = format!("{:?}", m.cdrom);
            assert!(call(&mut m).is_err());
            assert_eq!(m.memory, before);
            assert_eq!(*m.planar.borrow(), planar);
            assert_eq!(format!("{:?}", m.cdrom), cd);
        }
    }
    // Only the selector is accessed for short counts, even next to aperture.
    for count in 1..11 {
        ioctl(&mut m, Addr::new(0x9fff, 0xf), 12, count);
        assert_eq!(call(&mut m).unwrap(), 0x8305);
    }
    for dst in [Addr::new(0x9fff, 5), Addr::new(0xb000, 0)] {
        ioctl(&mut m, dst, 12, 65535);
        assert_eq!(call(&mut m).unwrap(), 0x300);
        assert_eq!(m.memory.read16(AT.wrapping_add(18)).unwrap(), 11);
    }
    // A short declaration still needs the complete unit/status prefix in RAM.
    for at in [Addr::new(0x9fff, 0xc), Addr::new(0xa000, 0)] {
        m.memory
            .write_bytes(at, &[1, 0xee, 0x85, 0x5a, 0xa5])
            .unwrap();
        m.regs.ax = 0x1510;
        m.regs.es = at.segment;
        m.regs.bx = at.offset;
        let before = m.memory.clone();
        let regs = m.regs;
        let planar = m.planar.borrow().clone();
        let cd = format!("{:?}", m.cdrom);
        assert!(m.interrupt(0x2f).is_err());
        assert_eq!(m.memory, before);
        assert_eq!(m.regs, regs);
        assert_eq!(*m.planar.borrow(), planar);
        assert_eq!(format!("{:?}", m.cdrom), cd);
    }
    // Mode13 is the original RAM path control, including the VGA aperture.
    m.video_mode = 0x13;
    ioctl(&mut m, Addr::new(0xa000, 0), 12, 11);
    assert_eq!(call(&mut m).unwrap(), 0x300);
}
#[test]
fn subchannel_0d_remains_unsupported_and_atomic() {
    let disc = Disc::new();
    let mut m = disc.machine();
    for mode in [0x12, 0x13] {
        m.video_mode = mode;
        ioctl(&mut m, DST, 13, 0);
        let before = m.memory.clone();
        let cd = format!("{:?}", m.cdrom);
        let error = call(&mut m).unwrap_err();
        assert!(error.0.contains("unsupported CD-ROM ioctl 0d"));
        assert_eq!(m.memory, before);
        assert_eq!(format!("{:?}", m.cdrom), cd);
    }
}
