// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed x87 data operations for statically generated code, NOT an opcode decoder.
//!
//! Values use rustc_apfloat's integer-only X87DoubleExtended (64 significand bits,
//! 15 exponent bits). No host floating-point arithmetic or native FFI is used.
//! Supported: canonical finite normal values and signed zeros, PC=11, all four
//! RC modes, integer/real loads and stores, basic arithmetic and ordered compare.
//! There are no transcendental, environment-save/restore or CPU-FLAGS operations.
//!
//! This is deliberately NOT a complete exception emulator. Invalid arithmetic,
//! zero division, overflow, underflow and stack faults record sticky x87 status
//! and return Fault even when masked: no fabricated indefinite/infinity result
//! is committed. INEXACT records PE and is nonfatal (including with PM clear).
//! ES/B reflect sticky unmasked exception bits, but deferred traps are not run.
//! NaN, infinity, denormal and noncanonical inputs are rejected; unsupported
//! encodings themselves are not misreported as hardware invalid exceptions.
//! APFloat detects tininess after rounding; the minimum-normal boundary is
//! checked separately to record x87's before-rounding underflow and fail closed.
//!
//! Register indices are logical ST(i); the tag word is in PHYSICAL R0..R7 order.
//! On Fault, values/TOP/control do not change; exception/condition bits may.
//! Stores return a value without popping. AOT callers must first complete a
//! checked guest-memory write and only then pop for FSTP/FISTP. This module does
//! not access guest memory, decode instruction bytes, or alter CPU flags.

#![forbid(unsafe_code)]

use crate::runtime::Fault;
use core::cmp::Ordering;
pub use rustc_apfloat::ieee::X87DoubleExtended;
use rustc_apfloat::ieee::{Double, IeeeFloat, Semantics, Single};
use rustc_apfloat::{Float, FloatConvert, Round, Status, StatusAnd};

// APFloat 0.2.3 reports only INEXACT when overflow saturates to finite max
// under directed rounding (ieee.rs::overflow_result). For that boundary only,
// recompute at the SAME 64-bit precision with a wider exponent, recovering the
// missing OE without using a host float or a higher-precision normal path.
// The same boundary-only probe handles x87 tininess-before-rounding.
struct OverflowProbe;
impl Semantics for OverflowProbe {
    const BITS: usize = 81;
    const EXP_BITS: usize = 17;
    const PRECISION: usize = 64;
}
type WideExponent = IeeeFloat<OverflowProbe>;

/// Explicit semantic operation, not a machine opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    SubReverse,
    Mul,
    Div,
    DivReverse,
}

/// Eight physical extended registers, plus control/status and circular TOP.
#[derive(Clone, Debug)]
pub struct Fpu {
    control: u16,
    status: u16,
    top: u8,
    registers: [Option<X87DoubleExtended>; 8],
}

impl Default for Fpu {
    fn default() -> Self {
        Self {
            control: 0x037f,
            status: 0,
            top: 0,
            registers: [None; 8],
        }
    }
}

impl Fpu {
    pub const IE: u16 = 1 << 0;
    pub const DE: u16 = 1 << 1;
    pub const ZE: u16 = 1 << 2;
    pub const OE: u16 = 1 << 3;
    pub const UE: u16 = 1 << 4;
    pub const PE: u16 = 1 << 5;
    pub const SF: u16 = 1 << 6;
    pub const ES: u16 = 1 << 7;
    pub const C0: u16 = 1 << 8;
    pub const C1: u16 = 1 << 9;
    pub const C2: u16 = 1 << 10;
    pub const C3: u16 = 1 << 14;
    pub const B: u16 = 1 << 15;
    const EXCEPTIONS: u16 = 0x003f;
    const COMPARE: u16 = Self::C0 | Self::C2 | Self::C3;

    /// FINIT data-state subset; resets all registers, tags, TOP and words.
    pub fn init(&mut self) {
        *self = Self::default();
    }

    /// Current raw control word; reserved bits are preserved, not normalized.
    pub fn control_word(&self) -> u16 {
        self.control
    }

    /// FLDCW subset. Only full extended precision (PC=11) is accepted; all RC
    /// modes and mask bits are retained. No control change occurs on rejection.
    pub fn set_control_word(&mut self, control: u16) -> Result<(), Fault> {
        if control & 0x0300 != 0x0300 {
            return Err(Fault(format!(
                "x87 unsupported precision control PC={:02b}; only PC=11 is implemented",
                (control >> 8) & 3
            )));
        }
        self.control = control;
        self.update_summary();
        Ok(())
    }

    /// FNSTSW data-state subset, including circular TOP in bits 11..13.
    pub fn status_word(&self) -> u16 {
        self.status | (u16::from(self.top) << 11)
    }

    /// Physical TOP, independent of stack occupancy.
    pub fn top(&self) -> u8 {
        self.top
    }

    /// Architectural full tag word, physical R0 first: valid=00, zero=01,
    /// special=10, empty=11. Public mutation admits only valid/zero values.
    pub fn tag_word(&self) -> u16 {
        let mut tags = 0;
        for (index, value) in self.registers.iter().enumerate() {
            let tag = match value {
                None => 3,
                Some(value) if value.is_zero() => 1,
                Some(value) if value.is_normal() => 0,
                Some(_) => 2,
            };
            tags |= tag << (index * 2);
        }
        tags
    }

