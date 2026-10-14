// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored text-video fixtures; no original game bytes or host devices.
//! Run: cargo test --offline --locked --no-default-features --test text_video

#![forbid(unsafe_code)]
use pal_wayland::{semantics, text_video};

#[cfg(test)]
mod video_tests {
    use super::semantics::{A20, GuestMemory, SegmentedAddress};
    use super::text_video::{
        self as video, CELLS, COLUMNS, DEFAULT_ATTRIBUTE, ROWS, Rect, SCREEN_BYTES,
        ScrollDirection, TeletypeEffect, VRAM_BASE,
    };

    fn memory() -> GuestMemory {
        let mut memory = GuestMemory::new(0xbc000, A20::Enabled);
        // Every neighboring byte differs from screen initialization; no asset.
        memory.write_linear(0, &vec![0xa5; 0xbc000]).unwrap();
        for row in 0..ROWS {
            for col in 0..COLUMNS {
                put(
                    &mut memory,
                    row,
                    col,
                    [cell_char(row, col), cell_attr(row, col)],
                );
            }
        }
        memory
    }

    fn cell_char(row: usize, col: usize) -> u8 {
        (row * 23 + col * 3) as u8
    }
    fn cell_attr(row: usize, col: usize) -> u8 {
        (row * 31 + col * 7 + 1) as u8
    }
    fn cursor(row: usize, col: usize) -> u16 {
        ((row as u16) << 8) | col as u16
    }
    fn put(memory: &mut GuestMemory, row: usize, col: usize, bytes: [u8; 2]) {
        memory
            .write_bytes(
                SegmentedAddress::new(0xb800, ((row * COLUMNS + col) * 2) as u16),
                &bytes,
            )
            .unwrap();
    }
    fn get(memory: &GuestMemory, row: usize, col: usize) -> [u8; 2] {
        let offset = VRAM_BASE + (row * COLUMNS + col) * 2;
        memory.as_bytes()[offset..offset + 2].try_into().unwrap()
    }
    fn assert_outside_screen(before: &GuestMemory, after: &GuestMemory) {
        assert_eq!(
            &before.as_bytes()[..VRAM_BASE],
            &after.as_bytes()[..VRAM_BASE]
        );
        assert_eq!(
            &before.as_bytes()[VRAM_BASE + SCREEN_BYTES..],
            &after.as_bytes()[VRAM_BASE + SCREEN_BYTES..]
        );
    }
    fn assert_scroll(
        before: &GuestMemory,
        after: &GuestMemory,
        rect: Rect,
        lines: u8,
        attr: u8,
        direction: ScrollDirection,
    ) {
        let height = usize::from(rect.bottom - rect.top) + 1;
        let n = if lines == 0 {
            height
        } else {
            usize::from(lines).min(height)
        };
        // Independent per-cell oracle reads only the immutable pre-call memory.
        for row in 0..ROWS {
            for col in 0..COLUMNS {
                let inside = row >= usize::from(rect.top)
                    && row <= usize::from(rect.bottom)
                    && col >= usize::from(rect.left)
                    && col <= usize::from(rect.right);
                let expected = if !inside {
                    get(before, row, col)
                } else {
                    let relative = row - usize::from(rect.top);
                    match direction {
                        ScrollDirection::Up if relative + n < height => get(before, row + n, col),
                        ScrollDirection::Down if relative >= n => get(before, row - n, col),
                        _ => [b' ', attr],
                    }
                };
                assert_eq!(get(after, row, col), expected, "row={row} col={col}");
            }
        }
        assert_outside_screen(before, after);
    }

    #[test]
    fn initialize_clears_exactly_visible_vram() {
        let mut m = memory();
        let before = m.clone();
        video::initialize(&mut m).unwrap();
        for row in 0..ROWS {
            for col in 0..COLUMNS {
                assert_eq!(get(&m, row, col), [b' ', DEFAULT_ATTRIBUTE]);
            }
        }
        assert_outside_screen(&before, &m);
    }

