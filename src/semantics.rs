// SPDX-License-Identifier: GPL-3.0-or-later
//! Small, safe machine-state primitives for future statically generated Rust.
//!
//! This is NOT a CPU decoder, interpreter, DOS implementation, or game runner.
//! Arithmetic implements 8/16-bit ADD/ADC/SUB/SBB/CMP; only CF/PF/AF/ZF/SF/OF
//! change. All other raw FLAGS bits survive, including reserved bits, IF and DF.
//!
//! Memory policy: a deliberately restricted real-mode, 16-bit-offset model,
//! NOT a claim of complete 8086/286 hardware compatibility. An effective offset
//! (including SP) is computed with u16 wrapping BEFORE an access. A word or
//! byte-slice access crossing offset 0xffff is rejected, rather than choosing
//! an uncertain CPU-specific segment-limit/bus-wrap behavior. For accepted
//! accesses, each byte's linear address is segment * 16 + offset, and A20 is
//! applied to EACH byte separately. Thus a word can cross physical 0xfffff
//! without crossing its 64-KiB segment. Enabled A20 preserves the real-mode
//! high-memory area; disabled A20 clears bit 20. No address is a host pointer.
//!
//! Public API: [`Flags`], add8/adc8/sub8/sbb8/cmp8 and their 16-bit counterparts;
//! [`Registers`] (including AX/AH/AL aliases and stack/far-transfer methods);
//! [`SegmentedAddress`], [`A20`], [`GuestMemory`], and [`MemoryError`]. All
//! fallible writes and stack/far transfers are atomic with respect to returned
//! errors, not with respect to concurrent hardware or host allocation failure.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;

/// Raw FLAGS storage. No reserved-bit normalization or CPU-specific reset value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    raw: u16,
}

impl Flags {
    pub const CF: u16 = 1 << 0; // Unsigned carry / subtraction borrow.
    pub const PF: u16 = 1 << 2; // Even parity of the LOW result byte.
    pub const AF: u16 = 1 << 4; // Carry / borrow at bit 4.
    pub const ZF: u16 = 1 << 6; // Entire result is zero.
    pub const SF: u16 = 1 << 7; // Most significant result bit.
    pub const TF: u16 = 1 << 8; // Trap; not changed by these primitives.
    pub const IF: u16 = 1 << 9; // Interrupt enable; preserved, not implemented.
    pub const DF: u16 = 1 << 10; // Direction; preserved.
    pub const OF: u16 = 1 << 11; // Signed overflow.
    pub const ARITHMETIC: u16 = Self::CF | Self::PF | Self::AF | Self::ZF | Self::SF | Self::OF;

    pub const fn from_bits(raw: u16) -> Self {
        Self { raw }
    }

    pub const fn bits(self) -> u16 {
        self.raw
    }

    /// Test a flag, or whether all bits of a mask are set.
    pub const fn contains(self, mask: u16) -> bool {
        self.raw & mask == mask
    }

    /// Explicitly set/clear a mask, without changing any other raw bits.
    pub fn set(&mut self, mask: u16, value: bool) {
        if value {
            self.raw |= mask;
        } else {
            self.raw &= !mask;
        }
    }

    pub const fn cf(self) -> bool {
        self.contains(Self::CF)
    }
    pub const fn pf(self) -> bool {
        self.contains(Self::PF)
    }
    pub const fn af(self) -> bool {
        self.contains(Self::AF)
    }
    pub const fn zf(self) -> bool {
        self.contains(Self::ZF)
    }
    pub const fn sf(self) -> bool {
        self.contains(Self::SF)
    }
    pub const fn of(self) -> bool {
        self.contains(Self::OF)
    }
}

// Only the public width-specific wrappers call this, with bits = 8 or 16.
fn arithmetic(a: u16, b: u16, carry: bool, bits: u32, subtract: bool, flags: &mut Flags) -> u16 {
    let a = u32::from(a);
    let b = u32::from(b);
    let c = u32::from(carry);
    let mask = (1_u32 << bits) - 1;
    let sign = 1_u32 << (bits - 1);
    let (wide, cf) = if subtract {
        (a.wrapping_sub(b).wrapping_sub(c), a < b + c)
    } else {
        let sum = a + b + c;
        (sum, sum > mask)
    };
    let result = wide & mask;
    let of = if subtract {
        ((a ^ b) & (a ^ result) & sign) != 0
    } else {
        (!(a ^ b) & (a ^ result) & sign) != 0
    };
    let mut updated = 0;
    for (value, bit) in [
        (cf, Flags::CF),
        ((result as u8).count_ones().is_multiple_of(2), Flags::PF),
        ((a ^ b ^ result) & 0x10 != 0, Flags::AF),
        (result == 0, Flags::ZF),
        (result & sign != 0, Flags::SF),
        (of, Flags::OF),
    ] {
        if value {
            updated |= bit;
        }
    }
    flags.raw = (flags.raw & !Flags::ARITHMETIC) | updated;
    result as u16
}

/// Wrapping byte addition; incoming CF is ignored.
pub fn add8(a: u8, b: u8, flags: &mut Flags) -> u8 {
    arithmetic(a.into(), b.into(), false, 8, false, flags) as u8
}

/// Wrapping byte addition including incoming CF.
pub fn adc8(a: u8, b: u8, flags: &mut Flags) -> u8 {
    arithmetic(a.into(), b.into(), flags.cf(), 8, false, flags) as u8
}

/// Wrapping byte subtraction; incoming CF is ignored.
pub fn sub8(a: u8, b: u8, flags: &mut Flags) -> u8 {
    arithmetic(a.into(), b.into(), false, 8, true, flags) as u8
}