    /// FNCLEX subset: clears exception flags, SF, ES and B, not TOP or C0..C3.
    pub fn clear_exceptions(&mut self) {
        self.status &= !(Self::EXCEPTIONS | Self::SF | Self::ES | Self::B);
    }

    /// Read a populated logical ST(i). Empty reads record IE/SF with C1=0.
    pub fn get(&mut self, index: usize) -> Result<X87DoubleExtended, Fault> {
        let physical = self.physical(index)?;
        match self.registers[physical] {
            Some(value) => Ok(value),
            None => Err(self.stack_fault(false)),
        }
    }

    /// Store an already computed supported value in logical ST(i), including
    /// an empty destination (FST ST(i)). Does not push/pop or round the value.
    pub fn set(&mut self, index: usize, value: X87DoubleExtended) -> Result<(), Fault> {
        let physical = self.physical(index)?;
        Self::supported(value)?;
        self.registers[physical] = Some(value);
        self.status &= !Self::C1;
        Ok(())
    }

    /// Push a supported value. An occupied new TOP records IE/SF with C1=1,
    /// returning Fault without moving TOP or replacing any register.
    pub fn push(&mut self, value: X87DoubleExtended) -> Result<(), Fault> {
        Self::supported(value)?;
        let new_top = self.top.wrapping_sub(1) & 7;
        if self.registers[usize::from(new_top)].is_some() {
            return Err(self.stack_fault(true));
        }
        self.top = new_top;
        self.registers[usize::from(new_top)] = Some(value);
        self.status &= !Self::C1;
        Ok(())
    }

    /// Pop a populated ST(0). On success preserves condition bits so an
    /// explicitly composed arithmetic/store-and-pop retains its C1 result.
    pub fn pop(&mut self) -> Result<X87DoubleExtended, Fault> {
        let value = self.get(0)?;
        self.registers[usize::from(self.top)] = None;
        self.top = (self.top + 1) & 7;
        Ok(value)
    }

    /// FXCH ST(i) for populated operands; empty operands fail atomically.
    pub fn exchange(&mut self, index: usize) -> Result<(), Fault> {
        let other = self.physical(index)?;
        let _ = self.get(0)?;
        let _ = self.get(index)?;
        self.registers.swap(usize::from(self.top), other);
        self.status &= !Self::C1;
        Ok(())
    }

    /// FFREE ST(i): marks a logical register empty without changing TOP.
    pub fn free(&mut self, index: usize) -> Result<(), Fault> {
        let physical = self.physical(index)?;
        self.registers[physical] = None;
        Ok(())
    }

    /// FILD signed word, exactly represented at full extended precision.
    pub fn load_i16(&mut self, value: i16) -> Result<(), Fault> {
        self.load_integer(i128::from(value))
    }

    /// FILD signed doubleword, exactly represented at full extended precision.
    pub fn load_i32(&mut self, value: i32) -> Result<(), Fault> {
        self.load_integer(i128::from(value))
    }

    /// FILD signed quadword, including i64::MIN, without a binary64 detour.
    pub fn load_i64(&mut self, value: i64) -> Result<(), Fault> {
        self.load_integer(i128::from(value))
    }

    /// FLD m32real from raw IEEE bits; normal/zero inputs only, exact widening.
    /// Denormal operands record DE; signaling NaNs record IE; all special
    /// inputs fail without pushing a replacement value.
    pub fn load_f32_bits(&mut self, bits: u32) -> Result<(), Fault> {
        self.load_real(Single::from_bits(u128::from(bits)))
    }

    /// FLD m64real from raw IEEE bits, with the same finite policy as m32real.
    pub fn load_f64_bits(&mut self, bits: u64) -> Result<(), Fault> {
        self.load_real(Double::from_bits(u128::from(bits)))
    }

    /// FLD m80real represented in the low 80 bits. Rejects high garbage bits,
    /// denormals, pseudo-denormals, unnormals, infinities and NaNs BEFORE APFloat
    /// can canonicalize them. Rejection is a subset-policy Fault, not a claim
    /// that FLD m80real would have raised a hardware exception.
    pub fn load_f80_bits(&mut self, bits: u128) -> Result<(), Fault> {
        if bits >> 80 != 0 {
            return Err(Fault("x87 m80real input has bits above bit 79".into()));
        }
        let exponent = (bits >> 64) & 0x7fff;
        let significand = bits as u64;
        if !((exponent == 0 && significand == 0)
            || (exponent > 0 && exponent < 0x7fff && significand >> 63 == 1))
        {
            return Err(Fault(
                "x87 unsupported m80real encoding (requires normal or signed zero)".into(),
            ));
        }
        self.push(X87DoubleExtended::from_bits(bits))
    }

    /// FLD ten little-endian guest bytes, subject to load_f80_bits's policy.
    pub fn load_f80_bytes(&mut self, bytes: [u8; 10]) -> Result<(), Fault> {
        let mut wide = [0; 16];
        wide[..10].copy_from_slice(&bytes);
        self.load_f80_bits(u128::from_le_bytes(wide))
    }

    /// FIST word using control-word RC; invalid range records IE, never clamps.
    /// Does not pop. Inexact conversion records PE and magnitude-rounding C1.
    pub fn store_i16(&mut self, index: usize) -> Result<i16, Fault> {
        Ok(self.store_integer(index, 16)? as i16)
    }