    #[test]
    fn read_packs_ax_and_observes_direct_guest_writes_and_a20() {
        for a20 in [A20::Disabled, A20::Enabled] {
            let mut m = memory();
            m.set_a20(a20);
            video::initialize(&mut m).unwrap();
            for (row, col) in [(0, 0), (13, 52), (24, 79)] {
                put(&mut m, row, col, [0xdb, 0xf1]);
                let before = m.clone();
                assert_eq!(
                    video::read_cell(&m, 3, 0, cursor(row, col)).unwrap(),
                    0xf1db
                );
                assert_eq!(m, before);
                put(&mut m, row, col, [b'Q', 0x28]);
                assert_eq!(
                    video::read_cell(&m, 3, 0, cursor(row, col)).unwrap(),
                    0x2851
                );
            }
        }
    }

    #[test]
    fn cursor_validation_exhaustive_bios_dx_domain() {
        for dx in 0..=u16::MAX {
            let row = dx >> 8;
            let col = dx & 255;
            let actual = video::validate_cursor(dx);
            if row < 25 && col < 80 {
                assert_eq!(actual.unwrap(), usize::from(row) * 80 + usize::from(col));
            } else {
                assert!(actual.is_err(), "accepted invalid DX={dx:04x}");
            }
        }
    }

    #[test]
    fn repeat_attribute_crosses_row_without_cursor_movement() {
        let mut m = memory();
        let before = m.clone();
        let dx = cursor(4, 78);
        assert_eq!(
            video::write_repeat(&mut m, 3, 0, dx, b'Z', Some(0xe7), 5).unwrap(),
            5
        );
        assert_eq!(dx, cursor(4, 78));
        for index in 0..CELLS {
            let (row, col) = (index / 80, index % 80);
            let expected = if (4 * 80 + 78..4 * 80 + 83).contains(&index) {
                [b'Z', 0xe7]
            } else {
                get(&before, row, col)
            };
            assert_eq!(get(&m, row, col), expected);
        }
        assert_outside_screen(&before, &m);
    }

    #[test]
    fn character_only_preserves_each_distinct_attribute_and_literal_controls() {
        for ch in [7, 8, 10, 13, 0, 0xff] {
            let mut m = memory();
            let before = m.clone();
            let start = 2 * 80 + 79;
            assert_eq!(
                video::write_repeat(&mut m, 3, 0, cursor(2, 79), ch, None, 84).unwrap(),
                84
            );
            for index in 0..CELLS {
                let (row, col) = (index / 80, index % 80);
                let mut expected = get(&before, row, col);
                if (start..start + 84).contains(&index) {
                    expected[0] = ch;
                }
                assert_eq!(get(&m, row, col), expected);
            }
            assert_outside_screen(&before, &m);
        }
    }

    #[test]
    fn repeat_zero_is_noop_and_max_cx_clips_without_overrun_or_scroll() {
        let mut m = memory();
        let before = m.clone();
        assert_eq!(
            video::write_repeat(&mut m, 3, 0, 0, b'X', Some(0x11), 0).unwrap(),
            0
        );
        assert_eq!(m, before);
        assert_eq!(
            video::write_repeat(&mut m, 3, 0, cursor(24, 78), b'X', Some(0x91), u16::MAX).unwrap(),
            2
        );
        for index in 0..CELLS {
            let (row, col) = (index / 80, index % 80);
            assert_eq!(
                get(&m, row, col),
                if index >= CELLS - 2 {
                    [b'X', 0x91]
                } else {
                    get(&before, row, col)
                }
            );
        }
        assert_outside_screen(&before, &m);
        let mut m = memory();
        assert_eq!(
            video::write_repeat(&mut m, 3, 0, 0, b'Y', None, u16::MAX).unwrap(),
            CELLS
        );
    }

    #[test]
    fn scroll_up_down_counts_and_rectangles_match_immutable_oracle() {
        for direction in [ScrollDirection::Up, ScrollDirection::Down] {
            for rect in [
                Rect::FULL_SCREEN,
                Rect {
                    top: 3,
                    left: 7,
                    bottom: 8,
                    right: 17,
                },
                Rect {
                    top: 0,
                    left: 0,
                    bottom: 0,
                    right: 0,
                },
                Rect {
                    top: 24,
                    left: 79,
                    bottom: 24,
                    right: 79,
                },
                Rect {
                    top: 0,
                    left: 79,
                    bottom: 24,
                    right: 79,
                },
            ] {
                for lines in [0, 1, 2, 5, 6, 24, 25, 26, 255] {
                    let mut m = memory();
                    let before = m.clone();
                    video::scroll(&mut m, 3, 0, rect, lines, 0xe2, direction).unwrap();
                    assert_scroll(&before, &m, rect, lines, 0xe2, direction);
                }
            }
        }
    }