/// Wrapping byte subtraction including incoming CF as a borrow.
pub fn sbb8(a: u8, b: u8, flags: &mut Flags) -> u8 {
    arithmetic(a.into(), b.into(), flags.cf(), 8, true, flags) as u8
}

/// Update flags exactly like SUB, without returning/storing a result.
pub fn cmp8(a: u8, b: u8, flags: &mut Flags) {
    sub8(a, b, flags);
}

/// Wrapping word addition; incoming CF is ignored.
pub fn add16(a: u16, b: u16, flags: &mut Flags) -> u16 {
    arithmetic(a, b, false, 16, false, flags)
}

/// Wrapping word addition including incoming CF.
pub fn adc16(a: u16, b: u16, flags: &mut Flags) -> u16 {
    arithmetic(a, b, flags.cf(), 16, false, flags)
}

/// Wrapping word subtraction; incoming CF is ignored.
pub fn sub16(a: u16, b: u16, flags: &mut Flags) -> u16 {
    arithmetic(a, b, false, 16, true, flags)
}

/// Wrapping word subtraction including incoming CF as a borrow.
pub fn sbb16(a: u16, b: u16, flags: &mut Flags) -> u16 {
    arithmetic(a, b, flags.cf(), 16, true, flags)
}

/// Update flags exactly like SUB, without returning/storing a result.
pub fn cmp16(a: u16, b: u16, flags: &mut Flags) {
    sub16(a, b, flags);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentedAddress {
    pub segment: u16,
    pub offset: u16,
}

impl SegmentedAddress {
    pub const fn new(segment: u16, offset: u16) -> Self {
        Self { segment, offset }
    }

    /// Before A20 gating: ranges from 0 through 0x10ffef inclusive.
    pub const fn linear(self) -> u32 {
        (self.segment as u32) * 16 + self.offset as u32
    }

    /// Wrap only the effective offset; never carry into the segment.
    pub const fn wrapping_add(self, displacement: u16) -> Self {
        Self::new(self.segment, self.offset.wrapping_add(displacement))
    }
}

impl fmt::Display for SegmentedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.segment, self.offset)
    }
}

/// A20 is an explicit input; there is intentionally no implicit default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum A20 {
    Disabled,
    Enabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryError {
    /// This model rejects a multi-byte access extending past offset 0xffff.
    SegmentBoundary {
        address: SegmentedAddress,
        len: usize,
    },
    /// Reports the first byte which is not backed by the guest buffer.
    OutOfBounds {
        address: SegmentedAddress,
        physical: usize,
        size: usize,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SegmentBoundary { address, len } => write!(
                f,
                "{len}-byte access at {address} crosses the 64-KiB segment boundary (unsupported)"
            ),
            Self::OutOfBounds {
                address,
                physical,
                size,
            } => write!(
                f,
                "guest byte at {address} maps to physical 0x{physical:x}, outside {size}-byte memory"
            ),
        }
    }
}

impl Error for MemoryError {}

/// Owned, bounded guest storage indexed from physical address zero.
/// A LoadedMz buffer is image-relative: place it at load_segment * 16 in this
/// storage, never pass the image alone to from_bytes with relocated registers.
/// No host-pointer conversion or MMIO behavior.
/// `new`/`from_bytes` use normal Vec allocation; allocation failure is not a
/// guest memory fault. Memory beyond 0x10ffef cannot be addressed by this model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestMemory {
    bytes: Vec<u8>,
    a20: A20,
}

impl GuestMemory {
    pub fn new(size: usize, a20: A20) -> Self {
        Self::from_bytes(vec![0; size], a20)
    }

