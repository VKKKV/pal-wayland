// SPDX-License-Identifier: GPL-3.0-or-later
//! Default DOS INT 21h ABI: register/memory marshalling and guest service semantics.
//! The parent retains IVT dispatch; installed guest handlers never enter here.
//! DosFiles owns the mounted namespace, handles and filesystem operations.
use super::{Fault, Machine};
use crate::semantics::{Flags, SegmentedAddress};

// DOS geometry is 64 * 512 bytes per cluster. Widen before multiplication.
fn dos_capacity(blocks: u64, available: u64, fragment_size: u64) -> (u16, u16) {
    let clusters = |n| {
        (u128::from(n) * u128::from(fragment_size) / (64 * 512)).min(u128::from(u16::MAX)) as u16
    };
    let total = clusters(blocks);
    (total, clusters(available).min(total))
}

#[cfg(test)]
#[test]
fn dos_capacity_uses_fragment_units_and_clamps_without_overflow() {
    assert_eq!(dos_capacity(800, 80, 4096), (100, 10));
    assert_eq!(dos_capacity(100, 200, 32768), (100, 100));
    assert_eq!(dos_capacity(1, 1, 32767), (0, 0));
    assert_eq!(dos_capacity(u64::MAX, u64::MAX, u64::MAX), (65535, 65535));
}