    #[test]
    fn scroll_invalid_rectangles_return_error_without_mutation() {
        for rect in [
            Rect {
                top: 5,
                left: 0,
                bottom: 4,
                right: 79,
            },
            Rect {
                top: 0,
                left: 5,
                bottom: 24,
                right: 4,
            },
            Rect {
                top: 0,
                left: 0,
                bottom: 25,
                right: 79,
            },
            Rect {
                top: 0,
                left: 0,
                bottom: 24,
                right: 80,
            },
            Rect {
                top: 255,
                left: 255,
                bottom: 255,
                right: 255,
            },
        ] {
            for direction in [ScrollDirection::Up, ScrollDirection::Down] {
                let mut m = memory();
                let before = m.clone();
                assert!(video::scroll(&mut m, 3, 0, rect, 0, 0x77, direction).is_err());
                assert_eq!(m, before);
            }
        }
    }

    #[test]
    fn teletype_print_preserves_attribute_and_wraps_row() {
        for (start, end) in [(cursor(3, 7), cursor(3, 8)), (cursor(3, 79), cursor(4, 0))] {
            let mut m = memory();
            let before = m.clone();
            let mut dx = start;
            assert_eq!(
                video::teletype(&mut m, 3, 0, &mut dx, b'P').unwrap(),
                TeletypeEffect::CharacterWritten
            );
            assert_eq!(dx, end);
            let mut expected = before;
            let (row, col) = (usize::from(start >> 8), usize::from(start & 255));
            let attr = get(&expected, row, col)[1];
            put(&mut expected, row, col, [b'P', attr]);
            assert_eq!(m, expected);
        }
    }

    #[test]
    fn teletype_bs_cr_lf_bell_are_controls_not_glyphs() {
        for (ch, start, end, effect) in [
            (8, cursor(6, 9), cursor(6, 8), TeletypeEffect::CursorMoved),
            (8, cursor(6, 0), cursor(6, 0), TeletypeEffect::CursorMoved),
            (13, cursor(6, 79), cursor(6, 0), TeletypeEffect::CursorMoved),
            (
                10,
                cursor(6, 37),
                cursor(7, 37),
                TeletypeEffect::CursorMoved,
            ),
            (
                7,
                cursor(24, 79),
                cursor(24, 79),
                TeletypeEffect::BellIgnored,
            ),
        ] {
            let mut m = memory();
            let before = m.clone();
            let mut dx = start;
            assert_eq!(video::teletype(&mut m, 3, 0, &mut dx, ch).unwrap(), effect);
            assert_eq!(dx, end);
            assert_eq!(m, before);
        }
    }

    #[test]
    fn teletype_bottom_right_writes_before_scroll_and_lf_preserves_column() {
        for (ch, start, end) in [
            (b'Q', cursor(24, 79), cursor(24, 0)),
            (10, cursor(24, 37), cursor(24, 37)),
        ] {
            let mut m = memory();
            let original = m.clone();
            let mut oracle = original.clone();
            let col = usize::from(start & 255);
            let attr = get(&oracle, 24, col)[1];
            if ch != 10 {
                put(&mut oracle, 24, col, [ch, attr]);
            }
            let mut dx = start;
            video::teletype(&mut m, 3, 0, &mut dx, ch).unwrap();
            assert_eq!(dx, end);
            assert_scroll(&oracle, &m, Rect::FULL_SCREEN, 1, attr, ScrollDirection::Up);
            assert_outside_screen(&original, &m);
        }
    }

    #[test]
    fn teletype_tab_and_noncontrols_are_literal_and_use_fresh_guest_attribute() {
        for ch in [0, 9, 11, 12, 0xff] {
            let mut m = memory();
            let mut dx = cursor(9, 4);
            put(&mut m, 9, 4, [0, 0xf3]);
            video::teletype(&mut m, 3, 0, &mut dx, ch).unwrap();
            assert_eq!(get(&m, 9, 4), [ch, 0xf3]);
            assert_eq!(dx, cursor(9, 5));
        }
    }