    pub fn from_bytes(bytes: Vec<u8>, a20: A20) -> Self {
        Self { bytes, a20 }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    pub fn a20(&self) -> A20 {
        self.a20
    }

    /// Change address translation only; the backing bytes are not moved.
    pub fn set_a20(&mut self, a20: A20) {
        self.a20 = a20;
    }

    /// Translate only: this does NOT imply that the returned index is in bounds.
    pub fn physical_address(&self, address: SegmentedAddress) -> usize {
        let linear = address.linear();
        match self.a20 {
            A20::Disabled => (linear & !(1 << 20)) as usize,
            A20::Enabled => linear as usize,
        }
    }

    /// Linear DMA/XMS spans do not wrap a 16-bit offset. Apply A20 to every
    /// byte, then preflight the complete span; no physical-to-segment truncation.
    fn linear_span(&self, start: u32, len: usize) -> Result<Vec<usize>, MemoryError> {
        if len > self.len() {
            return Err(MemoryError::OutOfBounds {
                address: SegmentedAddress::new(0, 0),
                physical: len,
                size: self.len(),
            });
        }
        let mut indices = Vec::with_capacity(len);
        for i in 0..len {
            let linear = start
                .checked_add(i as u32)
                .ok_or(MemoryError::OutOfBounds {
                    address: SegmentedAddress::new(0, 0),
                    physical: usize::MAX,
                    size: self.len(),
                })?;
            let physical = if self.a20 == A20::Disabled {
                linear & !(1 << 20)
            } else {
                linear
            } as usize;
            if physical >= self.len() {
                return Err(MemoryError::OutOfBounds {
                    address: SegmentedAddress::new(0, 0),
                    physical,
                    size: self.len(),
                });
            }
            indices.push(physical);
        }
        Ok(indices)
    }
    pub fn read_linear(&self, start: u32, len: usize) -> Result<Vec<u8>, MemoryError> {
        Ok(self
            .linear_span(start, len)?
            .into_iter()
            .map(|i| self.bytes[i])
            .collect())
    }
    pub fn write_linear(&mut self, start: u32, data: &[u8]) -> Result<(), MemoryError> {
        let indices = self.linear_span(start, data.len())?;
        for (i, &b) in indices.into_iter().zip(data) {
            self.bytes[i] = b;
        }
        Ok(())
    }
    fn checked_byte(&self, address: SegmentedAddress) -> Result<usize, MemoryError> {
        let physical = self.physical_address(address);
        if physical >= self.len() {
            Err(MemoryError::OutOfBounds {
                address,
                physical,
                size: self.len(),
            })
        } else {
            Ok(physical)
        }
    }

    fn check_segment_span(address: SegmentedAddress, len: usize) -> Result<(), MemoryError> {
        // Subtract from the fixed limit instead of overflowing offset + len.
        if len > 0x1_0000 - usize::from(address.offset) {
            Err(MemoryError::SegmentBoundary { address, len })
        } else {
            Ok(())
        }
    }

    fn checked_word(&self, address: SegmentedAddress) -> Result<[usize; 2], MemoryError> {
        Self::check_segment_span(address, 2)?;
        Ok([
            self.checked_byte(address)?,
            self.checked_byte(SegmentedAddress::new(address.segment, address.offset + 1))?,
        ])
    }

    fn checked_span(&self, address: SegmentedAddress, len: usize) -> Result<(), MemoryError> {
        Self::check_segment_span(address, len)?;
        for i in 0..len {
            // The segment-span check proved offset + i <= 0xffff.
            self.checked_byte(SegmentedAddress::new(
                address.segment,
                address.offset + i as u16,
            ))?;
        }
        Ok(())
    }

    pub fn read8(&self, address: SegmentedAddress) -> Result<u8, MemoryError> {
        Ok(self.bytes[self.checked_byte(address)?])
    }

    pub fn write8(&mut self, address: SegmentedAddress, value: u8) -> Result<(), MemoryError> {
        let index = self.checked_byte(address)?;
        self.bytes[index] = value;
        Ok(())
    }

    pub fn read16(&self, address: SegmentedAddress) -> Result<u16, MemoryError> {
        let [lo, hi] = self.checked_word(address)?;
        Ok(u16::from_le_bytes([self.bytes[lo], self.bytes[hi]]))
    }

    /// Little-endian; validates BOTH bytes before changing memory.
    pub fn write16(&mut self, address: SegmentedAddress, value: u16) -> Result<(), MemoryError> {
        let indices = self.checked_word(address)?;
        self.write_checked_word(indices, value);
        Ok(())
    }

    // Only use indices returned by checked_word on this same, unresized memory.
    fn write_checked_word(&mut self, [lo, hi]: [usize; 2], value: u16) {
        let bytes = value.to_le_bytes();
        self.bytes[lo] = bytes[0];
        self.bytes[hi] = bytes[1];
    }

    /// On error, `output` is unchanged. Empty reads always succeed.
    pub fn read_bytes(
        &self,
        address: SegmentedAddress,
        output: &mut [u8],
    ) -> Result<(), MemoryError> {
        self.checked_span(address, output.len())?;
        for (i, byte) in output.iter_mut().enumerate() {
            let at = SegmentedAddress::new(address.segment, address.offset + i as u16);
            *byte = self.bytes[self.physical_address(at)];
        }
        Ok(())
    }

    /// On error, memory is unchanged. Empty writes always succeed.
    pub fn write_bytes(
        &mut self,
        address: SegmentedAddress,
        input: &[u8],
    ) -> Result<(), MemoryError> {
        self.checked_span(address, input.len())?;
        for (i, &byte) in input.iter().enumerate() {
            let at = SegmentedAddress::new(address.segment, address.offset + i as u16);
            let index = self.physical_address(at);
            self.bytes[index] = byte;
        }
        Ok(())
    }
}

/// Architectural storage, not an executing CPU. Defaults are zero, not a
/// hardware reset state. AX is authoritative: AL/AH are never separate copies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Registers {
    pub ax: u16,
    pub bx: u16,
    pub cx: u16,
    pub dx: u16,
    pub si: u16,
    pub di: u16,
    pub bp: u16,
    pub sp: u16,
    pub cs: u16,
    pub ds: u16,
    pub es: u16,
    pub ss: u16,
    pub ip: u16,
    pub flags: Flags,
}

impl Registers {
    pub const fn al(&self) -> u8 {
        self.ax as u8
    }
    pub const fn ah(&self) -> u8 {
        (self.ax >> 8) as u8
    }
    pub fn set_al(&mut self, value: u8) {
        self.ax = (self.ax & 0xff00) | u16::from(value);
    }
    pub fn set_ah(&mut self, value: u8) {
        self.ax = (self.ax & 0x00ff) | (u16::from(value) << 8);
    }

    /// Push the supplied VALUE; does not choose a CPU's PUSH SP operand quirk.
    /// SP wraps as u16, then the complete SS:SP word is validated.
    pub fn push16(&mut self, memory: &mut GuestMemory, value: u16) -> Result<(), MemoryError> {
        let sp = self.sp.wrapping_sub(2);
        memory.write16(SegmentedAddress::new(self.ss, sp), value)?;
        self.sp = sp;
        Ok(())
    }

    /// Pop a value, not a destination register (POP SP quirks are out of scope).
    pub fn pop16(&mut self, memory: &GuestMemory) -> Result<u16, MemoryError> {
        let value = memory.read16(SegmentedAddress::new(self.ss, self.sp))?;
        self.sp = self.sp.wrapping_add(2);
        Ok(value)
    }

