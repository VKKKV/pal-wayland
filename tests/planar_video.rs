// SPDX-License-Identifier: GPL-3.0-or-later
//! 原创 synthetic fixtures，std-only；不用 game bytes、GUI、音频或 runtime。
//! 独立验证（不构建 runtime/GUI）：
//! rustc --edition=2024 --crate-name planar_video --crate-type=rlib src/devices/planar_video.rs -o "$TMPDIR/libplanar_video.rlib"
//! 将本文件的 crate import 改为 `use planar_video::*;` 写入 scratch 副本，随后：
//! rustc --edition=2024 --test "$TMPDIR/planar_video_tests.rs" --extern planar_video="$TMPDIR/libplanar_video.rlib" -o "$TMPDIR/planar-tests"
//! "$TMPDIR/planar-tests"
//! module 与 tests 均再以 -O -C overflow-checks=yes 编译执行优化版本。
#![forbid(unsafe_code)]
use pal_wayland::planar_video::*;

fn gc(v: &mut PlanarVideo, index: u8, value: u8) {
    v.write_port16(0x3ce, u16::from_le_bytes([index, value]))
        .unwrap();
}
fn map(v: &mut PlanarVideo, value: u8) {
    v.write_port16(0x3c4, u16::from_le_bytes([2, value]))
        .unwrap();
}
fn bytes(v: &PlanarVideo, offset: usize) -> [u8; 4] {
    std::array::from_fn(|p| v.plane(p).unwrap()[offset])
}
fn seed(v: &mut PlanarVideo, offset: usize, values: [u8; 4]) {
    gc(v, 5, 0);
    gc(v, 1, 0);
    gc(v, 3, 0);
    gc(v, 8, 255);
    for (p, value) in values.into_iter().enumerate() {
        map(v, 1 << p);
        v.write8(VRAM_BASE + offset as u32, value).unwrap();
    }
    map(v, 15);
}

#[test]
fn default_and_strict_supported_mode_table() {
    let mut v = PlanarVideo::new();
    assert_eq!(v.dimensions(), (640, 480));
    assert_eq!(STRIDE, 80);
    assert_eq!(v.latches(), [0; 4]);
    for p in 0..4 {
        assert_eq!(v.plane(p).unwrap(), vec![0; 65536]);
    }
    assert!(v.plane(4).is_err());
    for al in 0u8..=255 {
        let before = v.clone();
        let result = v.set_mode(al);
        assert_eq!(result.is_ok(), al == 0x12 || al == 0x92, "AL={al:02x}");
        if result.is_err() {
            assert_eq!(v, before);
        }
    }
}

#[test]
fn clearing_and_preserving_mode_resets_all_registers_and_latches() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [1, 2, 3, 4]);
    seed(&mut v, 65535, [255, 254, 253, 252]);
    v.read8(VRAM_END - 1).unwrap();
    for (index, value) in [
        (0, 15),
        (1, 15),
        (2, 14),
        (3, 31),
        (4, 3),
        (5, 11),
        (7, 1),
        (8, 0),
    ] {
        gc(&mut v, index, value);
    }
    map(&mut v, 0);
    let old = v.clone();
    v.set_mode(0x92).unwrap();
    for p in 0..4 {
        assert_eq!(v.plane(p).unwrap(), old.plane(p).unwrap());
    }
    assert_eq!(v.latches(), [0; 4]);
    for (i, value) in [0, 0, 0, 0, 0, 0, 5, 15, 255].into_iter().enumerate() {
        v.write_port(0x3ce, i as u8).unwrap();
        assert_eq!(v.read_port(0x3cf).unwrap(), value);
    }
    assert_eq!(v.read_port(0x3c4).unwrap(), 2);
    assert_eq!(v.read_port(0x3c5).unwrap(), 15);
    v.set_mode(0x12).unwrap();
    assert_eq!(v, PlanarVideo::default());
}

