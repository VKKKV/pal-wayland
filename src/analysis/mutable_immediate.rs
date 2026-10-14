// SPDX-License-Identifier: GPL-3.0-or-later
//! Analysis-only descriptions of explicitly writable MOV immediate bytes.
//!
//! A slot is NOT permission to stop guarding an instruction. The caller must
//! opt in each confirmed instruction address, retain byte-for-byte guards over
//! every byte outside [`ImmediateSlot::byte_range`], and replace ONLY the named
//! source operand with [`emit_read`]. No opcode, prefix, ModR/M, displacement,
//! length, destination, instruction semantics, or static control-flow target
//! may change. Other/overlapping instruction interpretations keep their own
//! guards; neither physical aliases nor additional source targets are inferred.
//!
//! Slots describe current guest memory, not cached parameters: byte stores,
//! word stores and segment aliases must all remain visible through the normal
//! memory API. Evaluate the emitted expression while CS:IP still identifies
//! the guarded instruction, before changing registers or writing its result.
//! Decoding is exclusively an analysis operation; the emitted expression does
//! not contain a decoder or an instruction dispatcher.
//!
//! The first version accepts only ordinary 16-bit MOV r/m8,imm8 and
//! MOV r/m16,imm16 (including opcode-encoded registers). Sign-extended
//! immediates, arithmetic, shifts, branches/far transfers, FPU macros, operand-
//! or address-size overrides, LOCK/REP and FS/GS prefixes fail closed. Extending
//! this whitelist requires independent semantic/codegen regressions, not just
//! discovering a constant offset in an otherwise unsupported instruction.

#![cfg(feature = "analysis")]

use iced_x86::{
    Code, CodeSize, Decoder, DecoderError, DecoderOptions, FlowControl, Instruction, OpKind,
    Register,
};
use std::ops::Range;

/// A validated source-immediate span, relative to the instruction's first byte.
///
/// Fields are private so a caller cannot turn metadata into an arbitrary guard
/// exemption. Construct through [`analyze`]; use this same slot for both the
/// guard exception and generated operand read. A slot is tied to the analyzed
/// instruction: do not reuse it to exempt a different node's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImmediateSlot {
    byte_offset: usize,
    byte_len: usize,
    operand_index: u32,
    sign_extended: bool,
    instruction_len: usize,
}

impl ImmediateSlot {
    pub const fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    pub const fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub const fn operand_index(&self) -> u32 {
        self.operand_index
    }

    /// Always false for this MOV-only version; imm8-to-16 is rejected.
    pub const fn sign_extended(&self) -> bool {
        self.sign_extended
    }

    pub const fn instruction_len(&self) -> usize {
        self.instruction_len
    }

    pub fn byte_range(&self) -> Range<usize> {
        self.byte_offset..self.byte_offset + self.byte_len
    }

    /// Both half-open spans MUST remain guarded. The second span is empty for
    /// currently supported MOV encodings, since their immediate is last.
    pub fn immutable_ranges(&self) -> [Range<usize>; 2] {
        [
            0..self.byte_offset,
            self.byte_offset + self.byte_len..self.instruction_len,
        ]
    }
}

