// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded XMS host memory service, not an instruction execution fallback.
use crate::{
    runtime::Fault,
    semantics::{GuestMemory, Registers, SegmentedAddress},
};
use std::{collections::BTreeMap, ops::Range};

/// No-I/O linear validation. Keep XMS conventional transfers linear even when
/// they cross a 64-KiB offset; disabled A20 clears bit 20 on every byte.
fn ram_linear_span(
    m: &GuestMemory,
    start: u32,
    len: usize,
    forbidden: &Range<u32>,
) -> Result<(), Fault> {
    if len > m.len() {
        return Err(Fault("XMS service span exceeds guest RAM size".into()));
    }
    for i in 0..len {
        let linear = start
            .checked_add(u32::try_from(i).map_err(|_| Fault("XMS linear span overflow".into()))?)
            .ok_or_else(|| Fault("XMS linear span overflow".into()))?;
        let physical = if m.a20() == crate::semantics::A20::Disabled {
            linear & !(1 << 20)
        } else {
            linear
        };
        if forbidden.contains(&physical) {
            return Err(Fault(format!(
                "XMS service span touches forbidden physical address {physical:#x}"
            )));
        }
        if physical as usize >= m.len() {
            return Err(Fault(format!(
                "XMS service span outside guest RAM at {physical:#x}"
            )));
        }
    }
    Ok(())
}
#[derive(Debug, Default)]
pub struct Xms {
    blocks: BTreeMap<u16, Vec<u8>>,
    addresses: BTreeMap<u16, u32>,
    locks: BTreeMap<u16, u8>,
}
impl Xms {
    /// Resolve an allocated physical span to the SAME backing used by AH=0Bh.
    /// CPU A20 gating is deliberately absent: ISA DMA supplies a physical address.
    /// Require a lock and containment in one allocation; never fall back to RAM
    /// for an unlocked block, an allocation gap, or a span crossing its boundary.
    pub fn checked_physical_span(&self, start: u32, len: usize) -> Result<&[u8], Fault> {
        let len32 = u32::try_from(len).map_err(|_| Fault("XMS physical span overflow".into()))?;
        let end = start
            .checked_add(len32)
            .ok_or_else(|| Fault("XMS physical span overflow".into()))?;
        for (&handle, &address) in &self.addresses {
            let block = &self.blocks[&handle];
            let block_end = address + block.len() as u32;
            if start >= address && start < block_end {
                if self.locks[&handle] == 0 {
                    return Err(Fault(format!(
                        "XMS physical DMA requires locked handle {handle}"
                    )));
                }
                if end > block_end {
                    return Err(Fault(format!(
                        "XMS physical DMA crosses handle {handle} boundary"
                    )));
                }
                let offset = (start - address) as usize;
                return Ok(&block[offset..offset + len]);
            }
        }
        Err(Fault(format!(
            "XMS physical DMA address {start:#x} is not allocated"
        )))
    }

    pub fn read_physical_byte(&self, address: u32) -> Result<u8, Fault> {
        Ok(self.checked_physical_span(address, 1)?[0])
    }

    fn holes(&self) -> Vec<(u32, u32)> {
        let mut used: Vec<_> = self
            .addresses
            .iter()
            .map(|(&id, &at)| (at, self.blocks[&id].len() as u32))
            .collect();
        used.sort_unstable();
        let mut at = 0x100000;
        let mut holes = Vec::new();
        for (start, len) in used {
            if start > at {
                holes.push((at, start - at));
            }
            at = start + len;
        }
        if at < 0x1100000 {
            holes.push((at, 0x1100000 - at));
        }
        holes
    }

    /// XMS 2.00 AH=0Bh: handle, starting offset and transfer length are
    /// distinct errors (A3/A4, A5/A6, A7). Validate without reading data so
    /// protocol rejection precedes a source copy or a destination write.
    fn move_span(
        &self,
        m: &GuestMemory,
        handle: u16,
        offset: u32,
        len: usize,
        handle_error: u16,
    ) -> Result<(), u16> {
        let offset_error = handle_error + 1;
        if handle == 0 {
            let start = SegmentedAddress::new((offset >> 16) as u16, offset as u16).linear();
            ram_linear_span(m, start, usize::from(len != 0), &(0..0)).map_err(|_| offset_error)?;
            ram_linear_span(m, start, len, &(0..0)).map_err(|_| 0xa7u16)?;
        } else {
            let block = self.blocks.get(&handle).ok_or(handle_error)?;
            let start = offset as usize;
            if start > block.len() || (start == block.len() && len != 0) {
                return Err(offset_error);
            }
            start
                .checked_add(len)
                .filter(|&end| end <= block.len())
                .ok_or(0xa7u16)?;
        }
        Ok(())
    }