#[test]
fn byte_and_word_indexed_io_agree_and_have_real_readback() {
    let mut a = PlanarVideo::new();
    let mut b = a.clone();
    for (port, index, value) in [
        (0x3ce, 0, 10),
        (0x3ce, 1, 5),
        (0x3ce, 2, 6),
        (0x3ce, 3, 29),
        (0x3ce, 4, 3),
        (0x3ce, 5, 11),
        (0x3ce, 6, 5),
        (0x3ce, 7, 12),
        (0x3ce, 8, 0x9a),
        (0x3c4, 2, 5),
        (0x3c4, 4, 6),
    ] {
        a.write_port(port, index).unwrap();
        a.write_port(port + 1, value).unwrap();
        b.write_port16(port, u16::from_le_bytes([index, value]))
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.read_port(port).unwrap(), index);
        assert_eq!(a.read_port(port + 1).unwrap(), value);
    }
}

#[test]
fn rejected_index_data_and_word_out_are_atomic_exhaustive() {
    let mut v = PlanarVideo::new();
    for port in [0x3ce, 0x3c4] {
        for index in 0u8..=255 {
            let valid_index = if port == 0x3ce {
                index <= 8
            } else {
                index == 2 || index == 4
            };
            let before = v.clone();
            assert_eq!(v.write_port(port, index).is_ok(), valid_index);
            if !valid_index {
                assert_eq!(v, before);
            }
            for value in 0u8..=255 {
                // Independent acceptance table, not production validation call.
                let valid = match (port, index) {
                    (0x3ce, 0..=4 | 7 | 8) | (0x3c4, 2) => true,
                    // Real GC05 odd/even and shift bits must all remain zero.
                    (0x3ce, 5) => value & 0x70 == 0,
                    (0x3ce, 6) => value % 16 == 5,
                    (0x3c4, 4) => (value / 2) % 8 == 3,
                    _ => false,
                };
                // The high-cost full state comparison is retained for each
                // rejection; unchanged index alone would miss partial data commit.
                let before = v.clone();
                assert_eq!(
                    v.write_port16(port, u16::from_le_bytes([index, value]))
                        .is_ok(),
                    valid
                );
                if !valid {
                    assert_eq!(v, before, "port={port:x} index={index} value={value}");
                }
            }
        }
    }
    for port in [0, 0x3c5, 0x3cf, 0x3c6, 0x3da, 0xffff] {
        let before = v.clone();
        assert!(v.write_port16(port, 0x0f02).is_err());
        assert_eq!(v, before);
    }
    for (port, index, value) in [
        (0x3ce, 5, 0x10),
        (0x3ce, 6, 0x0d),
        (0x3c4, 4, 0x0e),
        (0x3ce, 5, 0xa0), // shift interleave even with reserved bit 7.
        (0x3ce, 5, 0xc4), // shift256 even with both unused bits.
        (0x3ce, 6, 0xf7), // odd/even even with reserved high nibble.
        (0x3ce, 6, 0xf4), // alphanumeric instead of graphics.
        (0x3c4, 4, 0xf3), // odd/even enabled, bit 0 unused.
        (0x3c4, 4, 0xf5), // extended memory disabled.
        (0x3c4, 4, 0xff), // chain4 enabled even with reserved bits.
    ] {
        v.write_port(port, index).unwrap();
        let before = v.clone();
        assert!(v.write_port(port + 1, value).is_err());
        assert_eq!(v, before);
    }
    for port in [0, 0x3c6, 0x3da, 0xffff] {
        let before = v.clone();
        assert!(v.read_port(port).is_err());
        assert!(v.write_port(port, 0).is_err());
        assert_eq!(v, before);
    }
}