/// Validate one exact, originally decoded instruction and identify its source
/// immediate. `original_bytes` must contain the entire instruction and no
/// trailing bytes. The instruction must have been decoded in 16-bit mode with
/// `DecoderOptions::NONE`, at its segment-relative IP (not a physical address).
///
/// Fresh decoding and an all-fields comparison reject stale/mismatched IR;
/// iced's constant offsets, not guessed opcode lengths, identify the span.
/// This function establishes encoding eligibility, not evidence that a given
/// address is code or that the original program writes the immediate there.
/// Those remain explicit caller obligations.
pub fn analyze(instruction: &Instruction, original_bytes: &[u8]) -> Result<ImmediateSlot, String> {
    if original_bytes.is_empty()
        || original_bytes.len() > 15
        || original_bytes.len() != instruction.len()
    {
        return Err(
            "mutable immediate requires exactly one complete instruction (1..=15 bytes)".into(),
        );
    }
    if instruction.code_size() != CodeSize::Code16 || instruction.is_invalid() {
        return Err("mutable immediate requires a valid instruction decoded in 16-bit mode".into());
    }
    if instruction.ip() > u64::from(u16::MAX)
        || instruction.ip() + original_bytes.len() as u64 > 0x10000
    {
        return Err(
            "mutable immediate instruction crosses or lies outside the 64-KiB code segment".into(),
        );
    }

    let mut decoder = Decoder::with_ip(16, original_bytes, instruction.ip(), DecoderOptions::NONE);
    let decoded = decoder.decode();
    if decoder.last_error() != DecoderError::None
        || decoder.position() != original_bytes.len()
        || decoded.op_count() != instruction.op_count()
        || !decoded.eq_all_bits(instruction)
    {
        return Err(
            "mutable immediate original bytes do not match the supplied decoded instruction".into(),
        );
    }
    // A byte-sized MOV can retain an otherwise ineffective 66/67 prefix. Check
    // raw prefixes as well as decoded width so these cannot sneak through.
    for &byte in original_bytes {
        match byte {
            0x26 | 0x2e | 0x36 | 0x3e => (),
            0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3 => {
                return Err(format!("unsupported mutable-immediate prefix {byte:02x}"));
            }
            _ => break,
        }
    }

    let (kind, byte_len) = match decoded.code() {
        Code::Mov_r8_imm8 | Code::Mov_rm8_imm8 => (OpKind::Immediate8, 1),
        Code::Mov_r16_imm16 | Code::Mov_rm16_imm16 => (OpKind::Immediate16, 2),
        _ => return Err("mutable immediate supports only MOV r/m8,imm8 or MOV r/m16,imm16".into()),
    };
    if decoded.flow_control() != FlowControl::Next
        || decoded.op_count() != 2
        || decoded.op1_kind() != kind
        || !matches!(decoded.op0_kind(), OpKind::Register | OpKind::Memory)
    {
        return Err("unsupported mutable-immediate operand layout".into());
    }
    if decoded.op0_kind() == OpKind::Memory
        && (!matches!(
            decoded.memory_segment(),
            Register::ES | Register::CS | Register::SS | Register::DS
        ) || ![decoded.memory_base(), decoded.memory_index()]
            .into_iter()
            .all(|r| {
                matches!(
                    r,
                    Register::None | Register::BX | Register::BP | Register::SI | Register::DI
                )
            })
            || decoded.memory_index_scale() != 1
            || decoded.memory_size().size() != byte_len)
    {
        return Err("mutable immediate requires an ordinary 16-bit memory operand".into());
    }

    let offsets = decoder.get_constant_offsets(&decoded);
    let byte_offset = offsets.immediate_offset();
    if !offsets.has_immediate()
        || offsets.has_immediate2()
        || offsets.immediate_size() != byte_len
        || byte_offset == 0
        || byte_offset + byte_len != original_bytes.len()
        || (offsets.has_displacement()
            && offsets.displacement_offset() + offsets.displacement_size() > byte_offset)
    {
        return Err("mutable immediate has inconsistent constant-offset metadata".into());
    }
    Ok(ImmediateSlot {
        byte_offset,
        byte_len,
        operand_index: 1,
        sign_extended: false,
        instruction_len: original_bytes.len(),
    })
}

