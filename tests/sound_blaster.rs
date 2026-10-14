// SPDX-License-Identifier: GPL-3.0-or-later
use pal_wayland::{semantics, sound_blaster::SoundBlaster};
use semantics::{A20, GuestMemory};
fn out(sb: &mut SoundBlaster, ram: &mut GuestMemory, port: u16, value: u8) {
    sb.write_port(port, value, ram).unwrap();
}
fn dsp(sb: &mut SoundBlaster, ram: &mut GuestMemory, values: &[u8]) {
    for &value in values {
        out(sb, ram, 0x22c, value);
    }
}
fn dma(sb: &mut SoundBlaster, ram: &mut GuestMemory, address: u32, count: u16, mode: u8) {
    out(sb, ram, 0x0a, 5);
    out(sb, ram, 0x0c, 0);
    out(sb, ram, 0x0b, mode);
    out(sb, ram, 0x02, address as u8);
    out(sb, ram, 0x02, (address >> 8) as u8);
    out(sb, ram, 0x03, count as u8);
    out(sb, ram, 0x03, (count >> 8) as u8);
    out(sb, ram, 0x83, (address >> 16) as u8);
    out(sb, ram, 0x0a, 1);
}

#[test]
fn authored_original_init_dma_identification_and_muted_irq7() {
    let mut ram = GuestMemory::new(0x100000, A20::Disabled);
    let mut sb = SoundBlaster::default();
    assert_eq!(sb.base(), 0x220);
    assert_eq!(sb.irq(), 7);
    assert!(!sb.handles_port(0x388));
    assert!(sb.handles_port(0x226));
    assert_eq!(sb.read_port(0x22c).unwrap(), 0);
    // Ordered ports/parameters from PAL 3b61:01ce..02f7, not emulator output.
    dsp(&mut sb, &mut ram, &[0xd3]);
    out(&mut sb, &mut ram, 0x226, 1);
    let asserted = sb.status();
    for _ in 0..4 {
        assert_eq!(sb.read_port(0x226).unwrap(), 0xff);
        assert_eq!(sb.status(), asserted);
    }
    out(&mut sb, &mut ram, 0x226, 0);
    assert_eq!(sb.read_port(0x226).unwrap(), 0xff);
    assert_eq!(sb.read_port(0x22e).unwrap(), 0x80);
    assert_eq!(sb.read_port(0x22a).unwrap(), 0xaa);
    dsp(&mut sb, &mut ram, &[0xe0, 0xaa]);
    assert_eq!(sb.read_port(0x22a).unwrap(), 0x55);
    // Original helper passes CX=1 without subtracting: two DMA bytes.
    let at = 0x3f480 + 0x1843;
    dma(&mut sb, &mut ram, at, 1, 0x45);
    dsp(&mut sb, &mut ram, &[0xe2, 0x34]);
    assert_eq!(ram.read_linear(at, 1).unwrap(), [0x4c]);
    dsp(&mut sb, &mut ram, &[0xe2, 0x12]);
    assert_eq!(ram.read_linear(at, 2).unwrap(), [0x4c, 0x03]);
    assert_eq!(sb.status().dma_count, 0xffff);
    assert_eq!(sb.read_port(0x08).unwrap(), 2);
    assert_eq!(sb.read_port(0x08).unwrap(), 0);
    out(&mut sb, &mut ram, 0x0c, 0);
    assert_eq!(sb.read_port(0x03).unwrap(), 0xff);
    assert_eq!(sb.read_port(0x03).unwrap(), 0xff);
    assert!(!sb.irq_pending()); // E2 terminal count is not a DSP PCM IRQ.
    dsp(&mut sb, &mut ram, &[0xe4, 0xaa, 0xe8]);
    assert_eq!(sb.read_port(0x22a).unwrap(), 0xaa);
    dma(&mut sb, &mut ram, 0x3f480 + 0x1859, 0, 0x49);
    dsp(&mut sb, &mut ram, &[0x40, 0x64, 0x14, 0, 0]);
    sb.advance_to(155_999, &mut ram).unwrap();
    assert_eq!(sb.status().consumed_bytes, 1);
    assert!(!sb.irq_pending());
    sb.advance_to(156_000, &mut ram).unwrap();
    assert!(sb.irq_pending());
    assert_eq!(sb.status().last_completion_ns, Some(156_000));
    assert_eq!(sb.status().completed_transfers, 1);
    assert_eq!(sb.read_port(0x22e).unwrap(), 0); // original ISR acknowledges.
    assert!(!sb.irq_pending());
    let mut pcm = [[1, 1]; 16];
    let n = sb.drain_frames(&mut pcm);
    assert!(n > 0);
    assert!(pcm[..n].iter().all(|&frame| frame == [0, 0]));
    dsp(&mut sb, &mut ram, &[0xe1]);
    assert_eq!(sb.read_port(0x22a).unwrap(), 2);
    assert_eq!(sb.read_port(0x22a).unwrap(), 1);
}