#[test]
fn documented_unused_bits_normalize_byte_and_word_io_exhaustive() {
    // Independent field widths expressed without production masks.
    for (port, index, modulus) in [
        (0x3ce, 0, 16),
        (0x3ce, 1, 16),
        (0x3ce, 2, 16),
        (0x3ce, 3, 32),
        (0x3ce, 4, 4),
        (0x3ce, 5, 0),
        (0x3ce, 6, 16),
        (0x3ce, 7, 16),
        (0x3ce, 8, 256),
        (0x3c4, 2, 16),
        (0x3c4, 4, 0),
    ] {
        let mut word = PlanarVideo::new();
        seed(&mut word, 0, [0x12, 0x34, 0x56, 0x78]);
        word.read8(VRAM_BASE).unwrap();
        for value in 0u8..=255 {
            let canonical = match (port, index) {
                (0x3ce, 5) if (value / 16) % 8 != 0 => None,
                (0x3ce, 5) => Some((value % 4) + ((value / 8) % 2) * 8),
                (0x3ce, 6) if value % 16 != 5 => None,
                (0x3c4, 4) if (value / 2) % 8 != 3 => None,
                (0x3c4, 4) => Some(6),
                _ => Some((u16::from(value) % modulus) as u8),
            };
            let mut byte = word.clone();
            byte.write_port(port, index).unwrap();
            let byte_before = byte.clone();
            let word_before = word.clone();
            let byte_result = byte.write_port(port + 1, value);
            let word_result = word.write_port16(port, u16::from_le_bytes([index, value]));
            assert_eq!(byte_result.is_ok(), canonical.is_some());
            assert_eq!(word_result.is_ok(), canonical.is_some());
            if let Some(expected) = canonical {
                assert_eq!(byte, word, "port={port:x} index={index} value={value:x}");
                assert_eq!(word.read_port(port).unwrap(), index);
                assert_eq!(byte.read_port(port + 1).unwrap(), expected);
                assert_eq!(word.read_port(port + 1).unwrap(), expected);
                assert_eq!(word.latches(), [0x12, 0x34, 0x56, 0x78]);
                assert_eq!(bytes(&word, 0), [0x12, 0x34, 0x56, 0x78]);
            } else {
                assert_eq!(byte, byte_before);
                assert_eq!(word, word_before);
            }
        }
    }
}

#[test]
fn bgi_ff01_enables_four_planes_and_matches_0f01_rmw_and_frame() {
    let mut canonical = PlanarVideo::new();
    seed(&mut canonical, 0, [0x55, 0xaa, 0x0f, 0xf0]);
    seed(&mut canonical, 1, [0x12, 0x34, 0x56, 0x78]);
    canonical.read8(VRAM_BASE).unwrap();
    gc(&mut canonical, 0, 10);
    gc(&mut canonical, 3, 0);
    gc(&mut canonical, 8, 0x3c);
    let mut word = canonical.clone();
    let mut byte = canonical.clone();
    gc(&mut canonical, 1, 0x0f);
    word.write_port16(0x3ce, 0xff01).unwrap(); // Actual BGI indexed OUT value.
    byte.write_port(0x3ce, 1).unwrap();
    byte.write_port(0x3cf, 0xff).unwrap();
    assert_eq!(word.read_port(0x3cf).unwrap(), 0x0f);
    assert_eq!(byte, canonical);
    assert_eq!(word, canonical);
    for v in [&mut canonical, &mut word, &mut byte] {
        v.write8(VRAM_BASE + 1, 0x96).unwrap(); // Host data replaced on ALL planes.
        assert_eq!(bytes(v, 1), [0x41, 0xbe, 0x03, 0xfc]);
        assert_eq!(v.latches(), [0x55, 0xaa, 0x0f, 0xf0]);
        assert_eq!(&v.frame_indices()[8..16], &[10, 9, 10, 10, 10, 10, 6, 5]);
    }
    assert_eq!(word, canonical);
    assert_eq!(byte.frame(), canonical.frame());
    assert_eq!(word.frame(), canonical.frame());
}

