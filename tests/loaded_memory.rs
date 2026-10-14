// SPDX-License-Identifier: GPL-3.0-or-later
//! Author-created MZ fixture connecting image-relative loader output to physical memory.
use pal_wayland::{
    mz::MzFile,
    semantics::{A20, GuestMemory, Registers, SegmentedAddress},
};

#[test]
fn loaded_image_is_placed_at_its_physical_base_before_using_registers() {
    // 32-byte header, 64-byte image, one relocation of a data pointer at 0:2.
    let mut data = vec![0; 96];
    let words = [
        0x5a4d_u16, 96, 1, 1, 2, 0, 0xffff, 0, 64, 0, 4, 0, 28, 0, 2, 0,
    ];
    for (i, value) in words.into_iter().enumerate() {
        data[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }
    data[34..36].copy_from_slice(&1_u16.to_le_bytes());
    data[36] = 0xa5; // Arbitrary data marker, not code or a game byte fixture.
    let mz = MzFile::parse(&data).unwrap();
    for segment in [0_u16, 0x1000, 0xfffc] {
        let loaded = mz.load(segment).unwrap();
        let base = usize::from(segment) * 16;
        let mut physical = vec![0; base + loaded.memory.len()];
        physical[base..].copy_from_slice(&loaded.memory);
        for a20 in [A20::Disabled, A20::Enabled] {
            let mut memory = GuestMemory::from_bytes(physical.clone(), a20);
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
                0xa5
            );
            assert_eq!(
                memory.read16(SegmentedAddress::new(segment, 2)).unwrap(),
                segment + 1
            );
            let mut regs = initial;
            regs.far_call(&mut memory, SegmentedAddress::new(segment, 0x10), 6)
                .unwrap();
            assert_eq!(
                &memory.as_bytes()[base + 60..base + 62],
                &6_u16.to_le_bytes()
            );
            assert_eq!(
                &memory.as_bytes()[base + 62..base + 64],
                &segment.to_le_bytes()
            );
            regs.far_return(&memory).unwrap();
            assert_eq!(regs, Registers { ip: 6, ..initial });
            assert_eq!(&memory.as_bytes()[..base], &physical[..base]);
            assert_eq!(&memory.as_bytes()[base..base + 60], &loaded.memory[..60]);
        }
    }
}