impl Machine {
    pub(super) fn success(&mut self) {
        self.regs.flags.set(Flags::CF, false);
    }
    pub(super) fn dos_error(&mut self, code: u16) {
        self.last_dos_error = code;
        self.regs.ax = code;
        self.regs.flags.set(Flags::CF, true);
    }
    fn dos_string(
        &self,
        address: SegmentedAddress,
        terminator: u8,
        limit: usize,
    ) -> Result<Vec<u8>, Fault> {
        let mut result = Vec::new();
        for i in 0..limit {
            let offset = usize::from(address.offset) + i;
            if offset > 0xffff {
                return Err(self.fault("DOS string crosses segment"));
            }
            let b = self.read8(SegmentedAddress::new(address.segment, offset as u16))?;
            if b == terminator {
                return Ok(result);
            }
            result.push(b);
        }
        Err(self.fault("unterminated DOS string"))
    }
    pub(super) fn dos(&mut self) -> Result<(), Fault> {
        match self.regs.ah() {
            0x00 => self.exit_code = Some(0),
            0x02 => {
                let byte = self.regs.dx as u8;
                if byte == 9 {
                    return Err(self.fault("DOS console tab expansion requires a cursor model"));
                }
                self.console.push(byte);
                self.regs.set_al(byte);
            }
            0x09 => {
                let bytes = self.dos_string(
                    SegmentedAddress::new(self.regs.ds, self.regs.dx),
                    b'$',
                    65536,
                )?;
                self.console.extend(bytes);
                self.regs.set_al(b'$');
            }
            0x06 if self.regs.dx as u8 != 0xff => {
                let value = self.regs.dx as u8;
                self.console.push(value);
                self.regs.set_al(value);
            }
            0x06 => {
                if let Some(value) = self.bios_keys.pop_front() {
                    self.regs.set_al(value as u8);
                    self.regs.flags.set(Flags::ZF, false);
                } else {
                    self.regs.set_al(0);
                    self.regs.flags.set(Flags::ZF, true);
                }
            }
            0x0c => {
                let function = self.regs.al();
                if matches!(function, 1 | 7 | 8 | 10) {
                    return Err(self.fault("DOS flush/read subfunction not implemented"));
                }
                // Flush DOS/BIOS type-ahead, not hardware set-1 edges or held
                // state. Invalid AL (including00) performs flush only.
                self.bios_keys.clear();
                if function == 6 {
                    self.regs.set_ah(6);
                    self.dos()?;
                }
            }
            0x19 => self.regs.set_al(2), // C: in the declared guest environment.
            0x25 => {
                let at = u16::from(self.regs.al()) * 4;
                self.write16(SegmentedAddress::new(0, at), self.regs.dx)?;
                self.write16(SegmentedAddress::new(0, at + 2), self.regs.ds)?;
            }
            0x2c => {
                // DOS civil time uses UTC here; fixtures may inject exact time.
                // No clock-derived gameplay timing is synthesized from CPU steps.
                let hundredths = match self.clock_centiseconds {
                    Some(time) => u64::from(time),
                    None => {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_err(|e| self.fault(e.to_string()))?
                            .as_millis() as u64
                            / 10
                    }
                } % 8_640_000;
                self.regs.cx =
                    (((hundredths / 360_000) as u16) << 8) | ((hundredths / 6000 % 60) as u16);
                self.regs.dx =
                    (((hundredths / 100 % 60) as u16) << 8) | ((hundredths % 100) as u16);
            }
            0x30 => {
                let query = self.regs.al();
                if query > 1 {
                    return Err(self.fault("unsupported DOS version query"));
                }
                self.regs.ax = 5;
                self.regs.bx = if query == 0 { 0xff00 } else { 0 };
                self.regs.cx = 0;
            }
            0x33 if self.regs.al() == 0 => {
                self.regs.dx = (self.regs.dx & 0xff00) | u16::from(self.ctrl_break);
            }
            0x33 if self.regs.al() == 1 && self.regs.dx as u8 <= 1 => {
                self.ctrl_break = self.regs.dx as u8 != 0;
            }
            0x36 => {
                // Only default C: and explicit C: expose save capacity; D: does not.
                if !matches!(self.regs.dx as u8, 0 | 3) {
                    self.regs.ax = 0xffff;
                    return Ok(());
                }
                let Some(path) = self.files.save_root() else {
                    self.regs.ax = 0xffff;
                    return Ok(());
                };
                // Query in-process using caller-available, not reserved-root blocks.
                // Preflight before register writes: host failures are atomic.
                let capacity = rustix::fs::statvfs(path).map_err(|e| {
                    self.fault(format!("could not query save filesystem capacity: {e}"))
                })?;
                let (total, available) =
                    dos_capacity(capacity.f_blocks, capacity.f_bavail, capacity.f_frsize);
                self.regs.ax = 64;
                self.regs.cx = 512;
                self.regs.dx = total;
                self.regs.bx = available;
            }
            0x35 => {
                let at = u16::from(self.regs.al()) * 4;
                self.regs.bx = self.read16(SegmentedAddress::new(0, at))?;
                self.regs.es = self.read16(SegmentedAddress::new(0, at + 2))?;
            }
            0x3c | 0x3d => {
                let create = self.regs.ah() == 0x3c;
                let mode = if create { 2 } else { self.regs.al() & 7 };
                let name =
                    self.dos_string(SegmentedAddress::new(self.regs.ds, self.regs.dx), 0, 260)?;
                match self.files.open(&name, mode, create) {
                    Ok(h) => {
                        self.regs.ax = h;
                        self.success();
                    }
                    Err(e) => self.dos_error(e),
                }
            }
            0x3e => match self.files.close(self.regs.bx) {
                Ok(()) => self.success(),
                Err(e) => self.dos_error(e),
            },
            0x3f => match self.files.peek(self.regs.bx, usize::from(self.regs.cx)) {
                Ok(data) => {
                    self.write_bytes(SegmentedAddress::new(self.regs.ds, self.regs.dx), &data)?;
                    self.files
                        .advance(self.regs.bx, data.len())
                        .map_err(|e| self.fault(format!("DOS file position error {e}")))?;
                    self.regs.ax = data.len() as u16;
                    self.success();
                }
                Err(e) => self.dos_error(e),
            },
            0x40 => {
                let mut data = vec![0; usize::from(self.regs.cx)];
                self.read_bytes(SegmentedAddress::new(self.regs.ds, self.regs.dx), &mut data)?;
                if [1, 2].contains(&self.regs.bx) {
                    self.console.extend(data);
                    self.regs.ax = self.regs.cx;
                    self.success();
                } else {
                    match self.files.write(self.regs.bx, &data) {
                        Ok(n) => {
                            self.regs.ax = n as u16;
                            self.success();
                        }
                        Err(e) => self.dos_error(e),
                    }
                }
            }
            0x42 => {
                let offset = ((u32::from(self.regs.cx) << 16) | u32::from(self.regs.dx)) as i32;
                match self.files.seek(self.regs.bx, offset, self.regs.al()) {
                    Ok(p) => {
                        self.regs.ax = p as u16;
                        self.regs.dx = (p >> 16) as u16;
                        self.success();
                    }
                    Err(e) => self.dos_error(e),
                }
            }
            0x44 if self.regs.al() == 0 => {
                if self.regs.bx < 5 {
                    self.regs.dx = 0x80d3;
                    self.success();
                } else if self.files.contains(self.regs.bx) {
                    self.regs.dx = 2;
                    self.success();
                } else {
                    self.dos_error(6);
                }
            }
            0x47 => {
                // Default C: and flat C:/D: aliases have only root, no host cwd.
                if !matches!(self.regs.dx as u8, 0 | 3 | 4) {
                    self.dos_error(15);
                    return Ok(());
                }
                self.write8(SegmentedAddress::new(self.regs.ds, self.regs.si), 0)?;
                self.regs.ax = 0x0100;
                self.success();
            }
            0x48 => {
                let request = self.regs.bx;
                let mut cursor = self.psp;
                let mut candidate = None;
                let mut largest = 0;
                for (&segment, &size) in &self.blocks {
                    let gap = segment.saturating_sub(cursor);
                    largest = largest.max(gap);
                    if gap >= request && candidate.is_none() {
                        candidate = Some(cursor);
                    }
                    cursor = segment + size;
                }
                let gap = self.conventional_top.saturating_sub(cursor);
                largest = largest.max(gap);
                if gap >= request && candidate.is_none() {
                    candidate = Some(cursor);
                }
                if let Some(segment) = candidate.filter(|_| request != 0) {
                    self.blocks.insert(segment, request);
                    self.regs.ax = segment;
                    self.success();
                } else {
                    self.dos_error(8);
                    self.regs.bx = largest;
                }
            }
            0x49 => {
                if self.regs.es != self.psp && self.blocks.remove(&self.regs.es).is_some() {
                    self.success();
                } else {
                    self.dos_error(9);
                }
            }
            0x4a => {
                let segment = self.regs.es;
                let request = self.regs.bx;
                if !self.blocks.contains_key(&segment) {
                    self.dos_error(9);
                    return Ok(());
                }
                let next = self
                    .blocks
                    .range((
                        std::ops::Bound::Excluded(segment),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(&s, _)| s)
                    .unwrap_or(self.conventional_top);
                let largest = next - segment;
                if request > 0 && request <= largest {
                    self.blocks.insert(segment, request);
                    self.success();
                } else {
                    // DOS 2.1–6.0 grows an insufficient expansion to the largest
                    // possible size even though CF is returned set (RBIL 214a).
                    if request > largest {
                        self.blocks.insert(segment, largest);
                    }
                    self.dos_error(8);
                    self.regs.bx = largest;
                }
            }
            0x4c => self.exit_code = Some(self.regs.al()),
            0x59 => {
                self.regs.ax = self.last_dos_error;
                self.regs.bx = 0x0801;
                self.regs.cx = 0x0200;
            }
            0x51 | 0x62 => self.regs.bx = self.psp,
            _ => return Err(self.fault(format!("unsupported DOS service AX={:04x}", self.regs.ax))),
        }
        Ok(())
    }
}