#[test]
fn reserved_bits_do_not_alias_indices_or_hide_unsupported_modes() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x55, 0xaa, 0x0f, 0xf0]);
    v.read8(VRAM_BASE).unwrap();
    for (port, index, data) in [
        (0x3ce, 0x81, 0xff),
        (0x3c4, 0x82, 0xff),
        (0x3ce, 9, 0xff),
        (0x3c4, 3, 0xff),
        (0x3ce, 5, 0x94), // real host odd/even + unused bits.
        (0x3ce, 5, 0xa4), // real interleaved shift + unused bits.
        (0x3ce, 5, 0xc4), // real 256-colour shift + unused bits.
        (0x3ce, 6, 0xfd), // B8000 aperture + unused high nibble.
        (0x3ce, 6, 0xf7), // chain odd/even + unused high nibble.
        (0x3c4, 4, 0xff), // chain4 + unused bits.
        (0x3c4, 4, 0xf3), // odd/even host writes + unused bits.
    ] {
        let before = v.clone();
        assert!(
            v.write_port16(port, u16::from_le_bytes([index, data]))
                .is_err()
        );
        assert_eq!(v, before, "port={port:x} index={index:x} data={data:x}");
        // Failed word OUT must preserve the previously selected index as well.
        assert_eq!(v.read_port(port).unwrap(), before.read_port(port).unwrap());
    }
}

#[test]
fn each_read_loads_all_latches_and_write_does_not_refresh_them() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x55, 0xaa, 0x0f, 0xf0]);
    seed(&mut v, 1, [1, 2, 3, 4]);
    for p in 0..4 {
        gc(&mut v, 4, p);
        assert_eq!(
            v.read8(VRAM_BASE).unwrap(),
            [0x55, 0xaa, 0x0f, 0xf0][p as usize]
        );
        assert_eq!(v.latches(), [0x55, 0xaa, 0x0f, 0xf0]);
    }
    gc(&mut v, 5, 0);
    gc(&mut v, 0, 10);
    gc(&mut v, 1, 6);
    gc(&mut v, 3, 27);
    gc(&mut v, 8, 0x3c);
    v.write8(VRAM_BASE + 1, 0x96).unwrap();
    assert_eq!(bytes(&v, 1), [0x45, 0x96, 0x0f, 0xe0]);
    assert_eq!(v.latches(), [0x55, 0xaa, 0x0f, 0xf0]);
    // This destination differs from the source latch: implicit reload is wrong.
    v.write8(VRAM_BASE + 2, 0x96).unwrap();
    assert_eq!(bytes(&v, 2), bytes(&v, 1));
    assert_eq!(bytes(&v, 0), [0x55, 0xaa, 0x0f, 0xf0]);
}

#[test]
fn mode1_ignores_host_rotate_bitmask_setreset_and_operation() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x12, 0x34, 0x56, 0x78]);
    seed(&mut v, 1, [0xaa; 4]);
    v.read8(VRAM_BASE).unwrap();
    gc(&mut v, 5, 1);
    gc(&mut v, 0, 15);
    gc(&mut v, 1, 15);
    gc(&mut v, 3, 31);
    gc(&mut v, 8, 0);
    map(&mut v, 5);
    v.write_linear(VRAM_BASE + 1, &[0xff, 0, 0x5a]).unwrap();
    assert_eq!(bytes(&v, 1), [0x12, 0xaa, 0x56, 0xaa]);
    assert_eq!(bytes(&v, 2), [0x12, 0, 0x56, 0]);
    assert_eq!(bytes(&v, 3), [0x12, 0, 0x56, 0]);
    assert_eq!(v.latches(), [0x12, 0x34, 0x56, 0x78]);
}