    /// Push old CS, then return IP; load target CS:IP. The caller must provide
    /// the already-decoded next IP explicitly; no instruction length is guessed.
    /// Validate the entire four-byte frame before modifying memory or registers.
    /// The target is not fetched or checked against memory: generated static
    /// control-flow dispatch, not these storage primitives, owns target validity.
    pub fn far_call(
        &mut self,
        memory: &mut GuestMemory,
        target: SegmentedAddress,
        return_ip: u16,
    ) -> Result<(), MemoryError> {
        let cs_sp = self.sp.wrapping_sub(2);
        let ip_sp = self.sp.wrapping_sub(4);
        let cs_indices = memory.checked_word(SegmentedAddress::new(self.ss, cs_sp))?;
        let ip_indices = memory.checked_word(SegmentedAddress::new(self.ss, ip_sp))?;
        memory.write_checked_word(cs_indices, self.cs);
        memory.write_checked_word(ip_indices, return_ip);
        self.sp = ip_sp;
        self.cs = target.segment;
        self.ip = target.offset;
        Ok(())
    }

    /// Pop IP, then CS, without changing flags or stack bytes.
    pub fn far_return(&mut self, memory: &GuestMemory) -> Result<(), MemoryError> {
        self.far_return_with_cleanup(memory, 0)
    }