    /// Validate all RAM spans of an XMS call without changing registers,
    /// blocks, RAM or device state. Call this immediately before [`Self::call`]
    /// for mode12 with physical `forbidden = 0xa0000..0xb0000`.
    /// Only AH=0Bh touches guest memory; its descriptor is segmented, but
    /// handle-zero source/destination spans use the existing linear/A20 policy.
    pub fn preflight_ram_call(
        &self,
        r: &Registers,
        m: &GuestMemory,
        forbidden: Range<u32>,
    ) -> Result<(), Fault> {
        if r.ah() != 11 {
            return Ok(());
        }
        let at = SegmentedAddress::new(r.ds, r.si);
        if usize::from(at.offset) > 0x10000 - 16 {
            return Err(Fault(format!(
                "XMS descriptor at {at} crosses segment boundary"
            )));
        }
        ram_linear_span(m, at.linear(), 16, &forbidden)?;
        let mut raw = [0; 16];
        m.read_bytes(at, &mut raw)?;
        let d = |i: usize| u32::from_le_bytes(raw[i..i + 4].try_into().unwrap());
        let w = |i: usize| u16::from_le_bytes(raw[i..i + 2].try_into().unwrap());
        let n = d(0) as usize;
        if !n.is_multiple_of(2) {
            return Ok(()); // call reports A7h before accessing either buffer
        }
        for (handle, pointer) in [(w(4), d(6)), (w(10), d(12))] {
            if handle == 0 {
                let start = SegmentedAddress::new((pointer >> 16) as u16, pointer as u16).linear();
                ram_linear_span(m, start, n, &forbidden)?;
            }
        }
        Ok(())
    }

