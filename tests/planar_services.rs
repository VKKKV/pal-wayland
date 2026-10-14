// SPDX-License-Identifier: GPL-3.0-or-later
//! 原创 service span fixtures：仅 TMPDIR 文件，不打开 GUI/音频设备或读取游戏资产。
//! runtime 接线由 mode12 caller 负责；这里验证公开 preflight + 原 request/call。
use pal_wayland::{
    cdrom::CdRom,
    planar_video::PlanarVideo,
    runtime::Fault,
    semantics::{A20, Flags, GuestMemory, Registers, SegmentedAddress as Addr},
    xms::Xms,
};
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
    cd: CdRom,
}
impl Disc {
    fn new() -> Self {
        let dir =
            PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required")).join(format!(
                "planar-service-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&dir).unwrap();
        let mut raw = vec![0; 20 * 2352];
        for i in 0..18 {
            raw[i * 2352 + 15] = 1;
            raw[i * 2352 + 16..i * 2352 + 2064].fill(i as u8);
        }
        raw[16 * 2352 + 17..16 * 2352 + 22].copy_from_slice(b"CD001");
        let bin = dir.join("disc.bin");
        let cue = dir.join("disc.cue");
        fs::write(&bin, raw).unwrap();
        fs::write(&cue, "FILE \"disc.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:18\n").unwrap();
        Self {
            cd: CdRom::mount_bin_cue(&bin, &cue).unwrap(),
            dir,
        }
    }
}
impl Drop for Disc {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn memory() -> GuestMemory {
    GuestMemory::new(0x110000, A20::Enabled)
}
fn video() -> PlanarVideo {
    let mut v = PlanarVideo::new();
    v.write8(0xa0000, 0x65).unwrap();
    v.read8(0xa0000).unwrap();
    v.write8(0xa0001, 0x93).unwrap();
    v
}
fn pointer(at: Addr) -> u32 {
    (u32::from(at.segment) << 16) | u32::from(at.offset)
}
fn header(m: &mut GuestMemory, at: Addr, command: u8, length: u8) {
    let mut raw = [0; 25];
    raw[0] = length;
    raw[1] = 0xee;
    raw[2] = command;
    raw[3..5].copy_from_slice(&0xa55au16.to_le_bytes());
    // Linear fixture write can deliberately construct a segment-crossing header.
    m.write_linear(at.linear(), &raw).unwrap();
}
fn word(m: &mut GuestMemory, at: Addr, value: u16) {
    m.write16(at, value).unwrap();
}
fn dword(m: &mut GuestMemory, at: Addr, value: u32) {
    m.write_bytes(at, &value.to_le_bytes()).unwrap();
}
fn ioctl(m: &mut GuestMemory, dst: Addr, function: u8, count: u16) {
    header(m, AT, 3, 20);
    word(m, AT.wrapping_add(14), dst.offset);
    word(m, AT.wrapping_add(16), dst.segment);
    word(m, AT.wrapping_add(18), count);
    m.write8(dst, function).unwrap();
    if matches!(function, 7 | 11) {
        m.write8(dst.wrapping_add(1), u8::from(function == 11))
            .unwrap();
    }
}
fn read_sectors(m: &mut GuestMemory, dst: Addr, count: u16) {
    header(m, AT, 0x80, 25);
    word(m, AT.wrapping_add(14), dst.offset);
    word(m, AT.wrapping_add(16), dst.segment);
    word(m, AT.wrapping_add(18), count);
    dword(m, AT.wrapping_add(20), 17);
}
fn cd_call(cd: &mut CdRom, m: &mut GuestMemory, at: Addr, mode: u8) -> Result<(), Fault> {
    if mode == 0x12 {
        cd.preflight_ram_request(m, at, 0xa0000..0xb0000)?;
    }
    cd.request(m, at)
}
fn xms_call(x: &mut Xms, r: &mut Registers, m: &mut GuestMemory, mode: u8) -> Result<(), Fault> {
    if mode == 0x12 {
        x.preflight_ram_call(r, m, 0xa0000..0xb0000)?;
    }
    x.call(r, m)
}
fn rejected_cd(cd: &mut CdRom, m: &mut GuestMemory, at: Addr, v: &PlanarVideo) {
    let before_m = m.clone();
    let before_cd = format!("{cd:?}");
    let before_v = v.clone();
    assert!(cd_call(cd, m, at, 0x12).is_err());
    assert_eq!(*m, before_m, "拒绝不得写 unit/status/count/buffer");
    assert_eq!(format!("{cd:?}"), before_cd, "拒绝不得改变播放或队列状态");
    assert_eq!(*v, before_v, "preflight 不可触及 planes/latches");
}
fn descriptor(
    m: &mut GuestMemory,
    at: Addr,
    n: u32,
    src: u16,
    so: u32,
    dst: u16,
    target: u32,
) -> Registers {
    let mut raw = [0; 16];
    raw[..4].copy_from_slice(&n.to_le_bytes());
    raw[4..6].copy_from_slice(&src.to_le_bytes());
    raw[6..10].copy_from_slice(&so.to_le_bytes());
    raw[10..12].copy_from_slice(&dst.to_le_bytes());
    raw[12..16].copy_from_slice(&target.to_le_bytes());
    m.write_linear(at.linear(), &raw).unwrap();
    Registers {
        ax: 0x0b7e,
        bx: 0xabcd,
        cx: 0x4567,
        dx: 0x3210,
        ds: at.segment,
        si: at.offset,
        flags: Flags::from_bits(0xa55a),
        ..Registers::default()
    }
}
fn rejected_xms(x: &mut Xms, r: &mut Registers, m: &mut GuestMemory, v: &PlanarVideo) {
    let before_m = m.clone();
    let before_r = *r;
    let before_x = format!("{x:?}");
    let before_v = v.clone();
    assert!(xms_call(x, r, m, 0x12).is_err());
    assert_eq!(*r, before_r);
    assert_eq!(*m, before_m);
    assert_eq!(format!("{x:?}"), before_x, "包括 blocks/地址/锁");
    assert_eq!(*v, before_v);
}
fn allocate(x: &mut Xms, m: &mut GuestMemory) -> u16 {
    let mut r = Registers {
        ax: 0x900,
        dx: 1,
        ..Registers::default()
    };
    xms_call(x, &mut r, m, 0x12).unwrap();
    assert_eq!(r.ax, 1);
    r.dx
}

#[test]
fn mscdex_header_alias_status_and_full_schema_are_atomic() {
    let mut disc = Disc::new();
    let mut m = memory();
    let v = video();
    for at in [
        Addr::new(0xa000, 0),
        Addr::new(0x9fff, 0x10),
        Addr::new(0x9fff, 0xc),
        Addr::new(0xafff, 0xf),
        Addr::new(0x9ffe, 0x14),
    ] {
        for cmd in [3, 0x80, 0x84, 0x85, 0x88] {
            header(&mut m, at, cmd, 0);
            rejected_cd(&mut disc.cd, &mut m, at, &v);
        }
    }
}
#[test]
fn mscdex_actual_span_not_declared_length_and_legacy13() {
    let mut disc = Disc::new();
    let mut m = memory();
    let at = Addr::new(0x9ffe, 0x13); // exactly 13 RAM bytes before aperture
    header(&mut m, at, 0x85, 255);
    cd_call(&mut disc.cd, &mut m, at, 0x12).unwrap();
    assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x100);
    header(&mut m, at, 0x84, 13);
    // strict short header only writes status, never reads later fields
    cd_call(&mut disc.cd, &mut m, at, 0x12).unwrap();
    assert_eq!(m.read16(at.wrapping_add(3)).unwrap(), 0x8105);
    disc.cd.enable_legacy_header13();
    rejected_cd(&mut disc.cd, &mut m, at, &video());
}
#[test]
fn mscdex_segment_and_backing_bounds_reject_before_status() {
    let mut disc = Disc::new();
    let mut m = memory();
    for at in [Addr::new(0, 0xfffc), Addr::new(0, 0xfff0)] {
        header(&mut m, at, 0x84, 0);
        rejected_cd(&mut disc.cd, &mut m, at, &video());
    }
    let mut m = GuestMemory::new(0x104, A20::Enabled);
    m.write8(AT, 1).unwrap();
    m.write8(AT.wrapping_add(2), 0x85).unwrap();
    rejected_cd(&mut disc.cd, &mut m, AT, &video());
}
#[test]
fn mscdex_every_ioctl_schema_and_zero_count_checks_nested_span() {
    let mut disc = Disc::new();
    let mut m = memory();
    for (function, size) in [(6, 5), (7, 4), (8, 5), (9, 2), (10, 7), (11, 7), (15, 11)] {
        for count in [0, size, 65535] {
            for dst in [
                Addr::new(0xa000, 0),
                Addr::new(0x9fff, 0x10),
                Addr::new(0x9ffe, 0x20 - size + 1),
                Addr::new(0xafff, 0xf),
            ] {
                ioctl(&mut m, dst, function, count);
                rejected_cd(&mut disc.cd, &mut m, AT, &video());
            }
        }
        // Exact lower aperture edge is valid; oversized count isn't a span.
        let dst = Addr::new(0x9ffe, 0x20 - size);
        ioctl(&mut m, dst, function, 65535);
        cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
        assert_eq!(m.read16(AT.wrapping_add(18)).unwrap(), size);
        assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x100);
    }
}
#[test]
fn mscdex_short_ioctl_and_invalid_argument_only_check_actual_accesses() {
    let mut disc = Disc::new();
    let mut m = memory();
    let dst = Addr::new(0x9fff, 0xf);
    ioctl(&mut m, dst, 15, 1);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x8105);
    let dst = Addr::new(0x9fff, 0xe);
    for (function, argument, status) in [(7, 1, 0x8103), (11, 0, 0x8108)] {
        ioctl(&mut m, dst, function, 0);
        m.write8(dst.wrapping_add(1), argument).unwrap();
        cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
        assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), status);
    }
}
#[test]
fn mscdex_sector_destinations_cover_whole_and_mixed_aperture() {
    let mut disc = Disc::new();
    let mut m = memory();
    for dst in [
        Addr::new(0xa000, 0),
        Addr::new(0x9fff, 0x10),
        Addr::new(0x9f80, 1),
        Addr::new(0xafff, 0xf),
        Addr::new(0, 0xfc00),
    ] {
        read_sectors(&mut m, dst, 1);
        rejected_cd(&mut disc.cd, &mut m, AT, &video());
    }
    for dst in [Addr::new(0x9f80, 0), Addr::new(0xb000, 0), DST] {
        read_sectors(&mut m, dst, 1);
        cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
        assert_eq!(m.read_linear(dst.linear(), 2048).unwrap(), vec![17; 2048]);
    }
}
#[test]
fn mscdex_zero_sector_and_invalid_range_skip_unused_destination() {
    let mut disc = Disc::new();
    let mut m = memory();
    read_sectors(&mut m, Addr::new(0xa000, 0), 0);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x100);
    read_sectors(&mut m, Addr::new(0xa000, 0), 1);
    dword(&mut m, AT.wrapping_add(20), 999);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x8108);
}
#[test]
fn mscdex_audio_rejection_preserves_playback_queue_and_position() {
    let mut disc = Disc::new();
    let mut m = memory();
    header(&mut m, AT, 0x84, 22);
    dword(&mut m, AT.wrapping_add(14), 18);
    dword(&mut m, AT.wrapping_add(18), 2);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    disc.cd.prefetch_audio().unwrap();
    assert_eq!(disc.cd.render_audio(&mut [[0; 2]; 3]), 3);
    for cmd in [0x84, 0x85, 0x88] {
        let at = Addr::new(0x9fff, 0xc);
        header(&mut m, at, cmd, 0);
        rejected_cd(&mut disc.cd, &mut m, at, &video());
    }
}
#[test]
fn mscdex_a20_header_and_transfer_aliases_remain_valid_ram() {
    let mut disc = Disc::new();
    let mut m = memory();
    m.set_a20(A20::Disabled);
    let at = Addr::new(0xffff, 0x20);
    header(&mut m, at, 0x85, 13);
    cd_call(&mut disc.cd, &mut m, at, 0x12).unwrap();
    assert_eq!(m.as_bytes()[0x13..0x15], [0, 1]);
    ioctl(&mut m, Addr::new(0xffff, 0x10), 15, 0);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    assert_eq!(m.as_bytes()[0], 15);
}
#[test]
fn xms_descriptor_alias_mixed_segment_and_backing_bounds_are_atomic() {
    let mut x = Xms::default();
    let mut m = memory();
    let v = video();
    for at in [
        Addr::new(0xa000, 0),
        Addr::new(0x9fff, 0x10),
        Addr::new(0x9fff, 0xf),
        Addr::new(0xafff, 0xf),
        Addr::new(0, 0xfff1),
    ] {
        let mut r = descriptor(&mut m, at, 2, 0, pointer(DST), 0, pointer(DST));
        rejected_xms(&mut x, &mut r, &mut m, &v);
    }
    let mut m = GuestMemory::new(0x108, A20::Enabled);
    let mut r = Registers {
        ax: 0xb00,
        si: AT.offset,
        ..Registers::default()
    };
    rejected_xms(&mut x, &mut r, &mut m, &v);
}
#[test]
fn xms_both_handle_zero_buffers_reject_all_aperture_and_mixed_spans() {
    let mut x = Xms::default();
    let mut m = memory();
    let handle = allocate(&mut x, &mut m);
    let v = video();
    for forbidden in [
        Addr::new(0xa000, 0),
        Addr::new(0x9fff, 0x10),
        Addr::new(0x9fff, 0xf),
        Addr::new(0xafff, 0xf),
    ] {
        for (src, so, dst, target) in [
            (0, pointer(forbidden), handle, 0),
            (handle, 0, 0, pointer(forbidden)),
            (0, pointer(DST), 0, pointer(forbidden)),
            (0, pointer(forbidden), 0, pointer(DST)),
        ] {
            let mut r = descriptor(&mut m, AT, 2, src, so, dst, target);
            rejected_xms(&mut x, &mut r, &mut m, &v);
        }
    }
    // A long linear transfer starts outside but encloses the complete aperture.
    let mut r = descriptor(
        &mut m,
        AT,
        0x20002,
        0,
        pointer(Addr::new(0x9000, 0)),
        handle,
        0,
    );
    rejected_xms(&mut x, &mut r, &mut m, &v);
}
#[test]
fn xms_large_handle_offsets_are_not_misclassified_as_physical() {
    let mut x = Xms::default();
    let mut m = memory();
    let handle = allocate(&mut x, &mut m);
    let mut r = descriptor(&mut m, AT, 2, handle, 0xa0000, handle, 0xb0000);
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!((r.ax, r.bx), (0, 0xa4));
}
#[test]
fn xms_cross_64k_linear_transfer_and_exact_aperture_edges() {
    let mut x = Xms::default();
    let mut m = memory();
    let source = Addr::new(0x1000, 0xffff);
    m.write_linear(source.linear(), &[1, 2, 3, 4]).unwrap();
    let mut r = descriptor(&mut m, AT, 4, 0, pointer(source), 0, pointer(DST));
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!((r.ax, r.bx), (1, 0));
    assert_eq!(m.read_linear(DST.linear(), 4).unwrap(), [1, 2, 3, 4]);
    for dst in [Addr::new(0x9fff, 0xc), Addr::new(0xb000, 0)] {
        let mut r = descriptor(&mut m, AT, 4, 0, pointer(source), 0, pointer(dst));
        xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
        assert_eq!(m.read_linear(dst.linear(), 4).unwrap(), [1, 2, 3, 4]);
    }
}
#[test]
fn xms_a20_per_byte_wrap_and_enabled_hma_are_preserved() {
    // A 1-MiB guest exposes missing preflight A20 translation: the backing
    // high byte is out of bounds, while its disabled-A20 alias is valid RAM.
    let mut x = Xms::default();
    let mut m = GuestMemory::new(0x100000, A20::Disabled);
    let source = Addr::new(0xffff, 0xf);
    m.write_linear(source.linear(), &[11, 22, 33, 44]).unwrap();
    let mut r = descriptor(&mut m, AT, 4, 0, pointer(source), 0, pointer(DST));
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!(r.ax, 1);
    assert_eq!(m.read_linear(DST.linear(), 4).unwrap(), [11, 22, 33, 44]);
    for a20 in [A20::Disabled, A20::Enabled] {
        let mut x = Xms::default();
        let mut m = GuestMemory::new(0x110000, a20);
        let source = Addr::new(0xffff, 0xf);
        m.write_linear(source.linear(), &[11, 22, 33, 44]).unwrap();
        let at = Addr::new(0xffff, 0x110);
        let mut r = descriptor(&mut m, at, 4, 0, pointer(source), 0, pointer(DST));
        xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
        assert_eq!(r.ax, 1);
        assert_eq!(m.read_linear(DST.linear(), 4).unwrap(), [11, 22, 33, 44]);
        assert_eq!(
            m.as_bytes()[if a20 == A20::Disabled { 0 } else { 0x100000 }],
            22
        );
    }
}
#[test]
fn xms_zero_odd_and_non_move_calls_do_not_inspect_unused_buffers() {
    let mut x = Xms::default();
    let mut m = memory();
    for (n, expected) in [(0, (1, 0)), (1, (0, 0xa7))] {
        let mut r = descriptor(&mut m, AT, n, 0, 0xa0000000, 0, 0xa0000000);
        xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
        assert_eq!((r.ax, r.bx), expected);
    }
    let mut r = Registers {
        ds: 0xa000,
        si: 0xffff,
        ..Registers::default()
    };
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!(r.ax, 0x300);
}
#[test]
fn xms_ram_destination_failure_preserves_blocks_ram_and_registers() {
    let mut x = Xms::default();
    let mut m = GuestMemory::new(0x3000, A20::Enabled);
    let handle = allocate(&mut x, &mut m);
    let mut r = descriptor(&mut m, AT, 4, handle, 0, 0, pointer(Addr::new(0, 0x2fff)));
    rejected_xms(&mut x, &mut r, &mut m, &video());
}
#[test]
fn mode13_original_ram_services_remain_unfiltered() {
    let mut disc = Disc::new();
    let mut m = memory();
    let dst = Addr::new(0xa000, 0);
    read_sectors(&mut m, dst, 1);
    cd_call(&mut disc.cd, &mut m, AT, 0x13).unwrap();
    assert_eq!(m.read_linear(0xa0000, 2048).unwrap(), vec![17; 2048]);
    let mut x = Xms::default();
    let mut r = descriptor(&mut m, AT, 4, 0, pointer(dst), 0, pointer(DST));
    xms_call(&mut x, &mut r, &mut m, 0x13).unwrap();
    assert_eq!(r.ax, 1);
    assert_eq!(m.read_linear(DST.linear(), 4).unwrap(), [17; 4]);
}