#[test]
fn pcm_wrap_pause_resume_real_position_and_original_stop() {
    let mut ram = GuestMemory::new(0x100000, A20::Enabled);
    ram.write_linear(0x1ffff, &[0]).unwrap();
    ram.write_linear(0x10000, &[128, 255]).unwrap();
    let mut sb = SoundBlaster::default();
    dma(&mut sb, &mut ram, 0x1ffff, 2, 0x49);
    dsp(&mut sb, &mut ram, &[0xd1, 0x40, 156, 0x14, 2, 0]); // 10kHz.
    sb.advance_to(99_999, &mut ram).unwrap();
    assert_eq!(sb.status().consumed_bytes, 1);
    assert_eq!(sb.status().dma_address, 0);
    assert_eq!(sb.status().dma_page, 1); // address wraps inside the DMA page.
    dsp(&mut sb, &mut ram, &[0xd0]);
    sb.advance_to(1_099_999, &mut ram).unwrap();
    assert_eq!(sb.status().remaining_bytes, 2);
    assert!(!sb.irq_pending());
    dsp(&mut sb, &mut ram, &[0xd4]);
    sb.advance_to(1_100_000, &mut ram).unwrap();
    assert_eq!(sb.status().consumed_bytes, 2);
    sb.advance_to(1_200_000, &mut ram).unwrap();
    assert_eq!(sb.status().consumed_bytes, 3);
    assert!(!sb.irq_pending());
    sb.advance_to(1_300_000, &mut ram).unwrap();
    assert_eq!(sb.status().last_completion_ns, Some(1_300_000));
    assert!(sb.irq_pending());
    let mut pcm = [[0, 0]; 128];
    let n = sb.drain_frames(&mut pcm);
    assert!(pcm[..n].contains(&[-32768, -32768]));
    assert!(pcm[..n].contains(&[32512, 32512]));
    assert!(pcm[..n].contains(&[0, 0]));
    assert!(pcm[..n].iter().all(|frame| frame[0] == frame[1]));
    sb.read_port(0x22e).unwrap();
    dma(&mut sb, &mut ram, 0x10000, 2, 0x49);
    dsp(&mut sb, &mut ram, &[0x14, 2, 0]);
    sb.advance_to(1_350_000, &mut ram).unwrap();
    dsp(&mut sb, &mut ram, &[0xd0]);
    out(&mut sb, &mut ram, 0x0a, 5); // PAL stop sequence, not completion.
    assert!(!sb.status().playing);
    assert_eq!(sb.status().completed_transfers, 1);
    dma(&mut sb, &mut ram, 0x10000, 0, 0x49);
    dsp(&mut sb, &mut ram, &[0x14, 0, 0]);
    sb.advance_to(1_450_000, &mut ram).unwrap();
    assert_eq!(sb.status().completed_transfers, 2);
}

#[test]
fn a20_checked_full_preflight_and_bounded_output() {
    let mut ram = GuestMemory::new(0x100000, A20::Disabled);
    ram.write_linear(0, &[255]).unwrap();
    let mut sb = SoundBlaster::default();
    dma(&mut sb, &mut ram, 0x100000, 1, 0x49);
    dsp(&mut sb, &mut ram, &[0xd1, 0x40, 156, 0x14, 1, 0]);
    sb.advance_to(50_000, &mut ram).unwrap();
    assert_eq!(sb.status().consumed_bytes, 1);
    ram.set_a20(A20::Enabled);
    let before = sb.status();
    let bytes = ram.as_bytes().to_vec();
    assert!(sb.advance_to(200_000, &mut ram).is_err());
    assert_eq!(sb.status(), before);
    assert_eq!(ram.as_bytes(), bytes);
    ram.set_a20(A20::Disabled);
    sb.advance_to(200_000, &mut ram).unwrap();
    assert!(sb.irq_pending());
    let before = sb.status();
    assert!(sb.advance_to(2_000_000_000, &mut ram).is_err());
    assert_eq!(sb.status(), before);
    assert!(sb.write_port(0x22c, 0x1c, &mut ram).is_err());
    let mut short = GuestMemory::new(0x10001, A20::Enabled);
    let mut sb = SoundBlaster::default();
    dma(&mut sb, &mut short, 0x10000, 1, 0x49);
    dsp(&mut sb, &mut short, &[0x14, 1]);
    let before = sb.status();
    assert!(sb.write_port(0x22c, 0, &mut short).is_err());
    assert_eq!(sb.status(), before);
    assert_eq!(sb.status().consumed_bytes, 0);
}
