// SPDX-License-Identifier: GPL-3.0-or-later
use pal_wayland::{
    runtime::Machine,
    semantics::{Flags, SegmentedAddress as A},
};
use std::time::Duration;
fn machine() -> Machine {
    let mut b = vec![0; 96];
    for (i, w) in [0x5a4d_u16, 96, 1, 0, 2, 0, 0xffff, 0, 64, 0, 0, 0, 28, 0]
        .into_iter()
        .enumerate()
    {
        b[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    Machine::from_mz(&b, 0x1010).unwrap()
}
#[test]
fn explicit_cache_is_bounded_and_ports_cannot_freeze_vblank() {
    let mut m = machine();
    m.start_audio_timeline();
    let first = m.begin_execution_batch();
    std::thread::sleep(Duration::from_millis(2));
    assert_eq!(m.elapsed_ns(), first);
    for _ in 0..32 {
        assert!(!m.poll_timer_irq().unwrap());
    }
    assert_eq!(m.elapsed_ns(), first);
    assert!(!m.poll_timer_irq().unwrap());
    assert!(m.elapsed_ns() > first);
    let sampled = m.elapsed_ns();
    std::thread::sleep(Duration::from_millis(1));
    m.port_in(0x3da, 8).unwrap();
    assert!(m.elapsed_ns() > sampled);
    // A guest can spin exclusively on the status port; it must see both phases.
    let deadline = std::time::Instant::now() + Duration::from_millis(40);
    let mut phases = 0;
    while phases != 3 && std::time::Instant::now() < deadline {
        phases |= if m.port_in(0x3da, 8).unwrap() & 8 == 0 {
            1
        } else {
            2
        };
    }
    assert_eq!(phases, 3);
    m.end_execution_batch();
    let fresh = m.elapsed_ns();
    std::thread::sleep(Duration::from_millis(1));
    assert!(m.elapsed_ns() > fresh);
}
#[test]
fn virtual_clock_override_irq_shadow_and_priority_are_not_batch_counted() {
    let mut m = machine();
    m.virtual_elapsed_ns = Some(0);
    m.enable_irqs().unwrap();
    m.begin_execution_batch();
    m.irq_shadow = 1;
    m.virtual_elapsed_ns = Some(60_000_000);
    assert!(!m.poll_timer_irq().unwrap());
    assert_eq!(m.irq_shadow, 0);
    assert!(m.poll_timer_irq().unwrap());
    assert_eq!(m.irq_deliveries, 1);
    assert_eq!(m.regs.cs, 0xf000);
    assert_eq!(m.regs.ip, 0x200);
    // IF and ISR must still prevent reentry on every boundary.
    m.regs.flags.set(Flags::IF, true);
    m.virtual_elapsed_ns = Some(120_000_000);
    assert!(!m.poll_timer_irq().unwrap());
    m.bios_timer_return().unwrap();
    assert_eq!(m.read32(A::new(0x40, 0x6c)).unwrap(), 1);
    assert!(m.poll_timer_irq().unwrap());
}
#[test]
fn delayed_timer_delivery_preserves_periodic_phase_and_coalesces() {
    let mut m = machine();
    m.virtual_elapsed_ns = Some(0);
    m.enable_irqs().unwrap();
    m.virtual_elapsed_ns = Some(60_000_000);
    assert!(m.poll_timer_irq().unwrap());
    m.bios_timer_return().unwrap();
    m.virtual_elapsed_ns = Some(110_000_000);
    assert!(m.poll_timer_irq().unwrap());
    m.bios_timer_return().unwrap();
    // Long stalls produce one pending edge, not a burst of catch-up IRQs;
    // the next edge remains on the original cadence rather than now+period.
    m.virtual_elapsed_ns = Some(300_000_000);
    assert!(m.poll_timer_irq().unwrap());
    m.bios_timer_return().unwrap();
    assert!(!m.poll_timer_irq().unwrap());
    m.virtual_elapsed_ns = Some(330_000_000);
    assert!(m.poll_timer_irq().unwrap());
    m.bios_timer_return().unwrap();
    assert_eq!(m.read32(A::new(0x40, 0x6c)).unwrap(), 4);
}
#[test]
fn full_guard_checks_last_byte_at_stack_and_long_macro_lengths() {
    let mut m = machine();
    for len in [1, 15, 16, 64] {
        let expected = vec![0x90; len];
        m.write_bytes(A::new(m.regs.cs, m.regs.ip), &expected)
            .unwrap();
        m.guard_code(&expected).unwrap();
        m.write8(A::new(m.regs.cs, m.regs.ip + len as u16 - 1), 0xcc)
            .unwrap();
        assert!(
            m.guard_code(&expected)
                .unwrap_err()
                .to_string()
                .contains("code changed")
        );
    }
}
#[test]
fn resetting_startup_invalidates_cached_baseline() {
    let mut m = machine();
    m.begin_execution_batch();
    std::thread::sleep(Duration::from_millis(1));
    m.refresh_execution_clock();
    assert!(m.elapsed_ns() > 0);
    m.start_audio_timeline();
    assert_eq!(m.elapsed_ns(), 0);
    m.end_execution_batch();
}