    /// FIST doubleword using RC; no pop, no host float conversion.
    pub fn store_i32(&mut self, index: usize) -> Result<i32, Fault> {
        Ok(self.store_integer(index, 32)? as i32)
    }

    /// FIST quadword using RC; no pop, including exact signed endpoints.
    pub fn store_i64(&mut self, index: usize) -> Result<i64, Fault> {
        Ok(self.store_integer(index, 64)? as i64)
    }

    /// FST m32real bits using RC. Does not pop. Overflow, underflow and
    /// denormal results fail rather than return a surrogate value; rounding
    /// to a normal value at the format boundary follows the hardware fixtures.
    pub fn store_f32_bits(&mut self, index: usize) -> Result<u32, Fault> {
        Ok(self.store_real::<Single>(index)?.to_bits() as u32)
    }

    /// FST m64real bits using RC, with the same finite policy as m32real.
    pub fn store_f64_bits(&mut self, index: usize) -> Result<u64, Fault> {
        Ok(self.store_real::<Double>(index)?.to_bits() as u64)
    }

    /// FST m80real bits; no rounding or pop, including signed zero.
    pub fn store_f80_bits(&mut self, index: usize) -> Result<u128, Fault> {
        let value = self.get(index)?;
        self.status &= !Self::C1;
        Ok(value.to_bits())
    }

    /// FST ten little-endian guest bytes, without popping.
    pub fn store_f80_bytes(&mut self, index: usize) -> Result<[u8; 10], Fault> {
        let wide = self.store_f80_bits(index)?.to_le_bytes();
        let mut bytes = [0; 10];
        bytes.copy_from_slice(&wide[..10]);
        Ok(bytes)
    }

    /// Full-precision basic arithmetic: ST(dst) := op(ST(dst), rhs).
    /// Reverse forms exchange mathematical operands, NOT stack locations.
    /// Only supported finite inputs/results; no pop. C0/C2/C3 are retained
    /// (architecturally undefined here), C1 tracks significand rounding up.
    pub fn binary(
        &mut self,
        op: BinaryOp,
        dst: usize,
        rhs: X87DoubleExtended,
    ) -> Result<(), Fault> {
        let physical = self.physical(dst)?;
        let lhs = self.get(dst)?;
        Self::supported(rhs)?;
        let mut result = Self::calculate(op, lhs, rhs, self.rounding());
        if result.value.is_largest() && result.status == Status::INEXACT {
            let wide_lhs: StatusAnd<WideExponent> = lhs.convert(&mut false);
            let wide_rhs: StatusAnd<WideExponent> = rhs.convert(&mut false);
            let wide = Self::calculate(op, wide_lhs.value, wide_rhs.value, self.rounding());
            if wide.value.ilogb() > X87DoubleExtended::MAX_EXP {
                result.status |= Status::OVERFLOW;
            }
        }
        if result.value.is_smallest_normalized() && result.status == Status::INEXACT {
            // x87 detects tininess before rounding, APFloat after rounding.
            // Toward-zero at full precision in a wider exponent range tells
            // whether the exact magnitude was below extended minimum-normal.
            let wide_lhs: StatusAnd<WideExponent> = lhs.convert(&mut false);
            let wide_rhs: StatusAnd<WideExponent> = rhs.convert(&mut false);
            let wide = Self::calculate(op, wide_lhs.value, wide_rhs.value, Round::TowardZero);
            if wide.value.ilogb() < X87DoubleExtended::MIN_EXP {
                result.status |= Status::UNDERFLOW;
            }
        }
        let truncated = if result.status.contains(Status::INEXACT) {
            Self::calculate(op, lhs, rhs, Round::TowardZero).value
        } else {
            result.value
        };
        let rounded_up = result.value.abs() > truncated.abs();
        let value = self.finish(result, rounded_up)?;
        Self::supported(value)?;
        self.registers[physical] = Some(value);
        Ok(())
    }

    /// Register-source variant of binary; both operands must be populated.
    pub fn binary_registers(&mut self, op: BinaryOp, dst: usize, src: usize) -> Result<(), Fault> {
        self.physical(dst)?;
        let rhs = self.get(src)?;
        self.binary(op, dst, rhs)
    }