#[test]
fn mode3_ignores_logical_op_and_enable_setreset() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x55, 0xaa, 0x0f, 0xf0]);
    v.read8(VRAM_BASE).unwrap();
    gc(&mut v, 5, 3);
    gc(&mut v, 0, 5);
    gc(&mut v, 8, 0x3c);
    for op in 0..4 {
        gc(&mut v, 3, (op << 3) | 3);
        for en in 0..16 {
            gc(&mut v, 1, en);
            v.write8(VRAM_BASE + 1, 0x96).unwrap();
            assert_eq!(
                bytes(&v, 1),
                [0x55, 0xaa, 0x1f, 0xe0],
                "op={op} enable={en}"
            );
        }
    }
}

#[test]
fn mode2_ignores_rotation_and_setreset_low_nibble_expands() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x55, 0xaa, 0x0f, 0xf0]);
    v.read8(VRAM_BASE).unwrap();
    gc(&mut v, 5, 2);
    gc(&mut v, 0, 15);
    gc(&mut v, 1, 15);
    gc(&mut v, 8, 255);
    for rot in 0..8 {
        gc(&mut v, 3, rot);
        v.write8(VRAM_BASE + 1, 0xfa).unwrap();
        assert_eq!(bytes(&v, 1), [0, 255, 0, 255]);
    }
}

#[test]
fn read1_all_compare_care_combinations_and_zero_care_matches_every_pixel() {
    let mut v = PlanarVideo::new();
    // Pixels from left to right: 0,1,2,3,4,5,6,7 then 8..15.
    seed(&mut v, 0, [0x55, 0x33, 0x0f, 0]);
    seed(&mut v, 1, [0x55, 0x33, 0x0f, 255]);
    gc(&mut v, 5, 8);
    for compare in 0..16 {
        gc(&mut v, 2, compare);
        for care in 0..16 {
            gc(&mut v, 7, care);
            for group in 0..2 {
                let mut expected = 0u8;
                for pixel in 0..8 {
                    if (((group * 8 + pixel) as u8) ^ compare) & care == 0 {
                        expected += 1 << (7 - pixel);
                    }
                }
                assert_eq!(
                    v.read8(VRAM_BASE + group as u32).unwrap(),
                    expected,
                    "compare={compare} care={care} group={group}"
                );
                assert_eq!(v.latches(), bytes(&v, group));
            }
        }
    }
}

#[test]
fn full_64k_aperture_maps_same_offset_in_all_four_planes_without_spill() {
    let mut v = PlanarVideo::new();
    for p in 0..4 {
        let authored: Vec<u8> = (0..PLANE_BYTES)
            .map(|i| (i.wrapping_mul(37) + (i >> 8) * 11 + p * 19) as u8)
            .collect();
        map(&mut v, 1 << p);
        v.write_linear(VRAM_BASE, &authored).unwrap();
        assert_eq!(v.plane(p).unwrap(), authored);
        gc(&mut v, 4, p as u8);
        let mut output = vec![0; PLANE_BYTES];
        v.read_linear(VRAM_BASE, &mut output).unwrap();
        assert_eq!(output, authored);
    }
    for p in 0..4 {
        for i in [0, 32767, 38399, 38400, 65535] {
            assert_eq!(
                v.plane(p).unwrap()[i],
                (i * 37 + (i >> 8) * 11 + p * 19) as u8
            );
        }
    }
}

