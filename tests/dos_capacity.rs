// SPDX-License-Identifier: GPL-3.0-or-later
//! Authored DOS drive/mode callers; isolated filesystem, no assets or devices.
use pal_wayland::{
    runtime::Machine,
    semantics::{Flags, SegmentedAddress},
};
use std::{fs, path::PathBuf, process::Command};
fn machine() -> Machine {
    let mut b = vec![0; 544];
    for (i, w) in [
        0x5a4d_u16, 32, 2, 0, 2, 0x20, 0xffff, 0, 0x400, 0, 0, 0, 28, 0,
    ]
    .into_iter()
    .enumerate()
    {
        b[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    Machine::from_mz(&b, 0x1010).unwrap()
}
#[test]
fn dos36_queries_mounted_filesystem_without_path_or_process_and_failure_is_atomic() {
    const CHILD: &str = "PAL_DOS36_CAPACITY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for empty in [false, true] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "dos36_queries_mounted_filesystem_without_path_or_process_and_failure_is_atomic",
            ]).env(CHILD, "1").env_remove("PATH");
            // execvp may supply /bin:/usr/bin when PATH is absent. Also test
            // explicitly empty PATH to prove no external df can be resolved.
            if empty {
                command.env("PATH", "");
            }
            let out = command.output().unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        return;
    }
    assert!(std::env::var_os("PATH").is_none_or(|v| v.is_empty()));
    let root = PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR required"))
        .join(format!("pal-dos36-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let mut m = machine();
    m.mount_saves(&root).unwrap();
    // Independent oracle uses the OS fragment size and *user available* blocks.
    let stat = rustix::fs::statvfs(&root).unwrap();
    let total = (u128::from(stat.f_blocks) * u128::from(stat.f_frsize) / 32768).min(65535) as u16;
    let available = (u128::from(stat.f_bavail) * u128::from(stat.f_frsize) / 32768)
        .min(u128::from(total)) as u16;
    // DL is one-based; default and C: expose the save filesystem, not B:.
    for drive in [0, 3] {
        m.regs.ax = 0x3600;
        m.regs.bx = 0x1234;
        m.regs.cx = 0x5678;
        m.regs.dx = 0xab00 | drive;
        m.regs.flags = Flags::from_bits(0x0247);
        let flags = m.regs.flags;
        m.interrupt(0x21).unwrap();
        assert_eq!(
            (m.regs.ax, m.regs.bx, m.regs.cx, m.regs.dx),
            (64, available, 512, total)
        );
        assert_eq!(m.regs.flags, flags);
    }
    let buffer = SegmentedAddress::new(m.regs.ds, 0x200);
    m.memory.write_bytes(buffer, b"unchanged").unwrap();
    m.regs.ax = 0x4700;
    m.regs.dx = 2; // B: is not a declared namespace alias.
    m.regs.si = buffer.offset;
    m.regs.flags = Flags::from_bits(0x0246);
    let mut expected = m.regs;
    expected.ax = 0x000f;
    expected.flags.set(Flags::CF, true);
    let memory = m.memory.as_bytes().to_vec();
    m.interrupt(0x21).unwrap();
    assert_eq!(m.regs, expected, "invalid AH=47 drive");
    assert_eq!(m.memory.as_bytes(), memory);
    // Current-directory queries retain both flat C:/D: aliases and default C:.
    for drive in [0, 3, 4] {
        m.memory.write_bytes(buffer, b"unchanged").unwrap();
        m.regs.ax = 0x4700;
        m.regs.dx = 0xab00 | drive;
        m.regs.flags = Flags::from_bits(0x0247);
        let mut expected = m.regs;
        expected.ax = 0x0100; // DOS 5 success result.
        expected.flags.set(Flags::CF, false);
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs, expected);
        let mut output = [0; 9];
        m.memory.read_bytes(buffer, &mut output).unwrap();
        assert_eq!(&output, b"\0nchanged");
    }
    // Bit 2 belongs to access mode, not sharing/inheritance controls.
    fs::write(root.join("MODE.DAT"), b"authored").unwrap();
    m.memory.write_bytes(buffer, b"D:\\MODE.DAT\0").unwrap();
    m.regs.ax = 0x3d04;
    m.regs.dx = buffer.offset;
    m.regs.flags = Flags::from_bits(0x0246);
    let mut expected = m.regs;
    expected.ax = 12;
    expected.flags.set(Flags::CF, true);
    m.interrupt(0x21).unwrap();
    assert_eq!(m.regs, expected, "invalid AH=3D access mode");
    m.regs.ax = 0x3dc0; // Legal read-only with sharing/inheritance bits retained.
    m.interrupt(0x21).unwrap();
    assert!(!m.regs.flags.cf());
    assert_eq!(m.regs.ax, 5, "invalid mode must not allocate a handle");
    m.regs.bx = m.regs.ax;
    m.regs.ax = 0x3e00;
    m.interrupt(0x21).unwrap();
    assert!(!m.regs.flags.cf());
    fs::remove_file(root.join("MODE.DAT")).unwrap();
    // Invalid drives must be rejected before statvfs even when the mount vanished.
    fs::remove_dir(&root).unwrap();
    for drive in [2, 4] {
        // B: absent; D: CD/namespace alias has no save capacity.
        m.regs.ax = 0x3600;
        m.regs.dx = drive;
        m.regs.flags = Flags::from_bits(0x0247);
        let mut expected = m.regs;
        expected.ax = 0xffff;
        m.interrupt(0x21).unwrap();
        assert_eq!(m.regs, expected, "invalid AH=36 drive");
    }
    // A legal drive still queries the missing path and fails atomically.
    m.regs.ax = 0x3600;
    m.regs.dx = 3;
    let regs = m.regs;
    let memory = m.memory.as_bytes().to_vec();
    let err = m.interrupt(0x21).unwrap_err();
    assert!(
        err.to_string()
            .contains("could not query save filesystem capacity")
    );
    assert_eq!(m.regs, regs);
    assert_eq!(m.memory.as_bytes(), memory);
    let mut absent = machine();
    absent.regs.ax = 0x3600;
    let mut expected = absent.regs;
    expected.ax = 0xffff;
    absent.interrupt(0x21).unwrap();
    assert_eq!(absent.regs, expected);
}