#[test]
fn xms_ram_block_roundtrip_and_a20_disabled_destination() {
    let mut x = Xms::default();
    let mut m = memory();
    let h = allocate(&mut x, &mut m);
    m.write_bytes(DST, &[0x21, 0x43, 0x65, 0x87]).unwrap();
    let mut r = descriptor(&mut m, AT, 4, 0, pointer(DST), h, 1020);
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!(r.ax, 1);
    m.set_a20(A20::Disabled);
    let out = Addr::new(0xffff, 0xf);
    let mut r = descriptor(&mut m, AT, 4, h, 1020, 0, pointer(out));
    xms_call(&mut x, &mut r, &mut m, 0x12).unwrap();
    assert_eq!(r.ax, 1);
    assert_eq!(m.as_bytes()[0xfffff], 0x21);
    assert_eq!(&m.as_bytes()[..3], &[0x43, 0x65, 0x87]);
    let mut r = descriptor(&mut m, AT, u32::MAX - 1, 0, pointer(DST), h, 0);
    rejected_xms(&mut x, &mut r, &mut m, &video());
}
#[test]
fn mscdex_sector_destination_a20_wrap_is_per_byte() {
    let mut disc = Disc::new();
    let mut m = memory();
    m.set_a20(A20::Disabled);
    let dst = Addr::new(0xffff, 0xf);
    read_sectors(&mut m, dst, 1);
    cd_call(&mut disc.cd, &mut m, AT, 0x12).unwrap();
    assert_eq!(m.as_bytes()[0xfffff], 17);
    // Transfer aliases the request: finish_request re-writes the status there.
    assert_eq!(&m.as_bytes()[..0x100], &[17; 0x100]);
    assert_eq!(&m.as_bytes()[0x120..2047], &[17; 2047 - 0x120]);
    assert_eq!(m.read16(AT.wrapping_add(3)).unwrap(), 0x100);
}