#[test]
fn boundary_preflight_preserves_full_device_and_output() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 65535, [1, 2, 3, 4]);
    v.read8(VRAM_END - 1).unwrap();
    for (start, len) in [
        (0, 1),
        (VRAM_BASE - 1, 2),
        (VRAM_END - 1, 2),
        (VRAM_END - 3, 4),
        (VRAM_END, 1),
        (VRAM_END + 1, 0),
        (0xb8000, 1),
        (u32::MAX, 2),
    ] {
        let before = v.clone();
        let mut out = vec![0xa5; len];
        assert!(v.read_linear(start, &mut out).is_err());
        assert_eq!(out, vec![0xa5; len]);
        assert_eq!(v, before);
        assert!(v.write_linear(start, &out).is_err());
        assert_eq!(v, before);
    }
    assert!(PlanarVideo::validate_range(VRAM_BASE, usize::MAX).is_err());
    assert!(PlanarVideo::validate_range(u32::MAX, usize::MAX).is_err());
    assert_eq!(PlanarVideo::validate_range(VRAM_BASE, 65536).unwrap(), 0);
    let before = v.clone();
    v.write_linear(VRAM_END, &[]).unwrap();
    v.read_linear(VRAM_END, &mut []).unwrap();
    assert_eq!(v, before);
    assert!(v.read16(VRAM_END - 1).is_err());
    assert_eq!(v, before);
    assert!(v.read32(VRAM_END - 3).is_err());
    assert_eq!(v, before);
    assert!(v.write16(VRAM_END - 1, 0xabcd).is_err());
    assert_eq!(v, before);
    assert!(v.write32(VRAM_END - 3, 0xabcd1234).is_err());
    assert_eq!(v, before);
}

#[test]
fn little_endian_wide_access_and_last_byte_latches() {
    let mut v = PlanarVideo::new();
    v.write32(VRAM_END - 4, 0x78563412).unwrap();
    assert_eq!(v.read32(VRAM_END - 4).unwrap(), 0x78563412);
    assert_eq!(v.latches(), [0x78; 4]);
    v.write16(VRAM_BASE, 0xcdab).unwrap();
    assert_eq!(v.latches(), [0x78; 4]);
    assert_eq!(v.read16(VRAM_BASE).unwrap(), 0xcdab);
    assert_eq!(v.latches(), [0xcd; 4]);
    // Masked bulk writes reuse the same previous read, not last written byte.
    gc(&mut v, 8, 0x0f);
    v.write32(VRAM_BASE + 8, 0x04030201).unwrap();
    assert_eq!(&v.plane(0).unwrap()[8..12], &[0xc1, 0xc2, 0xc3, 0xc4]);
    assert_eq!(v.latches(), [0xcd; 4]);
}

#[test]
fn frame_geometry_bit_order_palette_and_no_state_mutation() {
    let mut v = PlanarVideo::new();
    // Every displayed pixel has independent mathematical colour (x+3*y)%16.
    for p in 0..4 {
        let mut data = vec![0; PLANE_BYTES];
        for y in 0..480 {
            for x in 0..640 {
                if ((x + 3 * y) % 16) & (1 << p) != 0 {
                    data[y * 80 + x / 8] += 1 << (7 - x % 8);
                }
            }
        }
        data[65535] = 0xff; // Offscreen VRAM must not appear in frame.
        map(&mut v, 1 << p);
        v.write_linear(VRAM_BASE, &data).unwrap();
    }
    v.read8(VRAM_END - 1).unwrap();
    let before = v.clone();
    let indices = v.frame_indices();
    let frame = v.frame();
    let palette = std::array::from_fn(|i| 0x123400 + i as u32);
    let custom = v.frame_with_palette(&palette);
    assert_eq!(indices.len(), 640 * 480);
    assert_eq!(frame.len(), 640 * 480);
    for y in 0..480 {
        for x in 0..640 {
            let color = (x + 3 * y) % 16;
            let i = y * 640 + x;
            assert_eq!(indices[i], color as u8);
            assert_eq!(frame[i], EGA_PALETTE[color]);
            assert_eq!(custom[i], palette[color]);
        }
    }
    assert_eq!(EGA_PALETTE[6], 0xaa5500);
    assert_eq!(EGA_PALETTE[15], 0xffffff);
    assert_eq!(v, before);
}

