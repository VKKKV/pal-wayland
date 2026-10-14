// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic hostile-input smoke tests, not a replacement for coverage fuzzing.
use pal_wayland::mz::MzFile;

fn exercise(data: &[u8]) {
    if let Ok(mz) = MzFile::parse(data) {
        assert!(mz.header_bytes() >= 28);
        assert!(mz.declared_file_bytes() <= data.len());
        assert_eq!(
            mz.image().len(),
            mz.declared_file_bytes() - mz.header_bytes()
        );
        assert!(mz.entry_image_offset() < mz.image().len());
        for relocation in mz.relocations() {
            assert!(relocation.image_offset() + 1 < mz.image().len());
        }
        for base in [0, 0x1000, 0x7fff, 0xf000, 0xffff] {
            if let Ok(loaded) = mz.load(base) {
                assert!(loaded.memory.len() >= mz.image().len());
                assert!(usize::from(base) * 16 + loaded.memory.len() <= 0x100000);
                assert_eq!(loaded.relocation_count, mz.relocations().len());
            }
        }
    }
}

#[test]
fn every_short_header_and_random_byte_buffer_is_safe() {
    let mut state = 0x41c6_4e6d_u32;
    for length in 0..512 {
        let mut data = vec![0; length];
        for byte in &mut data {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 24) as u8;
        }
        exercise(&data);
        if length >= 2 {
            data[..2].copy_from_slice(b"MZ");
            exercise(&data);
        }
    }
}

#[test]
fn mutate_each_header_field_and_relocation_record() {
    // Authored fixture: 64-byte header, 448-byte image, one relocation at 0:2.
    let mut valid = vec![0; 512];
    let words = [
        0x5a4d_u16, 0, 1, 1, 4, 0, 0xffff, 0, 448, 0, 0, 0, 28, 0, 2, 0,
    ];
    for (i, word) in words.into_iter().enumerate() {
        valid[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    let parsed = MzFile::parse(&valid).expect("valid authored fixture");
    assert!(parsed.load(0x1000).is_ok());
    for offset in (0..64).step_by(2) {
        for replacement in [
            0_u16, 1, 2, 15, 16, 27, 28, 31, 32, 63, 64, 255, 511, 512, 513, 0x7fff, 0xffff,
        ] {
            let mut mutated = valid.clone();
            mutated[offset..offset + 2].copy_from_slice(&replacement.to_le_bytes());
            exercise(&mutated);
        }
    }
    for length in 0..valid.len() {
        exercise(&valid[..length]);
    }
}
