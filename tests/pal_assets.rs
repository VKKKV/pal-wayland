// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in tests read proprietary assets at runtime, never embed them in test ELF.
//! First verify identity with tools/verify_assets.py. Missing assets fail, not skip.
use pal_wayland::{
    mz::MzFile,
    semantics::{A20, GuestMemory, Registers, SegmentedAddress},
};
use std::{fs, path::PathBuf, process::Command};

fn pal_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/unpacked/PAL.raw.exe")
}

#[test]
#[ignore = "requires local PAL.raw.exe; run cargo test --test pal_assets -- --ignored"]
fn real_pal_header_and_every_relocation() {
    let data = fs::read(pal_path()).expect("local PAL.raw.exe required; see README");
    let mz = MzFile::parse(&data).expect("parse verified PAL MZ");
    assert_eq!(data.len(), 210128);
    assert_eq!(mz.header_bytes(), 9536);
    assert_eq!(mz.declared_file_bytes(), data.len());
    assert_eq!(mz.relocations().len(), 2376);
    assert_eq!((mz.header().cs, mz.header().ip), (0x23b4, 0x042c));
    assert_eq!(mz.entry_file_offset(), 0x264ac);
    assert_eq!(mz.header().min_extra_paragraphs, 327);

    // Independent raw-header relocation oracle. Preserve duplicate record order.
    let table = u16::from_le_bytes([data[24], data[25]]) as usize;
    let count = u16::from_le_bytes([data[6], data[7]]) as usize;
    let image_len = mz.image().len();
    let allocation = image_len.div_ceil(16) * 16 + 327 * 16;
    for base in [0x0000_u16, 0x1000, 0x7000] {
        let loaded = mz.load(base).expect("minimum image load");
        let mut expected = mz.image().to_vec();
        for i in 0..count {
            let record = &data[table + i * 4..table + i * 4 + 4];
            let offset = u16::from_le_bytes([record[0], record[1]]) as usize;
            let segment = u16::from_le_bytes([record[2], record[3]]) as usize;
            let target = segment * 16 + offset;
            let before = u16::from_le_bytes([expected[target], expected[target + 1]]) as u32;
            let after = ((before + u32::from(base)) % 65536) as u16;
            expected[target..target + 2].copy_from_slice(&after.to_le_bytes());
        }
        expected.resize(allocation, 0);
        assert_eq!(loaded.memory, expected, "image/extra memory at {base:#x}");
        assert_eq!(loaded.relocation_count, 2376);
        assert_eq!(loaded.cs, base.wrapping_add(0x23b4));
        assert_eq!(loaded.ip, 0x042c);
        assert_eq!(loaded.ss, base.wrapping_add(mz.header().ss));
        assert_eq!(loaded.sp, mz.header().sp);

        // Loader bytes are image-relative, GuestMemory is physical-address-zero.
        let physical_base = usize::from(base) * 16;
        let mut physical = vec![0; physical_base + loaded.memory.len()];
        physical[physical_base..].copy_from_slice(&loaded.memory);
        let mut memory = GuestMemory::from_bytes(physical, A20::Disabled);
        let initial = Registers {
            cs: loaded.cs,
            ip: loaded.ip,
            ss: loaded.ss,
            sp: loaded.sp,
            ..Registers::default()
        };
        assert_eq!(
            memory
                .read8(SegmentedAddress::new(initial.cs, initial.ip))
                .unwrap(),
            loaded.memory[mz.entry_image_offset()]
        );
        // Exercise stack storage only; this is NOT a call into guest game code.
        let mut regs = initial;
        regs.far_call(
            &mut memory,
            SegmentedAddress::new(initial.cs, initial.ip),
            initial.ip,
        )
        .unwrap();
        regs.far_return(&memory).unwrap();
        assert_eq!(regs, initial);
    }
    assert!(
        mz.load(0xf000).is_err(),
        "reject conventional-address-space overflow"
    );
}

#[test]
#[ignore = "requires local PAL.raw.exe; run cargo test --test pal_assets -- --ignored"]
fn real_pal_cli_from_another_working_directory() {
    let binary = env!("CARGO_BIN_EXE_pal-wayland-tool");
    let output = Command::new(binary)
        .arg("load-check")
        .arg(pal_path())
        .arg("0x1000")
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "relocations: 2376",
        "entry_file_offset: 0x264ac",
        "entry_loaded: 33b4:042c",
        "applied_relocations: 2376",
    ] {
        assert!(
            stdout.lines().any(|line| line == expected),
            "missing {expected}"
        );
    }
    let failed = Command::new(binary)
        .arg("load-check")
        .arg(pal_path())
        .arg("0xf000")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(!failed.stderr.is_empty());
}