#[test]
fn read1_to_mode3_search_replace_uses_read_result_as_pixel_mask() {
    let mut v = PlanarVideo::new();
    seed(&mut v, 0, [0x55, 0x33, 0x0f, 0]);
    seed(&mut v, 1, [0x55, 0x33, 0x0f, 255]);
    gc(&mut v, 2, 5);
    gc(&mut v, 7, 15);
    gc(&mut v, 0, 10);
    gc(&mut v, 1, 0); // mode3 must still use set/reset on all planes.
    gc(&mut v, 8, 255);
    gc(&mut v, 3, 24); // XOR must NOT apply in mode3.
    gc(&mut v, 5, 11); // read1 + write3 together.
    for offset in 0..2 {
        let mask = v.read8(VRAM_BASE + offset).unwrap();
        assert_eq!(mask, if offset == 0 { 4 } else { 0 });
        v.write8(VRAM_BASE + offset, mask).unwrap();
    }
    let indices = v.frame_indices();
    assert_eq!(
        &indices[..16],
        &[0, 1, 2, 3, 4, 10, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
}

#[test]
fn randomized_physical_bounds_preflight_and_rejected_write_atomicity() {
    let mut v = PlanarVideo::new();
    let mut rng = 0x7568259b4cdb1791;
    for trial in 0..3000 {
        let len = (rand(&mut rng) % 65) as usize;
        let start = match trial % 4 {
            0 => (VRAM_BASE - 64) + rand(&mut rng) % 129,
            1 => (VRAM_END - 64) + rand(&mut rng) % 129,
            2 => VRAM_BASE + rand(&mut rng) % 65536,
            _ => rand(&mut rng),
        };
        // Widened independent mathematical half-open interval oracle.
        let valid = start >= 0xa0000 && u64::from(start) + len as u64 <= 0xb0000;
        let before = v.clone();
        let data = vec![trial as u8; len];
        assert_eq!(v.write_linear(start, &data).is_ok(), valid);
        if !valid {
            assert_eq!(v, before, "start={start:x} len={len}");
        }
        let before_read = v.clone();
        let mut out = vec![0xa5; len];
        assert_eq!(v.read_linear(start, &mut out).is_ok(), valid);
        if valid {
            assert_eq!(out, data);
        } else {
            assert_eq!(out, vec![0xa5; len]);
            assert_eq!(v, before_read);
        }
    }
}

// Independent scalar reference: represent every plane byte as eight booleans;
// select each pixel with a truth table. No rotate_right/byte mask/compositing
// expression is shared with implementation. Unwritten destination bits come
// from prior READ latches, not destination RAM; map-disabled planes stay intact.
#[derive(Clone, Copy, Debug)]
struct Config {
    mode: u8,
    op: u8,
    rot: u8,
    sr: u8,
    enable: u8,
    mask: u8,
    maps: u8,
}
fn bit(value: u8, index: usize) -> bool {
    (u16::from(value) / (1u16 << index)) % 2 == 1
}
fn scalar_write(old: [u8; 4], latch: [u8; 4], data: u8, c: Config) -> [u8; 4] {
    let mut result = [0; 4];
    for p in 0..4 {
        for b in 0..8 {
            let l = bit(latch[p], b);
            let source = match c.mode {
                0 => {
                    if bit(c.enable, p) {
                        bit(c.sr, p)
                    } else {
                        bit(data, (b + c.rot as usize) % 8)
                    }
                }
                2 => bit(data, p),
                _ => bit(c.sr, p),
            };
            let operated = if c.mode == 3 {
                source
            } else {
                match c.op {
                    0 => source,
                    1 => source && l,
                    2 => source || l,
                    _ => source != l,
                }
            };
            let selected = bit(c.mask, b) && (c.mode != 3 || bit(data, (b + c.rot as usize) % 8));
            let output = if !bit(c.maps, p) {
                bit(old[p], b)
            } else if c.mode == 1 {
                l
            } else if selected {
                operated
            } else {
                l
            };
            if output {
                result[p] += 1 << b;
            }
        }
    }
    result
}
fn scalar_read(value: [u8; 4], mode: u8, map: u8, compare: u8, care: u8) -> u8 {
    if mode == 0 {
        return value[map as usize];
    }
    let mut out = 0;
    for b in 0..8 {
        let agrees = (0..4).all(|p| !bit(care, p) || bit(value[p], b) == bit(compare, p));
        if agrees {
            out += 1 << b;
        }
    }
    out
}
fn rand(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 32) as u32
}
#[test]
fn scalar_reference_randomized_rmw_mapmask_modes_and_bounds() {
    let mut v = PlanarVideo::new();
    let mut reference = vec![[0u8; 4]; 65536];
    let mut latch = [0u8; 4];
    let mut rng = 0x61bdc1a5937e2fa1;
    let mut seen_modes = [0usize; 4];
    let mut seen_ops = [0usize; 4];
    let mut seen_rot = [0usize; 8];
    let mut seen_maps = [0usize; 16];
    for trial in 0..40000 {
        let c = Config {
            mode: (trial % 4) as u8,
            op: (rand(&mut rng) % 4) as u8,
            rot: (rand(&mut rng) % 8) as u8,
            sr: (rand(&mut rng) % 16) as u8,
            enable: (rand(&mut rng) % 16) as u8,
            mask: rand(&mut rng) as u8,
            maps: (rand(&mut rng) % 16) as u8,
        };
        seen_modes[c.mode as usize] += 1;
        seen_ops[c.op as usize] += 1;
        seen_rot[c.rot as usize] += 1;
        seen_maps[c.maps as usize] += 1;
        gc(&mut v, 0, c.sr);
        gc(&mut v, 1, c.enable);
        gc(&mut v, 3, c.op * 8 + c.rot);
        gc(&mut v, 8, c.mask);
        map(&mut v, c.maps);
        let readmode = (rand(&mut rng) % 2) as u8;
        let readmap = (rand(&mut rng) % 4) as u8;
        let compare = (rand(&mut rng) % 16) as u8;
        let care = (rand(&mut rng) % 16) as u8;
        gc(&mut v, 2, compare);
        gc(&mut v, 4, readmap);
        gc(&mut v, 7, care);
        gc(&mut v, 5, c.mode + readmode * 8);
        // Dense offsets exercise repeated RMW; full-range and exact last byte
        // also ensure storage doesn't shrink to just the visible framebuffer.
        let offset = match trial % 7 {
            0 => 65535,
            1 => 38399,
            2 => 38400,
            3 => rand(&mut rng) as usize % 65536,
            _ => rand(&mut rng) as usize % 256,
        };
        if trial % 3 != 0 {
            let source = if trial % 5 == 0 {
                offset
            } else {
                rand(&mut rng) as usize % 256
            };
            latch = reference[source];
            assert_eq!(
                v.read8(VRAM_BASE + source as u32).unwrap(),
                scalar_read(latch, readmode, readmap, compare, care),
                "read trial={trial}"
            );
            assert_eq!(v.latches(), latch);
        }
        let data = rand(&mut rng) as u8;
        reference[offset] = scalar_write(reference[offset], latch, data, c);
        v.write8(VRAM_BASE + offset as u32, data).unwrap();
        assert_eq!(
            bytes(&v, offset),
            reference[offset],
            "write trial={trial} config={c:?}"
        );
        assert_eq!(
            v.latches(),
            latch,
            "writes must preserve latches trial={trial}"
        );
        if trial % 257 == 0 {
            let before = v.clone();
            assert!(v.write_linear(VRAM_END - 1, &[data, data]).is_err());
            assert_eq!(v, before);
        }
    }
    assert!(seen_modes.into_iter().all(|n| n > 0));
    assert!(seen_ops.into_iter().all(|n| n > 0));
    assert!(seen_rot.into_iter().all(|n| n > 0));
    assert!(seen_maps.into_iter().all(|n| n > 0));
    for p in 0..4 {
        for (offset, value) in reference.iter().enumerate() {
            assert_eq!(
                v.plane(p).unwrap()[offset],
                value[p],
                "plane={p} offset={offset}"
            );
        }
    }
}