/// Emit a fallible `u8`/`u16` expression using the generator's `m: &mut Machine`
/// and `SegmentedAddress` names. Read from CURRENT CS:IP plus the validated byte
/// offset, never DS, the MOV destination's segment override, or a cached value.
/// The caller must guard immutable bytes before evaluating this expression.
pub fn emit_read(slot: &ImmediateSlot) -> String {
    format!(
        "m.read{}(SegmentedAddress::new(m.regs.cs, m.regs.ip.wrapping_add({}u16)))?",
        slot.byte_len * 8,
        slot.byte_offset,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8], ip: u16) -> Instruction {
        Decoder::with_ip(16, bytes, u64::from(ip), DecoderOptions::NONE).decode()
    }

    fn slot(bytes: &[u8]) -> ImmediateSlot {
        analyze(&decode(bytes, 0x1234), bytes).unwrap()
    }

    #[test]
    fn opcode_register_mov_byte_and_word_all_registers() {
        for register in 0..8 {
            let byte = [0xb0 + register, 0x80];
            let word = [0xb8 + register, 0x80, 0xfe];
            for (bytes, expected_len) in [(byte.as_slice(), 1), (word.as_slice(), 2)] {
                let s = slot(bytes);
                assert_eq!(s.byte_offset(), 1);
                assert_eq!(s.byte_len(), expected_len);
                assert_eq!(s.operand_index(), 1);
                assert!(!s.sign_extended());
                assert_eq!(s.instruction_len(), bytes.len());
                assert_eq!(s.byte_range(), 1..bytes.len());
                assert_eq!(s.immutable_ranges(), [0..1, bytes.len()..bytes.len()]);
            }
        }
    }

    #[test]
    fn modrm_register_mov_is_supported_without_treating_modrm_as_immediate() {
        for register in 0..8 {
            let byte = [0xc6, 0xc0 + register, 0x90];
            let word = [0xc7, 0xc0 + register, 0x90, 0x12];
            assert_eq!(slot(&byte).byte_range(), 2..3);
            assert_eq!(slot(&word).byte_range(), 2..4);
        }
    }

    #[test]
    fn displacement_and_segment_prefixes_never_join_the_mutable_span() {
        let cases: &[(&[u8], Range<usize>)] = &[
            (&[0xc6, 0x05, 0x81], 2..3),                         // byte [DI],imm8
            (&[0x2e, 0xc6, 0x05, 0x12], 3..4),                   // byte CS:[DI],imm8
            (&[0xc6, 0x46, 0xfe, 0x81], 3..4),                   // byte [BP-2],imm8
            (&[0xc7, 0x06, 0x34, 0x12, 0x78, 0x56], 4..6),       // word [1234],imm16
            (&[0x26, 0xc7, 0x83, 0x00, 0x80, 0xff, 0xff], 5..7), // ES:[BP+DI-8000h]
            (&[0x36, 0x2e, 0xc7, 0x46, 0x80, 0x00, 0x80], 5..7), // repeated overrides
            (&[0x3e, 0xb8, 0x00, 0x80], 2..4), // ignored DS override still guarded
        ];
        for (bytes, expected) in cases {
            let s = slot(bytes);
            assert_eq!(s.byte_range(), *expected, "{bytes:02x?}");
            assert_eq!(
                s.immutable_ranges(),
                [0..expected.start, expected.end..bytes.len()]
            );
            for index in 0..bytes.len() {
                assert_eq!(
                    s.immutable_ranges().iter().any(|r| r.contains(&index)),
                    !expected.contains(&index),
                );
            }
        }
    }

    #[test]
    fn every_16_bit_mov_modrm_address_shape_uses_iced_offsets() {
        for opcode in [0xc6, 0xc7] {
            for mode in 0..4u8 {
                for rm in 0..8u8 {
                    for prefix in [None, Some(0x26), Some(0x2e), Some(0x36), Some(0x3e)] {
                        let mut bytes: Vec<u8> = prefix.into_iter().collect();
                        bytes.extend_from_slice(&[opcode, mode << 6 | rm]);
                        let displacement_len = match (mode, rm) {
                            (0, 6) | (2, _) => 2,
                            (1, _) => 1,
                            _ => 0,
                        };
                        bytes.extend(std::iter::repeat_n(0x80, displacement_len));
                        let start = bytes.len();
                        let immediate_len = if opcode == 0xc6 { 1 } else { 2 };
                        bytes.extend(std::iter::repeat_n(0xa5, immediate_len));
                        assert_eq!(
                            slot(&bytes).byte_range(),
                            start..bytes.len(),
                            "{bytes:02x?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn arbitrary_immediate_values_preserve_the_exact_layout() {
        // Independent decode of mutated bytes checks structural invariants;
        // it is a build-time regression, never an emitted runtime decoder.
        let byte = [0x2e, 0xc6, 0x45, 0xfe, 0x00];
        let original = decode(&byte, 0x1234);
        let expected = slot(&byte);
        for value in 0..=u8::MAX {
            let mut changed = byte;
            changed[4] = value;
            let decoded = decode(&changed, 0x1234);
            assert_eq!(analyze(&decoded, &changed).unwrap(), expected);
            assert_eq!(decoded.code(), original.code());
            assert_eq!(
                decoded.memory_displacement64(),
                original.memory_displacement64()
            );
            assert_eq!(decoded.memory_segment(), original.memory_segment());
            assert_eq!(decoded.immediate8(), value);
        }
        let word = [0xc7, 0x86, 0xff, 0xff, 0x00, 0x00];
        let expected = slot(&word);
        for value in 0..=u16::MAX {
            let mut changed = word;
            changed[4..].copy_from_slice(&value.to_le_bytes());
            let decoded = decode(&changed, 0x1234);
            assert_eq!(analyze(&decoded, &changed).unwrap(), expected);
            assert_eq!(decoded.immediate16(), value);
        }
    }

    #[test]
    fn rejects_branches_shifts_sign_extension_and_non_mov_immediates() {
        let cases: &[&[u8]] = &[
            &[0x83, 0xc0, 0x80],             // ADD AX,imm8 sign extended to 16
            &[0x05, 0x34, 0x12],             // ADD AX,imm16
            &[0x04, 0x80],                   // ADD AL,imm8
            &[0xc1, 0xe0, 0x02],             // SHL AX,imm8
            &[0xd1, 0xe0],                   // SHL AX,1: implicit immediate has no slot
            &[0xe8, 0x00, 0x00],             // CALL near
            &[0xe9, 0x00, 0x00],             // JMP near
            &[0xeb, 0x00],                   // JMP short
            &[0x74, 0x00],                   // JZ
            &[0x9a, 0x34, 0x12, 0x78, 0x56], // CALL far: two constants
            &[0xea, 0x34, 0x12, 0x78, 0x56], // JMP far
            &[0xc2, 0x04, 0x00],             // RET imm16
            &[0x6a, 0x80],                   // PUSH signed imm8
            &[0x68, 0x34, 0x12],             // PUSH imm16
            &[0xc8, 0x10, 0x00, 0x02],       // ENTER: two immediates
            &[0xcd, 0x35],                   // INT, not a MOV/FPU macro slot
            &[0x8b, 0xc7],                   // MOV AX,DI: no immediate
            &[0xa1, 0x34, 0x12],             // MOV AX,[1234]: displacement, not immediate
            &[0xa3, 0x34, 0x12],             // MOV [1234],AX
            &[0x90],                         // NOP
        ];
        assert_eq!(decode(cases[0], 0).op1_kind(), OpKind::Immediate8to16);
        for bytes in cases {
            assert!(
                analyze(&decode(bytes, 0x100), bytes).is_err(),
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_prefixes_even_if_they_do_not_change_operand_width() {
        for prefix in [0x64, 0x65, 0x66, 0x67, 0xf0, 0xf2, 0xf3] {
            for base in [&[0xb0, 0x80][..], &[0xc6, 0xc0, 0x80][..]] {
                let mut bytes = vec![0x2e, prefix];
                bytes.extend_from_slice(base);
                assert!(analyze(&decode(&bytes, 0), &bytes).is_err(), "{bytes:02x?}");
            }
        }
        let dword = [0x66, 0xb8, 0x78, 0x56, 0x34, 0x12];
        assert!(analyze(&decode(&dword, 0), &dword).is_err());
        let addr32 = [0x67, 0xc6, 0x00, 0x12];
        assert!(analyze(&decode(&addr32, 0), &addr32).is_err());
    }

    #[test]
    fn rejects_stale_ir_for_opcode_modrm_displacement_and_immediate_mutations() {
        let bytes = [0x2e, 0xc7, 0x86, 0x34, 0x12, 0x78, 0x56];
        let instruction = decode(&bytes, 0x200);
        for index in 0..bytes.len() {
            let mut changed = bytes;
            changed[index] ^= 1;
            assert!(analyze(&instruction, &changed).is_err(), "index={index}");
        }
        let mut wrong_register = decode(&[0xb8, 0x34, 0x12], 0x200);
        wrong_register.set_op0_register(Register::DI);
        assert!(analyze(&wrong_register, &[0xb8, 0x34, 0x12]).is_err());
        let mut wrong_operand = instruction;
        wrong_operand.set_op1_kind(OpKind::Register);
        assert!(analyze(&wrong_operand, &bytes).is_err());
        let mut wrong_width = instruction;
        wrong_width.set_code_size(CodeSize::Code32);
        assert!(analyze(&wrong_width, &bytes).is_err());
    }

    #[test]
    fn rejects_truncation_trailing_bytes_invalid_instruction_and_other_decode_modes() {
        let bytes = [0xb8, 0x34, 0x12];
        let instruction = decode(&bytes, 0);
        for end in 0..bytes.len() {
            assert!(analyze(&instruction, &bytes[..end]).is_err());
            assert!(analyze(&decode(&bytes[..end], 0), &bytes[..end]).is_err());
        }
        assert!(analyze(&instruction, &[0xb8, 0x34, 0x12, 0x90]).is_err());
        assert!(analyze(&Instruction::default(), &[]).is_err());
        for bitness in [32, 64] {
            let bytes = [0x66, 0xb8, 0x34, 0x12];
            let instruction = Decoder::with_ip(bitness, &bytes, 0, DecoderOptions::NONE).decode();
            assert!(analyze(&instruction, &bytes).is_err());
        }
        let too_long = [0x2e; 16];
        assert!(analyze(&decode(&too_long, 0), &too_long).is_err());
    }

    #[test]
    fn accepts_segment_end_but_rejects_crossing_and_physical_ip() {
        let bytes = [0xb8, 0x34, 0x12];
        assert!(analyze(&decode(&bytes, 0xfffd), &bytes).is_ok());
        assert!(analyze(&decode(&bytes, 0xfffe), &bytes).is_err());
        assert!(analyze(&decode(&bytes, 0xffff), &bytes).is_err());
        let physical_ip = Decoder::with_ip(16, &bytes, 0x10000, DecoderOptions::NONE).decode();
        assert!(analyze(&physical_ip, &bytes).is_err());
        let byte = [0xb0, 0x80];
        assert!(analyze(&decode(&byte, 0xfffe), &byte).is_ok());
    }

    #[test]
    fn fifteen_byte_prefixed_instruction_has_decoder_derived_offset() {
        let mut bytes = vec![0x2e; 12];
        bytes.extend_from_slice(&[0xb8, 0x34, 0x12]);
        let s = slot(&bytes);
        assert_eq!(s.byte_range(), 13..15);
        assert_eq!(s.immutable_ranges(), [0..13, 15..15]);
    }

    #[test]
    fn emits_current_cs_ip_reads_not_destination_segment_or_stale_literals() {
        assert_eq!(
            emit_read(&slot(&[0x26, 0xc6, 0x05, 0xff])),
            "m.read8(SegmentedAddress::new(m.regs.cs, m.regs.ip.wrapping_add(3u16)))?",
        );
        assert_eq!(
            emit_read(&slot(&[0x36, 0xc7, 0x86, 0x34, 0x12, 0x80, 0xff])),
            "m.read16(SegmentedAddress::new(m.regs.cs, m.regs.ip.wrapping_add(5u16)))?",
        );
        // Same instruction shape but different IP and immediate must produce
        // identical reads: the current memory bytes, not either static value.
        let a = [0xb8, 0x00, 0x00];
        let b = [0xb8, 0xff, 0xff];
        assert_eq!(
            emit_read(&analyze(&decode(&a, 0x100), &a).unwrap()),
            emit_read(&analyze(&decode(&b, 0x900), &b).unwrap()),
        );
    }
}