    pub fn call(&mut self, r: &mut Registers, m: &mut GuestMemory) -> Result<(), Fault> {
        let free = 16384usize - self.blocks.values().map(|b| b.len() / 1024).sum::<usize>();
        match r.ah() {
            0 => {
                r.ax = 0x0300;
                r.bx = 0x0301;
                r.dx = 0;
            }
            8 => {
                r.ax = (self
                    .holes()
                    .iter()
                    .map(|(_, len)| len / 1024)
                    .max()
                    .unwrap_or(0)) as u16;
                r.dx = free as u16;
                r.bx = 0;
            }
            9 => {
                let n = r.dx as usize;
                if n == 0 || n > free {
                    r.ax = 0;
                    r.bx = 0xa0;
                } else if let Some(h) = (1..=u16::MAX).find(|h| !self.blocks.contains_key(h)) {
                    let mut address = 0x100000u32;
                    let mut occupied: Vec<_> = self
                        .addresses
                        .iter()
                        .map(|(&id, &at)| (at, self.blocks[&id].len() as u32))
                        .collect();
                    occupied.sort_unstable();
                    for (at, len) in occupied {
                        if address + n as u32 * 1024 <= at {
                            break;
                        }
                        address = at + len;
                    }
                    if address + n as u32 * 1024 > 0x1100000 {
                        r.ax = 0;
                        r.bx = 0xa0;
                        return Ok(());
                    }
                    self.addresses.insert(h, address);
                    self.locks.insert(h, 0);
                    self.blocks.insert(h, vec![0; n * 1024]);
                    r.ax = 1;
                    r.dx = h;
                    r.bx = 0;
                } else {
                    r.ax = 0;
                    r.bx = 0xa1;
                }
            }
            10 => {
                if self.locks.get(&r.dx).is_some_and(|&n| n != 0) {
                    r.ax = 0;
                    r.bx = 0xab;
                    return Ok(());
                }
                if self.blocks.remove(&r.dx).is_some() {
                    self.addresses.remove(&r.dx);
                    self.locks.remove(&r.dx);
                    r.ax = 1;
                    r.bx = 0;
                } else {
                    r.ax = 0;
                    r.bx = 0xa2;
                }
            }
            12 => {
                if let Some(&address) = self.addresses.get(&r.dx) {
                    let count = self.locks.get_mut(&r.dx).unwrap();
                    if *count == 255 {
                        r.ax = 0;
                        r.bx = 0xac;
                    } else {
                        *count += 1;
                        r.ax = 1;
                        // XMS 0Ch returns DX:BX (high:low), not BX:DX.
                        r.bx = address as u16;
                        r.dx = (address >> 16) as u16;
                    }
                } else {
                    r.ax = 0;
                    r.bx = 0xa2;
                }
            }
            13 => {
                if let Some(count) = self.locks.get_mut(&r.dx) {
                    if *count == 0 {
                        r.ax = 0;
                        r.bx = 0xaa;
                    } else {
                        *count -= 1;
                        r.ax = 1;
                        r.bx = 0;
                    }
                } else {
                    r.ax = 0;
                    r.bx = 0xa2;
                }
            }
            14 => {
                if let Some(block) = self.blocks.get(&r.dx) {
                    r.ax = 1;
                    r.bx = (u16::from(self.locks[&r.dx]) << 8)
                        | (255 - self.blocks.len().min(255)) as u16;
                    r.dx = (block.len() / 1024) as u16;
                } else {
                    r.ax = 0;
                    r.bx = 0xa2;
                }
            }
            11 => {
                let at = SegmentedAddress::new(r.ds, r.si);
                let mut raw = [0; 16];
                m.read_bytes(at, &mut raw)?;
                let d = |i: usize| u32::from_le_bytes(raw[i..i + 4].try_into().unwrap());
                let w = |i: usize| u16::from_le_bytes(raw[i..i + 2].try_into().unwrap());
                let n = d(0) as usize;
                let source = w(4);
                let so = d(6);
                let dest = w(10);
                let target = d(12);
                if !n.is_multiple_of(2) {
                    r.ax = 0;
                    r.bx = 0xa7;
                    return Ok(());
                }
                if let Err(error) = self
                    .move_span(m, source, so, n, 0xa3)
                    .and_then(|()| self.move_span(m, dest, target, n, 0xa5))
                {
                    r.ax = 0;
                    r.bx = error;
                    return Ok(());
                }
                // Both complete spans are valid. The temporary copy preserves
                // the existing overlap behavior and permits unlocked handles.
                let data = if source == 0 {
                    let at = SegmentedAddress::new((so >> 16) as u16, so as u16);
                    m.read_linear(at.linear(), n)?
                } else {
                    self.blocks[&source][so as usize..so as usize + n].to_vec()
                };
                if dest == 0 {
                    let at = SegmentedAddress::new((target >> 16) as u16, target as u16);
                    m.write_linear(at.linear(), &data)?;
                } else {
                    self.blocks.get_mut(&dest).unwrap()[target as usize..target as usize + n]
                        .copy_from_slice(&data);
                }
                r.ax = 1;
                r.bx = 0;
            }
            _ => return Err(Fault(format!("unsupported XMS function {:02x}", r.ah()))),
        }
        Ok(())
    }
}
impl crate::sound_blaster::DmaPhysicalMemory for Xms {
    fn validate_dma_span(&self, start: u32, len: usize) -> Result<(), Fault> {
        self.checked_physical_span(start, len).map(|_| ())
    }
    fn read_dma_byte(&self, address: u32) -> Result<u8, Fault> {
        self.read_physical_byte(address)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::A20;
    #[test]
    fn hma_move_preserves_ivt_and_fragmentation_query_matches_allocation() {
        let mut x = Xms::default();
        let mut r = Registers::default();
        let mut m = GuestMemory::new(0x110000, A20::Enabled);
        r.ax = 0x900;
        r.dx = 1;
        x.call(&mut r, &mut m).unwrap();
        x.blocks.get_mut(&1).unwrap()[..2].copy_from_slice(&[0xef, 0xbe]);
        let mut req = [0; 16];
        req[..4].copy_from_slice(&2u32.to_le_bytes());
        req[4..6].copy_from_slice(&1u16.to_le_bytes());
        req[12..].copy_from_slice(&0xffff0010u32.to_le_bytes());
        m.write_bytes(SegmentedAddress::new(0, 0x200), &req)
            .unwrap();
        r.ax = 0xb00;
        r.si = 0x200;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!(r.ax, 1);
        assert_eq!(
            m.read16(SegmentedAddress::new(0xffff, 0x10)).unwrap(),
            0xbeef
        );
        assert_eq!(m.read16(SegmentedAddress::new(0, 0)).unwrap(), 0);
        x = Xms::default();
        for n in [4096, 4096, 8192] {
            r.ax = 0x900;
            r.dx = n;
            x.call(&mut r, &mut m).unwrap();
            assert_eq!(r.ax, 1);
        }
        for h in [1, 3] {
            r.ax = 0xa00;
            r.dx = h;
            x.call(&mut r, &mut m).unwrap();
        }
        r.ax = 0x800;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!((r.ax, r.dx), (8192, 12288));
        r.dx = r.ax;
        r.ax = 0x900;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!(r.ax, 1);
        m.set_a20(A20::Disabled);
        m.write_linear(0xfffff, &[1, 2]).unwrap();
        assert_eq!(m.as_bytes()[0xfffff], 1);
        assert_eq!(m.as_bytes()[0], 2);
        let before = m.clone();
        assert!(m.write_linear(u32::MAX, &[3, 4]).is_err());
        assert_eq!(m, before);
    }
    #[test]
    fn allocate_query_move_free_and_errors() {
        let mut x = Xms::default();
        let mut r = Registers::default();
        let mut m = GuestMemory::new(0x100000, A20::Disabled);
        r.ax = 0x900;
        r.dx = 1;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!((r.ax, r.dx), (1, 1));
        m.write16(SegmentedAddress::new(0, 0x400), 0x1234).unwrap();
        let mut request = [0; 16];
        request[..4].copy_from_slice(&2u32.to_le_bytes());
        request[6..10].copy_from_slice(&0x400u32.to_le_bytes());
        request[10..12].copy_from_slice(&1u16.to_le_bytes());
        m.write_bytes(SegmentedAddress::new(0, 0x200), &request)
            .unwrap();
        r.ax = 0xb00;
        r.si = 0x200;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!(&x.blocks[&1][..2], &[0x34, 0x12]);
        r.ax = 0x800;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!(r.dx, 16383);
        r.ax = 0xa00;
        r.dx = 1;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!(r.ax, 1);
        r.ax = 0xa00;
        x.call(&mut r, &mut m).unwrap();
        assert_eq!((r.ax, r.bx), (0, 0xa2));
    }
}