    /// RETF imm16-style stack-byte cleanup after a successful IP/CS pop.
    /// The cleanup changes only SP and need not address readable guest memory.
    pub fn far_return_with_cleanup(
        &mut self,
        memory: &GuestMemory,
        stack_bytes: u16,
    ) -> Result<(), MemoryError> {
        let ip = memory.read16(SegmentedAddress::new(self.ss, self.sp))?;
        let cs = memory.read16(SegmentedAddress::new(self.ss, self.sp.wrapping_add(2)))?;
        self.ip = ip;
        self.cs = cs;
        self.sp = self.sp.wrapping_add(4).wrapping_add(stack_bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn addr(segment: u16, offset: u16) -> SegmentedAddress {
        SegmentedAddress::new(segment, offset)
    }

    // Independent oracle: signed wide integers, mathematical modular residue,
    // range comparisons for carry/overflow, decimal remainder for nibble carry,
    // and explicit bit counting for parity. No production XOR/sign-bit formulas.
    fn oracle(a: u16, b: u16, carry: bool, bits: u32, subtract: bool) -> (u16, u16) {
        let modulus = 1_i64 << bits;
        let midpoint = modulus / 2;
        let a = i64::from(a);
        let b = i64::from(b);
        let c = i64::from(carry);
        let signed = |v| if v < midpoint { v } else { v - modulus };
        let (wide, signed_wide, nibble) = if subtract {
            (a - b - c, signed(a) - signed(b) - c, a % 16 - b % 16 - c)
        } else {
            (a + b + c, signed(a) + signed(b) + c, a % 16 + b % 16 + c)
        };
        let result = wide.rem_euclid(modulus);
        let mut low_byte = result % 256;
        let mut ones = 0;
        for _ in 0..8 {
            ones += low_byte % 2;
            low_byte /= 2;
        }
        let mut flags = 0;
        for (bit, value) in [
            (Flags::CF, wide < 0 || wide >= modulus),
            (Flags::PF, ones % 2 == 0),
            (Flags::AF, !(0..16).contains(&nibble)),
            (Flags::ZF, result == 0),
            (Flags::SF, result >= midpoint),
            (
                Flags::OF,
                signed_wide < -midpoint || signed_wide >= midpoint,
            ),
        ] {
            if value {
                flags |= bit;
            }
        }
        (result as u16, flags)
    }

    fn check_arithmetic(a: u16, b: u16, bits: u32, initial: u16) {
        for op in 0..5 {
            let subtract = op >= 2;
            let carry = (op == 1 || op == 3) && initial & Flags::CF != 0;
            let (expected_result, expected_flags) = oracle(a, b, carry, bits, subtract);
            let mut flags = Flags::from_bits(initial);
            let result = if bits == 8 {
                let a = a as u8;
                let b = b as u8;
                match op {
                    0 => Some(u16::from(add8(a, b, &mut flags))),
                    1 => Some(u16::from(adc8(a, b, &mut flags))),
                    2 => Some(u16::from(sub8(a, b, &mut flags))),
                    3 => Some(u16::from(sbb8(a, b, &mut flags))),
                    _ => {
                        cmp8(a, b, &mut flags);
                        None
                    }
                }
            } else {
                match op {
                    0 => Some(add16(a, b, &mut flags)),
                    1 => Some(adc16(a, b, &mut flags)),
                    2 => Some(sub16(a, b, &mut flags)),
                    3 => Some(sbb16(a, b, &mut flags)),
                    _ => {
                        cmp16(a, b, &mut flags);
                        None
                    }
                }
            };
            if let Some(result) = result {
                assert_eq!(
                    result, expected_result,
                    "result: op={op} bits={bits} a={a:x} b={b:x} initial={initial:x}"
                );
            }
            assert_eq!(
                flags.bits(),
                (initial & !Flags::ARITHMETIC) | expected_flags,
                "flags: op={op} bits={bits} a={a:x} b={b:x} initial={initial:x}"
            );
        }
    }

    #[test]
    fn exhaustive_byte_arithmetic_with_both_carry_inputs() {
        for a in 0..=255_u16 {
            for b in 0..=255_u16 {
                let initial = (a * 257) ^ b.rotate_left(5);
                for carry in [0, Flags::CF] {
                    check_arithmetic(a, b, 8, (initial & !Flags::CF) | carry);
                }
            }
        }
    }

    fn next(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed
    }

    #[test]
    fn word_arithmetic_boundaries_and_100000_deterministic_pairs() {
        let edges = [
            0, 1, 2, 7, 0x000f, 0x0010, 0x001f, 0x00ff, 0x0100, 0x0fff, 0x1000, 0x7ffe, 0x7fff,
            0x8000, 0x8001, 0x800f, 0xfffe, 0xffff,
        ];
        for a in edges {
            for b in edges {
                for carry in [0, Flags::CF] {
                    check_arithmetic(a, b, 16, !Flags::CF | carry);
                }
            }
        }
        let mut seed = 0x7016_32a4_52e9_8d0b;
        for _ in 0..100_000 {
            let a = (next(&mut seed) >> 32) as u16;
            let b = (next(&mut seed) >> 32) as u16;
            let initial = (next(&mut seed) >> 32) as u16;
            for carry in [0, Flags::CF] {
                check_arithmetic(a, b, 16, (initial & !Flags::CF) | carry);
            }
        }
    }

    #[test]
    fn every_raw_flags_pattern_preserves_non_arithmetic_bits() {
        for raw in 0..=u16::MAX {
            check_arithmetic(0x7f, 1, 8, raw);
            check_arithmetic(0, 0xffff, 16, raw);
        }
    }

    #[test]
    fn flag_definitions_accessors_and_mask_updates() {
        assert_eq!(
            [
                Flags::CF,
                Flags::PF,
                Flags::AF,
                Flags::ZF,
                Flags::SF,
                Flags::OF
            ],
            [1, 4, 16, 64, 128, 2048]
        );
        assert_eq!([Flags::TF, Flags::IF, Flags::DF], [256, 512, 1024]);
        let mut flags = Flags::from_bits(0xffff);
        assert!(flags.cf() && flags.pf() && flags.af() && flags.zf() && flags.sf() && flags.of());
        flags.set(Flags::ARITHMETIC, false);
        assert!(
            !flags.cf() && !flags.pf() && !flags.af() && !flags.zf() && !flags.sf() && !flags.of()
        );
        assert!(flags.contains(Flags::IF | Flags::DF));
        assert_eq!(flags.bits(), !Flags::ARITHMETIC);
        flags.set(Flags::CF, true);
        assert_eq!(flags.bits(), !Flags::ARITHMETIC | Flags::CF);
        // PF uses the low byte, whereas ZF/SF use the full word.
        assert_eq!(add16(0x0100, 0, &mut flags), 0x0100);
        assert!(flags.pf());
        assert!(!flags.zf());
        assert!(!flags.sf());
    }

    fn registers(sp: u16, ss: u16) -> Registers {
        Registers {
            ax: 0x1234,
            bx: 0x5678,
            cx: 0x9abc,
            dx: 0xdef0,
            si: 0x1357,
            di: 0x2468,
            bp: 0x9876,
            sp,
            cs: 0xabcd,
            ds: 0x5555,
            es: 0xaaaa,
            ss,
            ip: 0x5432,
            flags: Flags::from_bits(0xfedb),
        }
    }

    #[test]
    fn ax_ah_al_are_exhaustive_aliases_not_copies() {
        for ax in 0..=u16::MAX {
            let original = Registers {
                ax,
                ..registers(0, 0)
            };
            assert_eq!(original.al(), (ax % 256) as u8);
            assert_eq!(original.ah(), (ax / 256) as u8);
            for byte in [0, 1, 0x7f, 0x80, 0xff] {
                let mut regs = original;
                regs.set_al(byte);
                assert_eq!(
                    regs,
                    Registers {
                        ax: (ax / 256) * 256 + u16::from(byte),
                        ..original
                    }
                );
                regs = original;
                regs.set_ah(byte);
                assert_eq!(
                    regs,
                    Registers {
                        ax: u16::from(byte) * 256 + ax % 256,
                        ..original
                    }
                );
            }
        }
    }

    #[test]
    fn little_endian_segment_aliases_and_buffer_ownership() {
        let mut memory = GuestMemory::new(0x20000, A20::Enabled);
        assert_eq!(memory.len(), 0x20000);
        assert!(!memory.is_empty());
        memory.write16(addr(0x1000, 0x10), 0xbeef).unwrap();
        assert_eq!(&memory.as_bytes()[0x10010..0x10012], &[0xef, 0xbe]);
        assert_eq!(memory.read16(addr(0x1001, 0)).unwrap(), 0xbeef);
        memory.write8(addr(0x1001, 1), 0x12).unwrap();
        assert_eq!(memory.read16(addr(0x1000, 0x10)).unwrap(), 0x12ef);
        let bytes = memory.into_bytes();
        let memory = GuestMemory::from_bytes(bytes, A20::Disabled);
        assert_eq!(memory.read16(addr(0x1001, 0)).unwrap(), 0x12ef);
    }

    #[test]
    fn a20_is_explicit_and_switching_it_does_not_move_bytes() {
        let mut memory = GuestMemory::new(0x10fff0, A20::Enabled);
        memory.write8(addr(0, 0), 0x11).unwrap();
        memory.write8(addr(0xffff, 0x10), 0x22).unwrap();
        memory.write8(addr(0xffff, 0xffff), 0x33).unwrap();
        assert_eq!(memory.physical_address(addr(0xffff, 0xffff)), 0x10ffef);
        let before = memory.as_bytes().to_vec();
        memory.set_a20(A20::Disabled);
        assert_eq!(memory.a20(), A20::Disabled);
        assert_eq!(memory.as_bytes(), before);
        assert_eq!(memory.read8(addr(0xffff, 0x10)).unwrap(), 0x11);
        assert_eq!(memory.physical_address(addr(0xffff, 0xffff)), 0xffef);
        memory.write8(addr(0xffff, 0x10), 0x44).unwrap();
        memory.set_a20(A20::Enabled);
        assert_eq!(memory.read8(addr(0xffff, 0x10)).unwrap(), 0x22);
        assert_eq!(memory.read8(addr(0, 0)).unwrap(), 0x44);
        assert_eq!(memory.read8(addr(0xffff, 0xffff)).unwrap(), 0x33);
    }

    #[test]
    fn each_word_byte_is_a20_gated_not_just_the_start_address() {
        let mut memory = GuestMemory::new(0x100000, A20::Disabled);
        memory.write16(addr(0xffff, 0x000f), 0xa1b2).unwrap();
        assert_eq!(memory.as_bytes()[0xfffff], 0xb2);
        assert_eq!(memory.as_bytes()[0], 0xa1);
        assert_eq!(memory.read16(addr(0xffff, 0x000f)).unwrap(), 0xa1b2);
        memory.set_a20(A20::Enabled);
        let before = memory.clone();
        let expected = MemoryError::OutOfBounds {
            address: addr(0xffff, 0x10),
            physical: 0x100000,
            size: 0x100000,
        };
        assert_eq!(memory.read16(addr(0xffff, 0xf)), Err(expected.clone()));
        assert_eq!(memory.write16(addr(0xffff, 0xf), 0x9988), Err(expected));
        assert_eq!(memory, before);
        let mut high = GuestMemory::new(0x100001, A20::Enabled);
        high.write16(addr(0xffff, 0xf), 0xa1b2).unwrap();
        assert_eq!(&high.as_bytes()[0xfffff..], &[0xb2, 0xa1]);
        assert_eq!(high.as_bytes()[0], 0);
    }

    #[test]
    fn effective_offset_wrap_is_distinct_from_rejected_word_crossing() {
        assert_eq!(addr(0x1234, 0xffff).wrapping_add(1), addr(0x1234, 0));
        for a20 in [A20::Disabled, A20::Enabled] {
            let mut memory = GuestMemory::new(0x10fff0, a20);
            memory.write16(addr(0xffff, 0xfffe), 0x6789).unwrap();
            assert_eq!(memory.read16(addr(0xffff, 0xfffe)).unwrap(), 0x6789);
            let before = memory.clone();
            let expected = MemoryError::SegmentBoundary {
                address: addr(0xffff, 0xffff),
                len: 2,
            };
            assert_eq!(memory.read16(addr(0xffff, 0xffff)), Err(expected.clone()));
            assert_eq!(memory.write16(addr(0xffff, 0xffff), 0x1234), Err(expected));
            assert_eq!(memory, before);
            assert!(memory.read8(addr(0xffff, 0xffff)).is_ok());
        }
        assert!(GuestMemory::check_segment_span(addr(0, 1), usize::MAX).is_err());
    }

    #[test]
    fn byte_and_word_bounds_never_wrap_by_buffer_size() {
        let mut empty = GuestMemory::new(0, A20::Disabled);
        assert!(empty.is_empty());
        assert!(empty.read8(addr(0, 0)).is_err());
        assert!(empty.write8(addr(0, 0), 1).is_err());
        empty.read_bytes(addr(0xffff, 0xffff), &mut []).unwrap();
        empty.write_bytes(addr(0xffff, 0xffff), &[]).unwrap();
        let mut memory = GuestMemory::from_bytes(vec![0xab], A20::Disabled);
        let before = memory.clone();
        assert_eq!(memory.read8(addr(0, 0)).unwrap(), 0xab);
        assert!(memory.write8(addr(0, 1), 0xcd).is_err());
        let expected = MemoryError::OutOfBounds {
            address: addr(0, 1),
            physical: 1,
            size: 1,
        };
        assert_eq!(memory.write16(addr(0, 0), 0x1234), Err(expected.clone()));
        assert_eq!(memory.read16(addr(0, 0)), Err(expected));
        assert_eq!(memory, before);
    }

    #[test]
    fn byte_slices_are_atomic_and_follow_the_same_address_policy() {
        let mut memory = GuestMemory::new(0x100000, A20::Disabled);
        memory
            .write_bytes(addr(0xffff, 0xe), &[1, 2, 3, 4])
            .unwrap();
        assert_eq!(&memory.as_bytes()[0xffffe..], &[1, 2]);
        assert_eq!(&memory.as_bytes()[..2], &[3, 4]);
        let mut output = [0; 4];
        memory.read_bytes(addr(0xffff, 0xe), &mut output).unwrap();
        assert_eq!(output, [1, 2, 3, 4]);
        memory.write_bytes(addr(0, 0xfffc), &[5, 6, 7, 8]).unwrap();
        let before = memory.clone();
        assert!(memory.write_bytes(addr(0, 0xfffd), &[9; 4]).is_err());
        assert!(memory.read_bytes(addr(0, 0xfffd), &mut output).is_err());
        assert_eq!(output, [1, 2, 3, 4]);
        assert_eq!(memory, before);
        memory.set_a20(A20::Enabled);
        let before = memory.clone();
        assert!(memory.write_bytes(addr(0xffff, 0xe), &[9; 4]).is_err());
        assert!(memory.read_bytes(addr(0xffff, 0xe), &mut output).is_err());
        assert_eq!(output, [1, 2, 3, 4]);
        assert_eq!(memory, before);
        let mut full_segment = vec![0x5a; 0x10000];
        memory.write_bytes(addr(0, 0), &full_segment).unwrap();
        full_segment.fill(0);
        memory.read_bytes(addr(0, 0), &mut full_segment).unwrap();
        assert!(full_segment.iter().all(|&b| b == 0x5a));
    }

    #[test]
    fn address_translation_properties() {
        let mut seed = 0x93e0_c08a_0e25_7051;
        for a20 in [A20::Disabled, A20::Enabled] {
            let memory = GuestMemory::new(0, a20);
            for _ in 0..10_000 {
                let segment = (next(&mut seed) >> 32) as u16;
                let offset = (next(&mut seed) >> 32) as u16;
                let linear = usize::from(segment) * 16 + usize::from(offset);
                let expected = if a20 == A20::Disabled && linear >= 1_048_576 {
                    linear - 1_048_576
                } else {
                    linear
                };
                assert_eq!(memory.physical_address(addr(segment, offset)), expected);
                if segment < u16::MAX && offset >= 16 {
                    assert_eq!(
                        memory.physical_address(addr(segment, offset)),
                        memory.physical_address(addr(segment + 1, offset - 16))
                    );
                }
            }
        }
    }

    #[test]
    fn push_pop_round_trip_for_every_sp_and_wrapping_boundary() {
        let mut memory = GuestMemory::new(0x10000, A20::Enabled);
        for sp in 0..=u16::MAX {
            let initial = registers(sp, 0);
            let mut regs = initial;
            let value = sp.rotate_left(7) ^ 0xc15a;
            if sp == 1 {
                let before = memory.clone();
                assert!(regs.push16(&mut memory, value).is_err());
                assert_eq!(regs, initial);
                assert_eq!(memory, before);
            } else {
                regs.push16(&mut memory, value).unwrap();
                let new_sp = sp.wrapping_sub(2);
                assert_eq!(
                    regs,
                    Registers {
                        sp: new_sp,
                        ..initial
                    }
                );
                assert_eq!(memory.read16(addr(0, new_sp)).unwrap(), value);
                let after_push = [
                    memory.as_bytes()[usize::from(new_sp)],
                    memory.as_bytes()[usize::from(new_sp) + 1],
                ];
                assert_eq!(regs.pop16(&memory).unwrap(), value);
                assert_eq!(regs, initial);
                assert_eq!(after_push, value.to_le_bytes());
            }
        }
    }

    #[test]
    fn far_call_frame_order_and_return_round_trip_for_every_sp() {
        let mut memory = GuestMemory::new(0x10000, A20::Enabled);
        let target = addr(0x1111, 0x2222);
        for sp in 0..=u16::MAX {
            let initial = registers(sp, 0);
            let mut regs = initial;
            let return_ip = sp.rotate_left(3) ^ 0xb37a;
            if sp == 1 || sp == 3 {
                let before = memory.clone();
                assert!(regs.far_call(&mut memory, target, return_ip).is_err());
                assert_eq!(regs, initial);
                assert_eq!(memory, before);
            } else {
                regs.far_call(&mut memory, target, return_ip).unwrap();
                assert_eq!(
                    regs,
                    Registers {
                        sp: sp.wrapping_sub(4),
                        cs: target.segment,
                        ip: target.offset,
                        ..initial
                    }
                );
                assert_eq!(
                    memory.read16(addr(0, sp.wrapping_sub(4))).unwrap(),
                    return_ip
                );
                assert_eq!(
                    memory.read16(addr(0, sp.wrapping_sub(2))).unwrap(),
                    initial.cs
                );
                regs.far_return(&memory).unwrap();
                assert_eq!(
                    regs,
                    Registers {
                        ip: return_ip,
                        ..initial
                    }
                );
            }
        }
    }

    #[test]
    fn stack_and_far_frames_across_a20_with_odd_sp() {
        for a20 in [A20::Disabled, A20::Enabled] {
            let size = if a20 == A20::Disabled {
                0x100000
            } else {
                0x10fff0
            };
            let mut memory = GuestMemory::new(size, a20);
            let original = registers(0x11, 0xffff);
            let mut regs = original;
            regs.push16(&mut memory, 0x9876).unwrap();
            assert_eq!(memory.as_bytes()[0xfffff], 0x76);
            assert_eq!(
                memory.as_bytes()[if a20 == A20::Disabled { 0 } else { 0x100000 }],
                0x98
            );
            assert_eq!(regs.pop16(&memory).unwrap(), 0x9876);
            assert_eq!(regs, original);
            regs.sp = 0x13;
            let original = regs;
            regs.far_call(&mut memory, addr(0x789a, 0xbcde), 0x3579)
                .unwrap();
            assert_eq!(regs.sp, 0xf);
            assert_eq!(memory.as_bytes()[0xfffff], 0x79);
            assert_eq!(
                memory.as_bytes()[if a20 == A20::Disabled { 0 } else { 0x100000 }],
                0x35
            );
            let before_return = memory.clone();
            regs.far_return(&memory).unwrap();
            assert_eq!(
                regs,
                Registers {
                    ip: 0x3579,
                    ..original
                }
            );
            assert_eq!(memory, before_return);
        }
    }

    #[test]
    fn successful_stack_writes_change_only_the_frame_bytes() {
        for a20 in [A20::Disabled, A20::Enabled] {
            for (ss, sp) in [(0, 0), (0, 2), (0, 7), (0x1234, 0x5678), (0xffff, 0x13)] {
                let mut memory = GuestMemory::from_bytes(vec![0x5a; 0x10fff0], a20);
                let original = registers(sp, ss);
                let mut regs = original;
                let mut expected = memory.as_bytes().to_vec();
                // Independent byte placement: wide multiply/add, then remainder
                // for the restricted A20-disabled real-mode address range.
                let place = |expected: &mut [u8], offset: u16, value: u16| {
                    let linear = usize::from(ss) * 16 + usize::from(offset);
                    for i in 0..2 {
                        let physical = if a20 == A20::Disabled {
                            (linear + i) % 0x100000
                        } else {
                            linear + i
                        };
                        expected[physical] = if i == 0 {
                            (value % 256) as u8
                        } else {
                            (value / 256) as u8
                        };
                    }
                };
                regs.push16(&mut memory, 0x7654).unwrap();
                place(&mut expected, sp.wrapping_sub(2), 0x7654);
                assert_eq!(memory.as_bytes(), expected);
                assert_eq!(regs.pop16(&memory).unwrap(), 0x7654);
                assert_eq!(regs, original);
                assert_eq!(memory.as_bytes(), expected);
                regs.far_call(&mut memory, addr(0x8765, 0x4321), 0x2468)
                    .unwrap();
                place(&mut expected, sp.wrapping_sub(2), original.cs);
                place(&mut expected, sp.wrapping_sub(4), 0x2468);
                assert_eq!(memory.as_bytes(), expected);
                regs.far_return(&memory).unwrap();
                assert_eq!(
                    regs,
                    Registers {
                        ip: 0x2468,
                        ..original
                    }
                );
                assert_eq!(memory.as_bytes(), expected);
            }
        }
    }

    #[test]
    fn nested_far_calls_and_retf_cleanup_wrap_sp() {
        let mut memory = GuestMemory::new(0x10000, A20::Disabled);
        let original = registers(2, 0);
        let mut regs = original;
        regs.push16(&mut memory, 0xcafe).unwrap();
        regs.far_call(&mut memory, addr(0x1000, 0x2000), 0x1234)
            .unwrap();
        regs.far_call(&mut memory, addr(0x3000, 0x4000), 0x2345)
            .unwrap();
        regs.push16(&mut memory, 0xbeef).unwrap();
        assert_eq!(regs.pop16(&memory).unwrap(), 0xbeef);
        let before_returns = memory.clone();
        regs.far_return(&memory).unwrap();
        assert_eq!((regs.cs, regs.ip, regs.sp), (0x1000, 0x2345, 0xfffc));
        regs.far_return(&memory).unwrap();
        assert_eq!(
            regs,
            Registers {
                sp: 0,
                ip: 0x1234,
                ..original
            }
        );
        assert_eq!(regs.pop16(&memory).unwrap(), 0xcafe);
        assert_eq!(memory, before_returns);
        regs.sp = 0xfff8;
        regs.far_call(&mut memory, addr(7, 8), 9).unwrap();
        regs.far_return_with_cleanup(&memory, 0x10).unwrap();
        assert_eq!(
            regs,
            Registers {
                sp: 8,
                ip: 9,
                ..original
            }
        );
        // Odd cleanup is a byte count, not a word count, and can wrap too.
        regs.sp = 0;
        regs.far_call(&mut memory, addr(7, 8), 10).unwrap();
        regs.far_return_with_cleanup(&memory, u16::MAX).unwrap();
        assert_eq!(
            regs,
            Registers {
                sp: 0xffff,
                ip: 10,
                ..original
            }
        );
    }

    #[test]
    fn all_stack_error_paths_preserve_entire_memory_and_registers() {
        // op: 0 push, 1 pop, 2 far call, 3 far return, 4 far return + cleanup.
        // Include late failures: second byte and second word, including A20.
        let cases = [
            (0, 0, 0, 0, A20::Disabled),
            (0, 1, 0, 2, A20::Enabled),
            (0, 0x10000, 0, 1, A20::Enabled),
            (1, 1, 0, 0, A20::Enabled),
            (1, 0x10000, 0, 0xffff, A20::Enabled),
            (2, 2, 0, 2, A20::Enabled),
            (2, 0x10000, 0, 3, A20::Enabled),
            (2, 1, 0, 2, A20::Enabled),
            (2, 3, 0, 4, A20::Enabled),
            (2, 0x100000, 0xffff, 0x11, A20::Enabled),
            (3, 2, 0, 0, A20::Enabled),
            (3, 3, 0, 0, A20::Enabled),
            (3, 0x10000, 0, 0xfffd, A20::Enabled),
            (3, 0x10000, 0, 0xffff, A20::Enabled),
            (3, 0x100000, 0xffff, 0xd, A20::Enabled),
            (4, 2, 0, 0, A20::Enabled),
            (4, 0x10000, 0, 0xfffd, A20::Enabled),
        ];
        for (op, size, ss, sp, a20) in cases {
            let mut memory = GuestMemory::from_bytes(vec![0xa5; size], a20);
            let before_memory = memory.clone();
            let mut regs = registers(sp, ss);
            let before_regs = regs;
            let result = match op {
                0 => regs.push16(&mut memory, 0x1234),
                1 => regs.pop16(&memory).map(|_| ()),
                2 => regs.far_call(&mut memory, addr(0x1111, 0x2222), 0x3333),
                3 => regs.far_return(&memory),
                _ => regs.far_return_with_cleanup(&memory, 0x17),
            };
            assert!(result.is_err(), "op={op} size={size:x} ss={ss:x} sp={sp:x}");
            assert_eq!(regs, before_regs, "register partial commit: op={op}");
            assert_eq!(memory, before_memory, "memory partial commit: op={op}");
        }
    }

    #[test]
    fn errors_implement_display_and_std_error() {
        let error = MemoryError::OutOfBounds {
            address: addr(0x1234, 0x5678),
            physical: 0x179b8,
            size: 1,
        };
        let as_error: &dyn Error = &error;
        assert!(as_error.source().is_none());
        assert_eq!(
            as_error.to_string(),
            "guest byte at 1234:5678 maps to physical 0x179b8, outside 1-byte memory"
        );
        let error = MemoryError::SegmentBoundary {
            address: addr(0, 0xffff),
            len: 2,
        };
        assert_eq!(
            error.to_string(),
            "2-byte access at 0000:ffff crosses the 64-KiB segment boundary (unsupported)"
        );
    }
}