    /// FADD finite operand into ST(dst), no pop.
    pub fn add(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::Add, dst, rhs)
    }

    /// FSUB: ST(dst) - rhs, no pop.
    pub fn sub(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::Sub, dst, rhs)
    }

    /// FSUBR: rhs - ST(dst), no pop.
    pub fn sub_reverse(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::SubReverse, dst, rhs)
    }

    /// FMUL finite operand into ST(dst), no pop.
    pub fn mul(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::Mul, dst, rhs)
    }

    /// FDIV: ST(dst) / rhs, no pop; invalid and division by zero fail closed.
    pub fn div(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::Div, dst, rhs)
    }

    /// FDIVR: rhs / ST(dst), no pop; zero denominator fails closed.
    pub fn div_reverse(&mut self, dst: usize, rhs: X87DoubleExtended) -> Result<(), Fault> {
        self.binary(BinaryOp::DivReverse, dst, rhs)
    }

    /// Ordered FCOM finite operands: less=C0, equal=C3, greater=0; C2 and C1
    /// are cleared. No CPU flags or pop; unsupported inputs are not assigned
    /// a fabricated unordered comparison result.
    pub fn compare(&mut self, lhs: usize, rhs: X87DoubleExtended) -> Result<Ordering, Fault> {
        let lhs = self.get(lhs)?;
        Self::supported(rhs)?;
        let order = lhs
            .partial_cmp(&rhs)
            .ok_or_else(|| Fault("x87 unordered comparison is unsupported".into()))?;
        self.status &= !(Self::COMPARE | Self::C1);
        self.status |= match order {
            Ordering::Less => Self::C0,
            Ordering::Equal => Self::C3,
            Ordering::Greater => 0,
        };
        Ok(order)
    }

    /// Register-source FCOM subset, no pop and no CPU flags changes.
    pub fn compare_registers(&mut self, lhs: usize, rhs: usize) -> Result<Ordering, Fault> {
        self.physical(lhs)?;
        let rhs = self.get(rhs)?;
        self.compare(lhs, rhs)
    }

    /// FCHS finite ST(0), including exact signed-zero negation.
    pub fn change_sign(&mut self) -> Result<(), Fault> {
        let value = self.get(0)?;
        self.set(0, -value)
    }

    /// FABS finite ST(0), including -0 -> +0.
    pub fn abs(&mut self) -> Result<(), Fault> {
        let value = self.get(0)?;
        self.set(0, value.abs())
    }

    fn physical(&self, index: usize) -> Result<usize, Fault> {
        if index >= 8 {
            return Err(Fault(format!("x87 invalid logical register ST({index})")));
        }
        Ok((usize::from(self.top) + index) & 7)
    }

    fn rounding(&self) -> Round {
        match (self.control >> 10) & 3 {
            0 => Round::NearestTiesToEven,
            1 => Round::TowardNegative,
            2 => Round::TowardPositive,
            _ => Round::TowardZero,
        }
    }

    fn update_summary(&mut self) {
        self.status &= !(Self::ES | Self::B);
        if self.status & !self.control & Self::EXCEPTIONS != 0 {
            self.status |= Self::ES | Self::B;
        }
    }

    fn record(&mut self, flags: u16) {
        self.status |= flags;
        self.update_summary();
    }

    fn stack_fault(&mut self, overflow: bool) -> Fault {
        self.status &= !Self::C1;
        if overflow {
            self.status |= Self::C1;
        }
        self.record(Self::IE | Self::SF);
        Fault(format!(
            "x87 stack {} (fail-closed, including masked exceptions)",
            if overflow { "overflow" } else { "underflow" }
        ))
    }

    fn supported<T: Float>(value: T) -> Result<(), Fault> {
        if !(value.is_normal() || value.is_zero()) {
            return Err(Fault("x87 unsupported nonfinite or denormal value".into()));
        }
        Ok(())
    }

    fn finish<T>(&mut self, result: StatusAnd<T>, rounded_up: bool) -> Result<T, Fault> {
        let mut flags = 0;
        for (ap, x87) in [
            (Status::INVALID_OP, Self::IE),
            (Status::DIV_BY_ZERO, Self::ZE),
            (Status::OVERFLOW, Self::OE),
            (Status::UNDERFLOW, Self::UE),
            (Status::INEXACT, Self::PE),
        ] {
            if result.status.contains(ap) {
                flags |= x87;
            }
        }
        self.status &= !Self::C1;
        self.record(flags);
        if flags & !Self::PE != 0 {
            return Err(Fault(format!(
                "x87 exception status={flags:#04x} (fail-closed, including masked exceptions)"
            )));
        }
        if rounded_up {
            self.status |= Self::C1;
        }
        Ok(result.value)
    }

    fn load_integer(&mut self, value: i128) -> Result<(), Fault> {
        // Only i16/i32/i64 wrappers call this, so conversion is always exact.
        let result = X87DoubleExtended::from_i128(value);
        let value = self.finish(result, false)?;
        self.push(value)
    }

    fn load_real<T: Float + FloatConvert<X87DoubleExtended>>(
        &mut self,
        value: T,
    ) -> Result<(), Fault> {
        if value.is_denormal() {
            self.record(Self::DE);
        }
        if value.is_signaling() {
            self.record(Self::IE);
        }
        Self::supported(value)?;
        let result = value.convert_r(self.rounding(), &mut false);
        let value = self.finish(result, false)?;
        self.push(value)
    }

    fn store_integer(&mut self, index: usize, width: usize) -> Result<i128, Fault> {
        let value = self.get(index)?;
        // Width is restricted by private callers to 16, 32 or 64.
        let result = value.to_i128_r(width, self.rounding(), &mut false);
        let rounded_up = result.status.contains(Status::INEXACT)
            && X87DoubleExtended::from_i128(result.value).value.abs() > value.abs();
        self.finish(result, rounded_up)
    }

    fn store_real<T: Float + FloatConvert<X87DoubleExtended>>(
        &mut self,
        index: usize,
    ) -> Result<T, Fault>
    where
        X87DoubleExtended: FloatConvert<T>,
    {
        let value = self.get(index)?;
        let mut result: StatusAnd<T> = value.convert_r(self.rounding(), &mut false);
        if result.value.is_largest()
            && result.status == Status::INEXACT
            && value.ilogb() > T::MAX_EXP
        {
            result.status |= Status::OVERFLOW;
        }
        let widened: StatusAnd<X87DoubleExtended> = result.value.convert(&mut false);
        let rounded_up =
            result.status.contains(Status::INEXACT) && widened.value.abs() > value.abs();
        let output = self.finish(result, rounded_up)?;
        Self::supported(output)?;
        Ok(output)
    }

    fn calculate<T: Float>(op: BinaryOp, lhs: T, rhs: T, round: Round) -> StatusAnd<T> {
        match op {
            BinaryOp::Add => lhs.add_r(rhs, round),
            BinaryOp::Sub => lhs.sub_r(rhs, round),
            BinaryOp::SubReverse => rhs.sub_r(lhs, round),
            BinaryOp::Mul => lhs.mul_r(rhs, round),
            BinaryOp::Div => lhs.div_r(rhs, round),
            BinaryOp::DivReverse => rhs.div_r(lhs, round),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: u128 = 0x3fff_8000_0000_0000_0000;
    const NEGATIVE_ZERO: u128 = 0x8000_0000_0000_0000_0000;
    const SIGN: u128 = 1 << 79;

    fn ext(bits: u128) -> X87DoubleExtended {
        X87DoubleExtended::from_bits(bits)
    }

    fn with_rc(rc: u16) -> Fpu {
        let mut fpu = Fpu::default();
        fpu.set_control_word(0x037f | (rc << 10)).unwrap();
        fpu
    }

    #[test]
    fn reset_stack_top_and_physical_tags() {
        let mut fpu = Fpu::default();
        assert_eq!(fpu.control_word(), 0x037f);
        assert_eq!(fpu.status_word(), 0);
        assert_eq!(fpu.tag_word(), 0xffff);
        fpu.load_i16(0).unwrap();
        assert_eq!(fpu.top(), 7);
        assert_eq!(fpu.status_word(), 0x3800);
        assert_eq!(fpu.tag_word(), 0x7fff); // physical R7 zero, others empty
        fpu.load_i16(1).unwrap();
        assert_eq!(fpu.tag_word(), 0x4fff); // R6 valid, R7 zero
        fpu.exchange(1).unwrap();
        assert_eq!(fpu.tag_word(), 0x1fff);
        assert_eq!(fpu.pop().unwrap().to_bits(), 0);
        assert_eq!(fpu.pop().unwrap().to_bits(), ONE);
        assert_eq!(fpu.top(), 0);
        assert_eq!(fpu.tag_word(), 0xffff);
        fpu.init();
        assert_eq!(fpu.status_word(), 0);
    }

    #[test]
    fn full_empty_holes_and_invalid_indices_fail_atomically() {
        let mut fpu = Fpu::default();
        assert!(fpu.pop().is_err());
        assert_eq!(fpu.status_word(), Fpu::IE | Fpu::SF);
        fpu.clear_exceptions();
        for value in 0..8 {
            fpu.load_i16(value).unwrap();
        }
        assert_eq!(fpu.top(), 0);
        assert!(fpu.load_i16(99).is_err());
        assert_eq!(fpu.status_word(), Fpu::IE | Fpu::SF | Fpu::C1);
        assert_eq!(fpu.get(0).unwrap().to_bits(), 0x4001_e000_0000_0000_0000);
        fpu.free(3).unwrap();
        let before = fpu.get(0).unwrap().to_bits();
        assert!(fpu.exchange(3).is_err());
        assert_eq!(fpu.get(0).unwrap().to_bits(), before);
        assert_eq!(fpu.status_word() & Fpu::C1, 0);
        assert!(fpu.get(usize::MAX).is_err());
        assert!(fpu.set(8, ext(ONE)).is_err());
        assert!(fpu.exchange(8).is_err());
        assert!(fpu.free(8).is_err());
        assert!(fpu.binary_registers(BinaryOp::Add, 8, 0).is_err());
        assert!(fpu.compare_registers(8, 0).is_err());
    }

    #[test]
    fn unsupported_precision_rejected_without_control_change() {
        let mut fpu = Fpu::default();
        for pc in 0..3 {
            assert!(fpu.set_control_word(0x007f | (pc << 8)).is_err());
            assert_eq!(fpu.control_word(), 0x037f);
        }
    }

    #[test]
    fn exact_integer_loads_seed_and_signed_endpoints() {
        let mut fpu = Fpu::default();
        fpu.load_i16(-32768).unwrap();
        assert_eq!(fpu.store_i16(0).unwrap(), i16::MIN);
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0xc00e_8000_0000_0000_0000);
        fpu.load_i32(i32::MAX).unwrap();
        assert_eq!(fpu.store_i32(0).unwrap(), i32::MAX);
        fpu.load_i64(i64::MIN).unwrap();
        assert_eq!(fpu.store_i64(0).unwrap(), i64::MIN);
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0xc03e_8000_0000_0000_0000);
        fpu.load_i64(i64::MAX).unwrap();
        assert_eq!(fpu.store_i64(0).unwrap(), i64::MAX);
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0x403d_ffff_ffff_ffff_fffe);
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, 0);
    }

    #[test]
    fn extended_two_to_63_plus_one_preserved() {
        let mut fpu = Fpu::default();
        fpu.load_f80_bits(0x403e_8000_0000_0000_0000).unwrap();
        fpu.add(0, ext(ONE)).unwrap();
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0x403e_8000_0000_0000_0001);
        fpu.sub(0, ext(0x403e_8000_0000_0000_0000)).unwrap();
        assert_eq!(fpu.store_i16(0).unwrap(), 1);
        assert_eq!(fpu.status_word() & Fpu::PE, 0);
    }

    #[test]
    fn fist_all_roundings_ties_signs_and_c1() {
        // Original bit fixtures, expected integers derived from RC definitions.
        let cases = [
            (0x3fff_c000_0000_0000_0000, [2, 1, 2, 1]), // 1.5
            (0xbfff_c000_0000_0000_0000, [-2, -2, -1, -1]),
            (0x4000_a000_0000_0000_0000, [2, 2, 3, 2]), // 2.5 ties even
            (0xc000_a000_0000_0000_0000, [-2, -3, -2, -2]),
            (0x3ffd_8000_0000_0000_0000, [0, 0, 1, 0]), // 0.25
            (0xbffd_8000_0000_0000_0000, [0, -1, 0, 0]),
        ];
        for (bits, expected) in cases {
            for rc in 0..4 {
                let mut fpu = with_rc(rc);
                fpu.load_f80_bits(bits).unwrap();
                let actual = fpu.store_i16(0).unwrap();
                assert_eq!(
                    actual,
                    expected[usize::from(rc)],
                    "bits={bits:020x} rc={rc}"
                );
                let up =
                    X87DoubleExtended::from_i128(i128::from(actual)).value.abs() > ext(bits).abs();
                assert_eq!(fpu.status_word() & Fpu::C1 != 0, up);
                assert_ne!(fpu.status_word() & Fpu::PE, 0);
                assert_eq!(fpu.get(0).unwrap().to_bits(), bits); // no pop or mutation
            }
        }
    }

    #[test]
    fn integer_overflow_is_invalid_not_clamped() {
        for (bits, width) in [
            (0x400e_8000_0000_0000_0000, 16), // +32768
            (0x401e_8000_0000_0000_0000, 32), // +2^31
            (0x403e_8000_0000_0000_0000, 64), // +2^63
            (0xc03e_8000_0000_0000_0001, 64), // -2^63-1
        ] {
            let mut fpu = Fpu::default();
            fpu.load_f80_bits(bits).unwrap();
            assert!(fpu.store_integer(0, width).is_err());
            assert_ne!(fpu.status_word() & Fpu::IE, 0);
            assert_eq!(fpu.get(0).unwrap().to_bits(), bits);
        }
    }

    #[test]
    fn extended_add_rounds_all_four_modes_in_both_signs() {
        // 1 + 2^-64 is exactly halfway between successive 64-bit significands.
        let half_ulp = 0x3fbf_8000_0000_0000_0000;
        for negative in [false, true] {
            let sign = if negative { SIGN } else { 0 };
            for rc in 0..4 {
                let mut fpu = with_rc(rc);
                fpu.load_f80_bits(ONE | sign).unwrap();
                fpu.add(0, ext(half_ulp | sign)).unwrap();
                let up = (!negative && rc == 2) || (negative && rc == 1);
                assert_eq!(fpu.get(0).unwrap().to_bits(), (ONE + u128::from(up)) | sign);
                assert_eq!(fpu.status_word() & Fpu::C1 != 0, up);
                assert_ne!(fpu.status_word() & Fpu::PE, 0);
            }
        }
    }

    #[test]
    fn real_load_store_bits_and_little_endian_round_trip() {
        let mut fpu = Fpu::default();
        fpu.load_f32_bits(0x3fc00000).unwrap(); // 1.5
        assert_eq!(fpu.store_f64_bits(0).unwrap(), 0x3ff8000000000000);
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0x3fff_c000_0000_0000_0000);
        let bytes = fpu.store_f80_bytes(0).unwrap();
        assert_eq!(bytes, [0, 0, 0, 0, 0, 0, 0, 0xc0, 0xff, 0x3f]);
        fpu.load_f80_bytes(bytes).unwrap();
        assert_eq!(fpu.store_f32_bits(0).unwrap(), 0x3fc00000);
        fpu.load_f64_bits(0x3ff0000000000001).unwrap();
        assert_eq!(fpu.store_f80_bits(0).unwrap(), 0x3fff_8000_0000_0000_0800);
    }

    #[test]
    fn float_stores_round_all_modes_without_binary64_intermediate() {
        for negative in [false, true] {
            for rc in 0..4 {
                let sign = if negative { SIGN } else { 0 };
                let up = (!negative && rc == 2) || (negative && rc == 1);
                let mut fpu = with_rc(rc);
                // 1 + 2^-24, halfway at single precision.
                fpu.load_f80_bits(0x3fff_8000_0080_0000_0000 | sign)
                    .unwrap();
                assert_eq!(
                    fpu.store_f32_bits(0).unwrap(),
                    0x3f800000 | (u32::from(negative) << 31) | u32::from(up)
                );
                assert_eq!(fpu.status_word() & Fpu::C1 != 0, up);
                // 1 + 2^-53, halfway at double precision.
                fpu.set(0, ext(0x3fff_8000_0000_0000_0400 | sign)).unwrap();
                assert_eq!(
                    fpu.store_f64_bits(0).unwrap(),
                    0x3ff0000000000000 | (u64::from(negative) << 63) | u64::from(up)
                );
                assert_eq!(fpu.status_word() & Fpu::C1 != 0, up);
                assert_ne!(fpu.status_word() & Fpu::PE, 0);
            }
        }
    }

    #[test]
    fn arithmetic_reverse_and_compare_flags() {
        let mut fpu = Fpu::default();
        fpu.load_i16(6).unwrap();
        fpu.load_i16(2).unwrap();
        fpu.binary_registers(BinaryOp::SubReverse, 0, 1).unwrap(); // 6 - 2
        assert_eq!(fpu.store_i16(0).unwrap(), 4);
        fpu.div_reverse(0, ext(0x4003_8000_0000_0000_0000)).unwrap(); // 16 / 4
        assert_eq!(fpu.store_i16(0).unwrap(), 4);
        fpu.mul(0, ext(0x4000_8000_0000_0000_0000)).unwrap();
        assert_eq!(fpu.store_i16(0).unwrap(), 8);
        for (rhs, order, flags) in [
            (9, Ordering::Less, Fpu::C0),
            (8, Ordering::Equal, Fpu::C3),
            (7, Ordering::Greater, 0),
        ] {
            assert_eq!(
                fpu.compare(0, X87DoubleExtended::from_i128(rhs).value)
                    .unwrap(),
                order
            );
            assert_eq!(fpu.status_word() & (Fpu::COMPARE | Fpu::C1), flags);
        }
    }

    #[test]
    fn signed_zero_load_arithmetic_store_and_compare() {
        for rc in 0..4 {
            let mut fpu = with_rc(rc);
            fpu.load_f32_bits(0x80000000).unwrap();
            assert_eq!(fpu.store_f80_bits(0).unwrap(), NEGATIVE_ZERO);
            assert_eq!(fpu.store_f64_bits(0).unwrap(), 0x8000000000000000);
            assert_eq!(fpu.store_i16(0).unwrap(), 0);
            assert_eq!(fpu.status_word() & Fpu::PE, 0);
            assert_eq!(fpu.compare(0, ext(0)).unwrap(), Ordering::Equal);
            fpu.change_sign().unwrap();
            assert_eq!(fpu.store_f32_bits(0).unwrap(), 0);
            fpu.set(0, ext(ONE)).unwrap();
            fpu.sub(0, ext(ONE)).unwrap();
            assert_eq!(
                fpu.get(0).unwrap().to_bits(),
                if rc == 1 { NEGATIVE_ZERO } else { 0 }
            );
            fpu.set(0, ext(NEGATIVE_ZERO)).unwrap();
            fpu.mul(0, ext(ONE)).unwrap();
            assert_eq!(fpu.get(0).unwrap().to_bits(), NEGATIVE_ZERO);
            fpu.abs().unwrap();
            assert_eq!(fpu.get(0).unwrap().to_bits(), 0);
        }
    }

    #[test]
    fn exceptional_arithmetic_fails_closed_masked_and_unmasked() {
        for masked in [true, false] {
            let mut fpu = Fpu::default();
            if !masked {
                fpu.set_control_word(0x0300).unwrap();
            }
            fpu.load_i16(1).unwrap();
            assert!(fpu.div(0, ext(0)).is_err());
            assert_eq!(fpu.get(0).unwrap().to_bits(), ONE);
            assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::ZE);
            assert_eq!(fpu.status_word() & (Fpu::ES | Fpu::B) != 0, !masked);
            fpu.clear_exceptions();
            fpu.set(0, ext(0)).unwrap();
            assert!(fpu.div(0, ext(0)).is_err());
            assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::IE);
            fpu.clear_exceptions();
            assert_eq!(
                fpu.status_word() & (Fpu::EXCEPTIONS | Fpu::ES | Fpu::B | Fpu::SF),
                0
            );
        }
    }

    #[test]
    fn mask_summary_and_nonfatal_precision() {
        let mut fpu = Fpu::default();
        fpu.load_f80_bits(0x3fff_c000_0000_0000_0000).unwrap();
        assert_eq!(fpu.store_i16(0).unwrap(), 2);
        assert_eq!(fpu.status_word() & (Fpu::ES | Fpu::B), 0);
        fpu.set_control_word(0x035f).unwrap(); // unmask precision, retain PE
        assert_eq!(fpu.status_word() & (Fpu::ES | Fpu::B), Fpu::ES | Fpu::B);
        assert_eq!(fpu.store_i16(0).unwrap(), 2); // documented nonfatal PE policy
        fpu.set_control_word(0x037f).unwrap();
        assert_eq!(fpu.status_word() & (Fpu::ES | Fpu::B), 0);
        let top_and_c1 = fpu.status_word() & (0x3800 | Fpu::C1);
        fpu.clear_exceptions();
        assert_eq!(fpu.status_word(), top_and_c1);
    }

    #[test]
    fn malformed_and_special_values_cannot_enter_stack() {
        let mut fpu = Fpu::default();
        for bits in [
            1,
            0x8000_0000_0000_0000,
            0x3fff_0000_0000_0000_0001,
            0x7fff_8000_0000_0000_0000,
            0x7fff_c000_0000_0000_0000,
            1 << 80,
        ] {
            assert!(fpu.load_f80_bits(bits).is_err(), "{bits:032x}");
            assert_eq!(fpu.top(), 0);
            assert_eq!(fpu.tag_word(), 0xffff);
        }
        assert!(fpu.load_f32_bits(1).is_err());
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::DE);
        fpu.clear_exceptions();
        assert!(fpu.load_f64_bits(0x7ff0000000000001).is_err());
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::IE);
        assert!(fpu.push(X87DoubleExtended::NAN).is_err());
        assert!(fpu.set(0, X87DoubleExtended::INFINITY).is_err());
        assert_eq!(fpu.tag_word(), 0xffff);
    }

    #[test]
    fn hardware_tininess_boundary_fixtures() {
        // Independent x87 oracle: min-normal / (1 + 2^-63), RC=up,
        // rounds back to min-normal but still raises UE|PE for arithmetic.
        let mut fpu = with_rc(2);
        let min_normal = 0x0001_8000_0000_0000_0000;
        fpu.load_f80_bits(min_normal).unwrap();
        assert!(fpu.div(0, ext(ONE + 1)).is_err());
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::UE | Fpu::PE);
        assert_eq!(fpu.get(0).unwrap().to_bits(), min_normal);

        // The FST conversion boundary differs: a value just below the target
        // minimum normal that rounds up to it reports PE but not UE on x87.
        let mut fpu = Fpu::default();
        fpu.load_f80_bits(0x3f80_ffff_ffff_ffff_ffff).unwrap();
        assert_eq!(fpu.store_f32_bits(0).unwrap(), 0x00800000);
        assert_eq!(fpu.status_word() & 0x023f, Fpu::PE | Fpu::C1);
        fpu.clear_exceptions();
        fpu.set(0, ext(0x3c00_ffff_ffff_ffff_ffff)).unwrap();
        assert_eq!(fpu.store_f64_bits(0).unwrap(), 0x0010000000000000);
        assert_eq!(fpu.status_word() & 0x023f, Fpu::PE | Fpu::C1);
    }

    #[test]
    fn directed_overflow_sets_oe_instead_of_silently_saturating() {
        // Regression for APFloat 0.2.3 overflow_result(TowardZero/Negative),
        // which reports only INEXACT although x87 also sets overflow.
        let largest = 0x7ffe_ffff_ffff_ffff_ffff;
        for sign in [0, SIGN] {
            for rc in 0..4 {
                let mut fpu = with_rc(rc);
                fpu.load_f80_bits(largest | sign).unwrap();
                assert!(fpu.mul(0, ext(0x4000_8000_0000_0000_0000)).is_err());
                assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::OE | Fpu::PE);
                assert_eq!(fpu.get(0).unwrap().to_bits(), largest | sign);
                fpu.clear_exceptions();
                fpu.set(0, ext(0x407f_8000_0000_0000_0000 | sign)).unwrap(); // 2^128
                assert!(fpu.store_f32_bits(0).is_err());
                assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::OE | Fpu::PE);
                fpu.clear_exceptions();
                fpu.set(0, ext(0x43ff_8000_0000_0000_0000 | sign)).unwrap(); // 2^1024
                assert!(fpu.store_f64_bits(0).is_err());
                assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::OE | Fpu::PE);
            }
        }
        // A merely inexact largest-finite result is NOT necessarily overflow:
        // max + 1 rounded toward zero stays at max and must set only PE.
        let mut fpu = with_rc(3);
        fpu.load_f80_bits(largest).unwrap();
        fpu.add(0, ext(ONE)).unwrap();
        assert_eq!(fpu.get(0).unwrap().to_bits(), largest);
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::PE);
    }

    #[test]
    fn overflow_underflow_and_denormal_results_never_commit() {
        let mut fpu = Fpu::default();
        let largest = 0x7ffe_ffff_ffff_ffff_ffff;
        fpu.load_f80_bits(largest).unwrap();
        assert!(fpu.mul(0, ext(0x4000_8000_0000_0000_0000)).is_err());
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::OE | Fpu::PE);
        assert_eq!(fpu.get(0).unwrap().to_bits(), largest);
        fpu.clear_exceptions();
        fpu.set(0, ext(0x0001_8000_0000_0000_0000)).unwrap();
        assert!(fpu.div(0, ext(0x4000_c000_0000_0000_0000)).is_err()); // min / 3
        assert_eq!(fpu.status_word() & Fpu::EXCEPTIONS, Fpu::UE | Fpu::PE);
        fpu.clear_exceptions();
        assert!(fpu.div(0, ext(0x4000_8000_0000_0000_0000)).is_err()); // exact denormal
        assert_eq!(fpu.get(0).unwrap().to_bits(), 0x0001_8000_0000_0000_0000);
        fpu.set(0, ext(0x407f_8000_0000_0000_0000)).unwrap(); // 2^128
        assert!(fpu.store_f32_bits(0).is_err());
        assert_ne!(fpu.status_word() & Fpu::OE, 0);
        fpu.set(0, ext(0x3f6a_8000_0000_0000_0000)).unwrap(); // 2^-149, exact subnormal single
        assert!(fpu.store_f32_bits(0).is_err());
    }
}