    #[test]
    fn all_unsupported_modes_and_nonzero_pages_fail_atomically() {
        let mut m = memory();
        let before = m.clone();
        let contexts = (0..=u8::MAX)
            .filter(|&mode| mode != 3)
            .map(|mode| (mode, 0))
            .chain((1..=u8::MAX).map(|page| (3, page)));
        for (mode, page) in contexts {
            let mut dx = cursor(24, 79);
            assert!(video::read_cell(&m, mode, page, dx).is_err());
            assert!(video::write_repeat(&mut m, mode, page, dx, b'Q', Some(0xff), 0).is_err());
            assert!(
                video::scroll(
                    &mut m,
                    mode,
                    page,
                    Rect::FULL_SCREEN,
                    0,
                    0xff,
                    ScrollDirection::Up
                )
                .is_err()
            );
            assert!(video::teletype(&mut m, mode, page, &mut dx, b'Q').is_err());
            assert_eq!(dx, cursor(24, 79));
        }
        assert_eq!(m, before);
    }

    #[test]
    fn invalid_cursors_fail_before_memory_or_cursor_changes_even_noops() {
        for bad in [0x0050, 0x00ff, 0x1900, 0x194f, 0xffff] {
            let mut m = memory();
            let before = m.clone();
            assert!(video::read_cell(&m, 3, 0, bad).is_err());
            for count in [0, 1, u16::MAX] {
                assert!(video::write_repeat(&mut m, 3, 0, bad, b'Q', Some(0xff), count).is_err());
            }
            for ch in [7, 8, 10, 13, b'Q'] {
                let mut dx = bad;
                assert!(video::teletype(&mut m, 3, 0, &mut dx, ch).is_err());
                assert_eq!(dx, bad);
            }
            assert_eq!(m, before);
        }
    }

    #[test]
    fn incomplete_screen_rejects_every_operation_atomically_including_controls() {
        // Earlier cells exist in the final fixture: complete-screen preflight
        // is required even when the requested operation touches only cell zero.
        for size in [
            0,
            VRAM_BASE - 1,
            VRAM_BASE,
            VRAM_BASE + 2,
            VRAM_BASE + SCREEN_BYTES - 1,
        ] {
            let mut m = GuestMemory::from_bytes(vec![0x55; size], A20::Enabled);
            let before = m.clone();
            assert!(video::initialize(&mut m).is_err());
            assert!(video::read_cell(&m, 3, 0, 0).is_err());
            for count in [0, 1, u16::MAX] {
                assert!(video::write_repeat(&mut m, 3, 0, 0, b'X', None, count).is_err());
            }
            for direction in [ScrollDirection::Up, ScrollDirection::Down] {
                assert!(
                    video::scroll(&mut m, 3, 0, Rect::FULL_SCREEN, 0, 0xaa, direction).is_err()
                );
            }
            for ch in [7, 8, 10, 13, b'Q'] {
                for start in [0, cursor(24, 79)] {
                    let mut dx = start;
                    assert!(video::teletype(&mut m, 3, 0, &mut dx, ch).is_err());
                    assert_eq!(dx, start);
                }
            }
            assert_eq!(m, before);
        }
    }

    #[test]
    fn exact_screen_boundary_succeeds_and_preserves_bda() {
        let mut m = GuestMemory::new(VRAM_BASE + SCREEN_BYTES, A20::Disabled);
        m.write16(SegmentedAddress::new(0x40, 0x50), 0x1234)
            .unwrap();
        video::initialize(&mut m).unwrap();
        let mut dx = cursor(24, 79);
        video::teletype(&mut m, 3, 0, &mut dx, b'X').unwrap();
        assert_eq!(dx, cursor(24, 0));
        assert_eq!(m.read16(SegmentedAddress::new(0x40, 0x50)).unwrap(), 0x1234);
        assert_eq!(video::read_cell(&m, 3, 0, cursor(23, 79)).unwrap(), 0x0758);
    }
}
