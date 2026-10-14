// SPDX-License-Identifier: GPL-3.0-or-later
//! XMS ABI and the production ISA physical bus; no VOC parser in the device.
use pal_wayland::{
    runtime::Machine,
    semantics::{A20, SegmentedAddress},
    sound_blaster::SoundBlaster,
};

fn machine() -> Machine {
    let mut mz = vec![0; 544];
    for (i, word) in [
        0x5a4du16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        mz[i * 2..i * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    let mut m = Machine::from_mz(&mz, 0x1010).unwrap();
    m.virtual_elapsed_ns = Some(0);
    m.retain_sfx_pcm = true;
    m.sound_blaster = Some(SoundBlaster::default());
    m
}
fn call(m: &mut Machine, function: u8, dx: u16) {
    m.regs.ax = u16::from(function) << 8;
    m.regs.dx = dx;
    m.xms_call().unwrap();
    assert_eq!(m.regs.ax, 1);
}
fn alloc(m: &mut Machine, kb: u16) -> u16 {
    call(m, 9, kb);
    m.regs.dx
}
fn lock(m: &mut Machine, handle: u16) -> u32 {
    call(m, 12, handle);
    (u32::from(m.regs.dx) << 16) | u32::from(m.regs.bx)
}
fn move_to_xms(m: &mut Machine, handle: u16, offset: u32, data: &[u8]) {
    assert!(data.len().is_multiple_of(2));
    m.memory.write_linear(0x5000, data).unwrap();
    let mut request = [0; 16];
    request[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
    request[6..10].copy_from_slice(&0x5000u32.to_le_bytes());
    request[10..12].copy_from_slice(&handle.to_le_bytes());
    request[12..].copy_from_slice(&offset.to_le_bytes());
    m.memory.write_linear(0x200, &request).unwrap();
    m.regs.ds = 0;
    m.regs.si = 0x200;
    call(m, 11, 0);
}
fn out(m: &mut Machine, port: u16, value: u8) {
    m.port_out(port, u16::from(value), 8).unwrap();
}
fn dma(m: &mut Machine, address: u32, count: u16, mode: u8) {
    for (p, v) in [
        (0xa, 5),
        (0xc, 0),
        (0xb, mode),
        (2, address as u8),
        (2, (address >> 8) as u8),
        (3, count as u8),
        (3, (count >> 8) as u8),
        (0x83, (address >> 16) as u8),
        (0xa, 1),
    ] {
        out(m, p, v);
    }
}
fn dsp(m: &mut Machine, bytes: &[u8]) {
    for &v in bytes {
        out(m, 0x22c, v);
    }
}
fn frames(m: &mut Machine, count: usize) -> Vec<[i16; 2]> {
    let mut result = vec![[0; 2]; count];
    assert_eq!(
        m.sound_blaster.as_mut().unwrap().drain_frames(&mut result),
        count
    );
    result
}

#[test]
fn dx_bx_lock_and_original_pcm_prefix_from_real_backing_reaches_runtime_irq() {
    let mut m = machine();
    // Reproduce physical placement from the original VOC98 snapshot review.
    let _padding = alloc(&mut m, 1010);
    let h = alloc(&mut m, 512);
    assert_eq!(lock(&mut m, h), 0x1fc800);
    assert_eq!((m.regs.dx, m.regs.bx), (0x001f, 0xc800));
    assert_eq!(m.memory.a20(), A20::Disabled);
    // Authored PCM only: do not embed copyrighted game samples in public tests.
    // Preserve the independently observed physical placement, not resource bytes.
    let pcm: [u8; 32] = std::array::from_fn(|i| ((i * 73 + 19) & 255) as u8);
    move_to_xms(&mut m, h, 0x71fdc + 32, &pcm);
    assert_eq!(
        m.xms.checked_physical_span(0x26e7fc, pcm.len()).unwrap(),
        pcm
    );
    dma(&mut m, 0x26e7fc, 31, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 31, 0]); // 100 us per source byte
    // Toggle CPU A20 mid-transfer: ISA DMA keeps the same physical source.
    m.advance_sound_to(100_000).unwrap();
    m.memory.set_a20(A20::Enabled);
    m.virtual_elapsed_ns = Some(3_200_000);
    assert!(!m.poll_timer_irq().unwrap()); // IRQ delivery off, device still completes
    let status = m.sound_blaster.as_ref().unwrap().status();
    assert_eq!((status.consumed_bytes, status.completed_transfers), (32, 1));
    assert_eq!(status.last_completion_ns, Some(3_200_000));
    assert!(status.irq_pending);
    let actual = frames(&mut m, 141);
    for (index, frame) in actual.iter().enumerate() {
        let at = ((index as u64 + 1) * 1_000_000_000).div_ceil(44100);
        let expected = (i16::from(pcm[(at / 100_000) as usize]) - 128) * 256;
        assert_eq!(*frame, [expected; 2], "output frame {index}");
    }
    assert_eq!(m.port_in(0x22e, 8).unwrap(), 0);
    assert!(!m.sound_blaster.as_ref().unwrap().irq_pending());
}

#[test]
fn source_move_after_start_is_observed_without_a_ram_mirror() {
    let mut m = machine();
    let h = alloc(&mut m, 1);
    let address = lock(&mut m, h);
    move_to_xms(&mut m, h, 0, &[128, 128]);
    dma(&mut m, address, 1, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 1, 0]);
    m.advance_sound_to(50_000).unwrap();
    move_to_xms(&mut m, h, 0, &[128, 255]);
    m.advance_sound_to(150_000).unwrap();
    let actual = frames(&mut m, 6);
    assert_eq!(&actual[..4], &[[0; 2]; 4]);
    assert_eq!(&actual[4..], &[[32512; 2]; 2]);
}

#[test]
fn full_boundary_preflight_late_unlock_and_port_error_are_atomic() {
    let mut m = machine();
    let _pad = alloc(&mut m, 1);
    let h = alloc(&mut m, 1);
    let address = lock(&mut m, h);
    assert!(m.xms.checked_physical_span(address + 1022, 4).is_err());
    dma(&mut m, address + 1022, 3, 0x49);
    dsp(&mut m, &[0x14, 3]);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    assert!(m.port_out(0x22c, 0, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);
    // Final parameter remains uncommitted; reset then start a valid transfer.
    out(&mut m, 0x226, 1);
    out(&mut m, 0x226, 0);
    move_to_xms(&mut m, h, 0, &[0, 255, 128, 128]);
    dma(&mut m, address, 3, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 3, 0]);
    m.advance_sound_to(50_000).unwrap();
    call(&mut m, 13, h);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    let memory = m.memory.clone();
    m.retain_sfx_pcm = false; // Error must precede even muted queue draining.
    assert!(m.advance_sound_to(40_000_000).is_err());
    assert!(m.port_out(0x22c, 0xd3, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);
    assert_eq!(m.memory, memory);
    assert_eq!(m.sfx_discarded_frames, 0);
    assert_eq!(lock(&mut m, h), address);
    m.retain_sfx_pcm = true;
    m.advance_sound_to(400_000).unwrap();
    assert!(m.sound_blaster.as_ref().unwrap().irq_pending());
}

#[test]
fn fixed_page_wrap_checks_both_allocations_and_reads_same_locked_block() {
    let mut m = machine();
    let _unlocked_padding = alloc(&mut m, 63);
    let h = alloc(&mut m, 1);
    assert_eq!(lock(&mut m, h), 0x10fc00);
    dma(&mut m, 0x10ffff, 2, 0x49);
    dsp(&mut m, &[0x14, 2]);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    // First byte is valid, but wrap to 100000 hits an unlocked allocation.
    assert!(m.port_out(0x22c, 0, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);

    let mut m = machine();
    let h = alloc(&mut m, 128);
    assert_eq!(lock(&mut m, h), 0x100000);
    move_to_xms(&mut m, h, 0xfffe, &[128, 0]);
    move_to_xms(&mut m, h, 0, &[128, 255]);
    dma(&mut m, 0x10ffff, 2, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 2, 0]);
    m.advance_sound_to(300_000).unwrap();
    assert!(m.sound_blaster.as_ref().unwrap().irq_pending());
    assert_eq!(m.sound_blaster.as_ref().unwrap().status().dma_address, 2);
    assert_eq!(
        frames(&mut m, 13),
        [
            [-32768; 2],
            [-32768; 2],
            [-32768; 2],
            [-32768; 2],
            [0; 2],
            [0; 2],
            [0; 2],
            [0; 2],
            [32512; 2],
            [32512; 2],
            [32512; 2],
            [32512; 2],
            [32512; 2]
        ]
    );
}

#[test]
fn identification_pending_write_and_extended_rejection_use_same_provider() {
    let mut m = machine();
    dma(&mut m, 0x5000, 0, 0x45);
    out(&mut m, 0xa, 5);
    dsp(&mut m, &[0xe2, 0x34]);
    assert_eq!(m.memory.read_linear(0x5000, 1).unwrap(), [0]);
    out(&mut m, 0xa, 1);
    assert_eq!(m.memory.read_linear(0x5000, 1).unwrap(), [0x4c]);
    assert!(m.sound_blaster.as_ref().unwrap().status().dma_terminal);
    let h = alloc(&mut m, 1);
    let address = lock(&mut m, h);
    dma(&mut m, address, 0, 0x45);
    dsp(&mut m, &[0xe2]);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    let memory = m.memory.clone();
    assert!(m.port_out(0x22c, 0x34, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);
    assert_eq!(m.memory, memory);
    assert_eq!(m.xms.read_physical_byte(address).unwrap(), 0);
}

#[test]
fn resolver_rejects_unlocked_freed_gap_and_overflow_without_aliases() {
    let mut m = machine();
    let _padding = alloc(&mut m, 2049);
    let h = alloc(&mut m, 1);
    let address = 0x300400;
    assert!(m.xms.read_physical_byte(address).is_err());
    assert_eq!(lock(&mut m, h), address);
    move_to_xms(&mut m, h, 0, &[255, 0]);
    assert_eq!(m.xms.read_physical_byte(address).unwrap(), 255);
    assert!(m.xms.checked_physical_span(u32::MAX, 2).is_err());
    call(&mut m, 13, h);
    assert!(m.xms.read_physical_byte(address).is_err());
    // XMS 2.00 AH=0Bh distinguishes handle, offset and length errors.
    // Validate via the Machine caller; rejection preserves RAM and all XMS
    // backing, including the destination and the unlocked source allocation.
    move_to_xms(&mut m, h, 1022, &[0x34, 0x12]);
    m.memory.set_a20(A20::Enabled);
    let dest = alloc(&mut m, 1);
    let dest_address = lock(&mut m, dest);
    move_to_xms(&mut m, dest, 0, &[0x5a, 0xa5]);
    for mode in [0x12, 0x13] {
        m.video_mode = mode;
        for (source, source_offset, destination, dest_offset, length, error) in [
            (0xffff, 0, dest, 0, 2, 0xa3),
            (h, 2048, dest, 0, 2, 0xa4),
            (h, 1024, dest, 0, 2, 0xa4),
            (h, 1022, dest, 0, 4, 0xa7),
            (h, 0, 0xffff, 0, 2, 0xa5),
            (h, 0, dest, 2048, 2, 0xa6),
            (h, 0, dest, 1022, 4, 0xa7),
            (h, 0, dest, 0, 3, 0xa7),
        ] {
            let mut request = [0; 16];
            request[..4].copy_from_slice(&u32::to_le_bytes(length));
            request[4..6].copy_from_slice(&u16::to_le_bytes(source));
            request[6..10].copy_from_slice(&u32::to_le_bytes(source_offset));
            request[10..12].copy_from_slice(&u16::to_le_bytes(destination));
            request[12..].copy_from_slice(&u32::to_le_bytes(dest_offset));
            m.memory.write_linear(0x200, &request).unwrap();
            m.regs.ax = 0xb00;
            let memory = m.memory.clone();
            let mut regs = m.regs;
            let planar = m.planar.borrow().clone();
            let xms = format!("{:?}", m.xms);
            m.xms_call().unwrap();
            assert_eq!((m.regs.ax, m.regs.bx), (0, error), "mode {mode:02x}");
            regs.ax = 0;
            regs.bx = error;
            assert_eq!(m.regs, regs);
            assert_eq!(m.memory, memory);
            assert_eq!(*m.planar.borrow(), planar);
            assert_eq!(format!("{:?}", m.xms), xms);
        }
        // Exact source end is valid; both handle-to-handle and conventional
        // destinations retain the same little-endian bytes.
        for (destination, dest_offset) in [(dest, 1022), (0, 0x5000)] {
            let mut request = [0; 16];
            request[..4].copy_from_slice(&2u32.to_le_bytes());
            request[4..6].copy_from_slice(&h.to_le_bytes());
            request[6..10].copy_from_slice(&1022u32.to_le_bytes());
            request[10..12].copy_from_slice(&u16::to_le_bytes(destination));
            request[12..].copy_from_slice(&u32::to_le_bytes(dest_offset));
            m.memory.write_linear(0x200, &request).unwrap();
            call(&mut m, 11, 0);
            assert_eq!(m.regs.bx, 0);
        }
        assert_eq!(
            m.xms.checked_physical_span(dest_address + 1022, 2).unwrap(),
            [0x34, 0x12]
        );
        assert_eq!(m.memory.read_linear(0x5000, 2).unwrap(), [0x34, 0x12]);
    }
    call(&mut m, 10, h);
    assert!(m.xms.read_physical_byte(address).is_err());
    // Clearing CPU A20 bit would alias this address into the still-allocated
    // padding block. The production bus must reject, not read that alias.
    dma(&mut m, address, 0, 0x49);
    dsp(&mut m, &[0x14, 0]);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    assert!(m.port_out(0x22c, 0, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);
}

#[test]
fn production_bus_rejects_a20_alias_and_preserves_conventional_e2_and_pcm() {
    let mut m = machine();
    m.memory.write_linear(0, &[255, 255]).unwrap();
    dma(&mut m, 0x100000, 1, 0x49);
    dsp(&mut m, &[0x14, 1]);
    let before = format!("{:?}", m.sound_blaster.as_ref().unwrap());
    assert!(m.port_out(0x22c, 0, 8).is_err());
    assert_eq!(format!("{:?}", m.sound_blaster.as_ref().unwrap()), before);
    out(&mut m, 0x226, 1);
    out(&mut m, 0x226, 0);
    dma(&mut m, 0x5000, 1, 0x45);
    dsp(&mut m, &[0xe2, 0x34, 0xe2, 0x12]);
    assert_eq!(m.memory.read_linear(0x5000, 2).unwrap(), [0x4c, 3]);
    m.memory.write_linear(0x5000, &[0, 255]).unwrap();
    dma(&mut m, 0x5000, 1, 0x49);
    dsp(&mut m, &[0xd1, 0x40, 156, 0x14, 1, 0]);
    m.advance_sound_to(200_000).unwrap();
    assert!(m.sound_blaster.as_ref().unwrap().irq_pending());
    assert_eq!(
        frames(&mut m, 8),
        [
            [-32768; 2],
            [-32768; 2],
            [-32768; 2],
            [-32768; 2],
            [32512; 2],
            [32512; 2],
            [32512; 2],
            [32512; 2]
        ]
    );
    // GuestMemory remains the original 1 MiB, not a second XMS backing.
    assert_eq!(m.memory.len(), 0x100000);
    assert_eq!(m.memory.read8(SegmentedAddress::new(0, 0)).unwrap(), 255);
}
