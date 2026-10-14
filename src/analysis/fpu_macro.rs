// SPDX-License-Identifier: GPL-3.0-or-later
//! Static recovery of Borland's 16-bit INT 34h..3Dh inline FPU macros.
//!
//! This module belongs to analysis, not the generated runtime. Recognition is
//! conditional on the caller having established the Borland interrupt-vector
//! convention; arbitrary DOS programs can use these vectors for other purposes.
//! The input starts at the candidate instruction, NOT at the segment base.
//!
//! Recovery follows the observed hardware handlers: INT 34h..3Bh is rewritten
//! from `CD nn` to `9B (nn+A4h)`; INT 3Ch embeds segment/opcode information in
//! its next byte. The canonical transformation retains WAIT even where a
//! CPU-dependent handler may replace it with NOP. INT 3Dh is a synchronization
//! marker, not an x87 arithmetic instruction or an ordinary two-byte interrupt.
//!
//! Only the canonical x87 body is decoded by iced-x86. WAIT is kept separately
//! in `transformed_bytes`: decoding that byte as part of the body can return a
//! standalone WAIT and lose the inline ModR/M/displacement boundary. In
//! particular, FNSTCW/FNSTSW/FNCLEX/FNINIT bodies do not silently lose the
//! associated synchronization obligation.
//!
//! Validity here means iced-x86 1.21 accepts the D8h..DFh encoding in 16-bit
//! mode. It does NOT prove that a particular 8087/287/387 or Borland software
//! emulator implements it. Prefixes before INT, malformed encodings, truncated
//! operands and code fetches across a 64-KiB segment boundary fail explicitly.
//! There is no guest execution, self-patching, f64 approximation, or claim of
//! x87 precision, exceptions, flags, control/status or stack semantics.

#![cfg(feature = "analysis")]

use iced_x86::{Decoder, DecoderError, DecoderOptions, Instruction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroKind {
    /// An implicit synchronization obligation followed by one x87 instruction.
    X87,
    /// INT 3Dh: synchronization only. Must not be emitted as a runtime no-op.
    Sync,
}

#[derive(Clone, Debug)]
pub struct DecodedMacro {
    /// Original segment-relative macro address. The caller retains CS/physical
    /// address separately; this decoder never infers segment aliases.
    pub ip: u16,
    pub vector: u8,
    pub kind: MacroKind,
    /// Complete ORIGINAL length, including INT, embedded opcode, ModR/M and
    /// displacement. CFG fallthrough must use this, not `Instruction::len()`.
    pub len: usize,
    pub original_bytes: Vec<u8>,
    /// Canonical `WAIT; [segment prefix] x87` bytes. For X87 these have exactly
    /// the original length, but are not a claim about actual self-patched RAM.
    /// For Sync this is only `[9B]`, NOT a literal replacement for `CD 3D` (the
    /// hardware handler synchronizes out of line; the software path differs).
    pub transformed_bytes: Vec<u8>,
    /// X87 body WITHOUT the leading WAIT, including any recovered segment
    /// prefix. Its IP is `ip + 1`, its length is `len - 1`, and its next IP is
    /// the original macro's next IP. Sync has no x87 body and returns None.
    pub instruction: Option<Instruction>,
}

fn is_prefix(byte: u8) -> bool {
    matches!(
        byte,
        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
    )
}

fn require(bytes: &[u8], ip: u16, count: usize) -> Result<(), String> {
    let remaining = 0x10000 - usize::from(ip);
    if count > remaining {
        return Err(format!(
            "FPU macro at IP {ip:04x}: instruction crosses 64-KiB code segment boundary (needs {count} bytes, {remaining} remain)"
        ));
    }
    if count > bytes.len() {
        return Err(format!(
            "FPU macro at IP {ip:04x}: truncated instruction at memory/code-range boundary (needs {count} bytes, {} supplied)",
            bytes.len()
        ));
    }
    Ok(())
}

/// Recover one macro from bytes beginning at `ip`, without modifying the input.
///
/// `Ok(None)` means an ordinary instruction (including INT outside 34h..3Dh,
/// the one-byte opcode 34h, and already-patched native x87/WAIT). No search is
/// performed inside another instruction. An empty input, incomplete INT or
/// incomplete prefix sequence is an error, since it cannot rule out a macro.
/// Prefixed macros are rejected, not returned as ordinary INTs whose operand
/// bytes could then accidentally become CFG instructions.
///
/// Only bytes belonging to the recovered macro are consumed. The caller must
/// restrict the slice to a confirmed code/image range; segment limits are
/// independently enforced even if the slice contains the next segment's bytes.
/// A macro ending exactly at offset 10000h is accepted; the caller decides
/// whether the subsequent wrapping fallthrough is a confirmed code target.
/// All returned operations remain unsupported for production execution until
/// independently verified x87 semantics exist.
pub fn decode(bytes: &[u8], ip: u16) -> Result<Option<DecodedMacro>, String> {
    require(bytes, ip, 1)?;
    // Inspect prefixes only to reject prefixed macros. Do not normalize them:
    // the handlers rewind the return IP by exactly two bytes, not prefix+2.
    let mut start = 0;
    loop {
        require(bytes, ip, start + 1)?;
        if !is_prefix(bytes[start]) {
            break;
        }
        start += 1;
        if start == 15 {
            return Err(format!(
                "FPU macro probe at IP {ip:04x}: prefix sequence exceeds the x86 instruction length limit"
            ));
        }
    }
    if bytes[start] != 0xcd {
        return Ok(None);
    }
    require(bytes, ip, start + 2)?;
    let vector = bytes[start + 1];
    if !(0x34..=0x3d).contains(&vector) {
        return Ok(None);
    }
    if start != 0 {
        return Err(format!(
            "FPU macro at IP {ip:04x}: unsupported prefix before INT {vector:02x}h"
        ));
    }
    if vector == 0x3d {
        return Ok(Some(DecodedMacro {
            ip,
            vector,
            kind: MacroKind::Sync,
            len: 2,
            original_bytes: bytes[..2].to_vec(),
            transformed_bytes: vec![0x9b],
            instruction: None,
        }));
    }

    let (opcode, segment, modrm_at) = if vector == 0x3c {
        require(bytes, ip, 3)?;
        let encoded = bytes[2];
        let opcode = encoded | 0xc0;
        if !(0xd8..=0xdf).contains(&opcode) {
            return Err(format!(
                "FPU macro at IP {ip:04x}: invalid INT 3Ch segment/opcode byte {encoded:02x} (does not recover D8h..DFh)"
            ));
        }
        // The top two encoded bits select DS, SS, CS, ES respectively;
        // the handler obtains this prefix by SHR 3, NOT, AND 18h, OR 26h.
        let segment = ((!(encoded >> 3)) & 0x18) | 0x26;
        (opcode, Some(segment), 3)
    } else {
        (vector + 0xa4, None, 2)
    };
    require(bytes, ip, modrm_at + 1)?;
    let modrm = bytes[modrm_at];
    // In 16-bit addressing there is no SIB. Only mod=00,r/m=110 has a
    // displacement without a base, and mod=11 is always a register form.
    let displacement_len = match (modrm >> 6, modrm & 7) {
        (0, 6) | (2, _) => 2,
        (1, _) => 1,
        _ => 0,
    };
    let len = modrm_at + 1 + displacement_len;
    require(bytes, ip, len)?;
    let original_bytes = bytes[..len].to_vec();
    let mut transformed_bytes = original_bytes.clone();
    transformed_bytes[0] = 0x9b;
    if let Some(segment) = segment {
        transformed_bytes[1] = segment;
        transformed_bytes[2] = opcode;
    } else {
        transformed_bytes[1] = opcode;
    }
    let mut decoder = Decoder::with_ip(
        16,
        &transformed_bytes[1..],
        u64::from(ip) + 1,
        DecoderOptions::NONE,
    );
    let instruction = decoder.decode();
    if instruction.is_invalid() || decoder.last_error() != DecoderError::None {
        return Err(format!(
            "FPU macro at IP {ip:04x}: invalid or unsupported x87 encoding {opcode:02x} {modrm:02x} ({:?})",
            decoder.last_error()
        ));
    }
    if instruction.len() != len - 1 {
        return Err(format!(
            "FPU macro at IP {ip:04x}: x87 decoder length {} disagrees with recovered body length {}",
            instruction.len(),
            len - 1
        ));
    }
    Ok(Some(DecodedMacro {
        ip,
        vector,
        kind: MacroKind::X87,
        len,
        original_bytes,
        transformed_bytes,
        instruction: Some(instruction),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::{Code, Mnemonic, OpKind, Register};

    // Original ISA-based fixtures, not extracted from the game's code/data.
    // The expected bodies are written independently of the decoder's rewrite.
    struct Fixture {
        raw: &'static [u8],
        body: &'static [u8],
        code: Code,
        ndisasm: &'static str,
    }

    fn fixtures() -> Vec<Fixture> {
        vec![
            Fixture {
                raw: &[0xcd, 0x34, 0x43, 0xa5],
                body: &[0xd8, 0x43, 0xa5],
                code: Code::Fadd_m32fp,
                ndisasm: "fadd dword [bp+di-0x5b]",
            },
            Fixture {
                raw: &[0xcd, 0x35, 0xc5],
                body: &[0xd9, 0xc5],
                code: Code::Fld_sti,
                ndisasm: "fld st5",
            },
            Fixture {
                raw: &[0xcd, 0x36, 0x16, 0xb1, 0xa2],
                body: &[0xda, 0x16, 0xb1, 0xa2],
                code: Code::Ficom_m32int,
                ndisasm: "ficom dword [0xa2b1]",
            },
            Fixture {
                raw: &[0xcd, 0x37, 0x2a],
                body: &[0xdb, 0x2a],
                code: Code::Fld_m80fp,
                ndisasm: "fld tword [bp+si]",
            },
            Fixture {
                raw: &[0xcd, 0x38, 0x84, 0x57, 0x13],
                body: &[0xdc, 0x84, 0x57, 0x13],
                code: Code::Fadd_m64fp,
                ndisasm: "fadd qword [si+0x1357]",
            },
            Fixture {
                raw: &[0xcd, 0x39, 0x7e, 0xf1],
                body: &[0xdd, 0x7e, 0xf1],
                code: Code::Fnstsw_m2byte,
                ndisasm: "fnstsw [bp-0xf]",
            },
            Fixture {
                raw: &[0xcd, 0x3a, 0xc3],
                body: &[0xde, 0xc3],
                code: Code::Faddp_sti_st0,
                ndisasm: "faddp st3",
            },
            Fixture {
                raw: &[0xcd, 0x3b, 0x3c],
                body: &[0xdf, 0x3c],
                code: Code::Fistp_m64int,
                ndisasm: "fistp qword [si]",
            },
            Fixture {
                raw: &[0xcd, 0x3c, 0x19, 0x86, 0x24, 0x68],
                body: &[0x3e, 0xd9, 0x86, 0x24, 0x68],
                code: Code::Fld_m32fp,
                ndisasm: "fld dword [ds:bp+0x6824]",
            },
            Fixture {
                raw: &[0xcd, 0x3c, 0x59, 0x86, 0x24, 0x68],
                body: &[0x36, 0xd9, 0x86, 0x24, 0x68],
                code: Code::Fld_m32fp,
                ndisasm: "fld dword [ss:bp+0x6824]",
            },
            Fixture {
                raw: &[0xcd, 0x3c, 0x99, 0x86, 0x24, 0x68],
                body: &[0x2e, 0xd9, 0x86, 0x24, 0x68],
                code: Code::Fld_m32fp,
                ndisasm: "fld dword [cs:bp+0x6824]",
            },
            Fixture {
                raw: &[0xcd, 0x3c, 0xd9, 0x86, 0x24, 0x68],
                body: &[0x26, 0xd9, 0x86, 0x24, 0x68],
                code: Code::Fld_m32fp,
                ndisasm: "fld dword [es:bp+0x6824]",
            },
            Fixture {
                raw: &[0xcd, 0x37, 0xe3],
                body: &[0xdb, 0xe3],
                code: Code::Fninit,
                ndisasm: "fninit",
            },
            Fixture {
                raw: &[0xcd, 0x35, 0x7b, 0x29],
                body: &[0xd9, 0x7b, 0x29],
                code: Code::Fnstcw_m2byte,
                ndisasm: "fnstcw [bp+di+0x29]",
            },
        ]
    }

    #[test]
    fn original_fixtures_preserve_bytes_addresses_and_entire_boundaries() {
        for fixture in fixtures() {
            let mut input = fixture.raw.to_vec();
            input.extend_from_slice(&[0xc3, 0xcd, 0x35]); // RET then incomplete macro.
            let before = input.clone();
            let decoded = decode(&input, 0x1350).unwrap().unwrap();
            assert_eq!(input, before);
            assert_eq!(decoded.ip, 0x1350);
            assert_eq!(decoded.vector, fixture.raw[1]);
            assert_eq!(decoded.kind, MacroKind::X87);
            assert_eq!(decoded.len, fixture.raw.len());
            assert_eq!(decoded.original_bytes, fixture.raw);
            assert_eq!(decoded.transformed_bytes[0], 0x9b);
            assert_eq!(&decoded.transformed_bytes[1..], fixture.body);
            let instruction = decoded.instruction.unwrap();
            assert_eq!(instruction.code(), fixture.code);
            assert_eq!(instruction.ip(), 0x1351);
            assert_eq!(instruction.len(), fixture.body.len());
            assert_eq!(instruction.next_ip(), 0x1350 + decoded.len as u64);
            assert!(
                decode(&input[decoded.len..], instruction.next_ip16())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn segment_mapping_and_16_bit_effective_addresses() {
        for (token, expected) in [
            (0x19, Register::DS),
            (0x59, Register::SS),
            (0x99, Register::CS),
            (0xd9, Register::ES),
        ] {
            let decoded = decode(&[0xcd, 0x3c, token, 0x86, 0x24, 0x68], 0x1350)
                .unwrap()
                .unwrap();
            let instruction = decoded.instruction.unwrap();
            assert_eq!(instruction.op0_kind(), OpKind::Memory);
            assert_eq!(instruction.segment_prefix(), expected);
            assert_eq!(instruction.memory_segment(), expected);
            assert_eq!(instruction.memory_base(), Register::BP);
            assert_eq!(instruction.memory_index(), Register::None);
            assert_eq!(instruction.memory_displacement64(), 0x6824);
            assert_eq!(instruction.memory_displ_size(), 2);
        }
        let instruction = decode(&[0xcd, 0x34, 0x43, 0xa5], 0)
            .unwrap()
            .unwrap()
            .instruction
            .unwrap();
        assert_eq!(instruction.memory_base(), Register::BP);
        assert_eq!(instruction.memory_index(), Register::DI);
        assert_eq!(instruction.memory_segment(), Register::SS);
        assert_eq!(instruction.memory_displacement64(), 0xffa5);
        let direct = decode(&[0xcd, 0x36, 0x16, 0xb1, 0xa2], 0)
            .unwrap()
            .unwrap()
            .instruction
            .unwrap();
        assert_eq!(direct.memory_base(), Register::None);
        assert_eq!(direct.memory_index(), Register::None);
        assert_eq!(direct.memory_segment(), Register::DS);
        assert_eq!(direct.memory_displacement64(), 0xa2b1);
    }

    #[test]
    fn synchronization_is_explicit_not_a_two_byte_x87_instruction() {
        let decoded = decode(&[0xcd, 0x3d, 0xc3], 0xfffe).unwrap().unwrap();
        assert_eq!(decoded.ip, 0xfffe);
        assert_eq!(decoded.len, 2);
        assert_eq!(decoded.vector, 0x3d);
        assert_eq!(decoded.kind, MacroKind::Sync);
        assert_eq!(decoded.original_bytes, [0xcd, 0x3d]);
        assert_eq!(decoded.transformed_bytes, [0x9b]);
        assert!(decoded.instruction.is_none());
    }

    #[test]
    fn normal_instructions_and_other_interrupts_are_not_macros() {
        for bytes in [
            &[0x34, 0xcd][..],       // XOR AL,CDh, NOT INT 34h.
            &[0xb8, 0xcd, 0x35][..], // MOV AX,35CDh, not a scan target.
            &[0xcd, 0x33][..],
            &[0xcd, 0x3e][..],
            &[0xcd, 0x21][..],
            &[0xd9, 0xc5][..],
            &[0x9b, 0xd9, 0xc5][..],
            &[0x26, 0xcd, 0x21][..],
            &[0xc3][..],
        ] {
            assert!(decode(bytes, 0x1350).unwrap().is_none(), "{bytes:02x?}");
        }
        assert!(decode(&[0xc3, 0xcd, 0x34], 0xffff).unwrap().is_none());
    }

    #[test]
    fn prefixed_macros_fail_closed_including_int34_and_sync() {
        for prefix in [
            0x26, 0x2e, 0x36, 0x3e, 0x64, 0x65, 0x66, 0x67, 0xf0, 0xf2, 0xf3,
        ] {
            for vector in 0x34..=0x3d {
                let error = decode(&[prefix, 0xcd, vector, 0xe3], 0x1350).unwrap_err();
                assert!(error.contains("unsupported prefix"), "{error}");
            }
        }
        assert!(
            decode(&[0x26, 0x66, 0xcd, 0x34, 0xc1], 0)
                .unwrap_err()
                .contains("prefix")
        );
        assert!(decode(&[0x26; 15], 0).unwrap_err().contains("length limit"));
    }

    #[test]
    fn every_fixture_truncation_fails_instead_of_consuming_operand_as_code() {
        for fixture in fixtures() {
            for end in 0..fixture.raw.len() {
                let error = decode(&fixture.raw[..end], 0x1350).unwrap_err();
                assert!(error.contains("truncated"), "{error}");
                assert!(error.contains("1350"), "{error}");
            }
        }
        for bytes in [&[][..], &[0xcd][..], &[0x26][..], &[0x26, 0xcd][..]] {
            assert!(decode(bytes, 0).unwrap_err().contains("truncated"));
        }
    }

    #[test]
    fn every_segment_cut_fails_even_with_more_physical_bytes_supplied() {
        let mut inputs: Vec<Vec<u8>> = fixtures().into_iter().map(|f| f.raw.to_vec()).collect();
        inputs.push(vec![0xcd, 0x3d]);
        for bytes in inputs {
            for remaining in 1..bytes.len() {
                let ip = (0x10000 - remaining) as u16;
                assert!(decode(&bytes, ip).unwrap_err().contains("64-KiB"));
                assert!(
                    decode(&bytes[..remaining], ip)
                        .unwrap_err()
                        .contains("64-KiB")
                );
            }
            let ip = (0x10000 - bytes.len()) as u16;
            let decoded = decode(&bytes, ip).unwrap().unwrap();
            assert_eq!(usize::from(ip) + decoded.len, 0x10000);
            if let Some(instruction) = decoded.instruction {
                assert_eq!(instruction.next_ip16(), 0);
                assert_eq!(instruction.next_ip(), 0x10000);
            }
        }
        assert!(
            decode(&[0x26, 0xcd, 0x34, 0xc1], 0xfffe)
                .unwrap_err()
                .contains("64-KiB")
        );
    }

    #[test]
    fn invalid_embedded_opcodes_and_reserved_x87_encodings_fail() {
        for token in 0u8..=255 {
            if !(0xd8..=0xdf).contains(&(token | 0xc0)) {
                let error = decode(&[0xcd, 0x3c, token, 0x06, 0x24, 0x68], 0).unwrap_err();
                assert!(error.contains("segment/opcode"), "{error}");
            }
        }
        // D9 /1 is reserved in memory form. OR-ing an invalid token into a
        // non-x87 opcode must not turn arbitrary following bytes into a macro.
        assert!(
            decode(&[0xcd, 0x35, 0x08], 0)
                .unwrap_err()
                .contains("x87 encoding")
        );
        assert!(
            decode(&[0xcd, 0x3c, 0x99, 0x08], 0)
                .unwrap_err()
                .contains("x87 encoding")
        );
    }

    #[test]
    fn wait_is_not_mistaken_for_the_body_or_merged_into_a_different_code() {
        let decoded = decode(&[0xcd, 0x37, 0xe3], 0x1350).unwrap().unwrap();
        assert_eq!(decoded.transformed_bytes, [0x9b, 0xdb, 0xe3]);
        let body = decoded.instruction.unwrap();
        assert_eq!(body.mnemonic(), Mnemonic::Fninit);
        assert_eq!(body.len(), 2);
        assert_eq!(decoded.len, 3);
        let prefixed_register = decode(&[0xcd, 0x3c, 0xd9, 0xc5], 0).unwrap().unwrap();
        assert_eq!(prefixed_register.instruction.unwrap().code(), Code::Fld_sti);
        assert_eq!(prefixed_register.len, 4);
    }

    #[test]
    fn all_opcode_modrm_and_segment_forms_agree_with_direct_iced_decoding() {
        let mut valid = 0;
        let mut invalid = 0;
        for opcode in 0xd8u8..=0xdf {
            for modrm in 0u8..=255 {
                // Padding is deliberately recognizable and may NOT be consumed
                // unless the direct decoder identifies a displacement there.
                for segment in [None, Some(0x3e), Some(0x36), Some(0x2e), Some(0x26)] {
                    let mut direct = Vec::new();
                    if let Some(prefix) = segment {
                        direct.push(prefix);
                    }
                    direct.extend_from_slice(&[opcode, modrm, 0x24, 0x68, 0xc3]);
                    let mut reference = Decoder::with_ip(16, &direct, 0x1351, DecoderOptions::NONE);
                    let expected = reference.decode();
                    let mut raw = if let Some(prefix) = segment {
                        let high = match prefix {
                            0x3e => 0,
                            0x36 => 0x40,
                            0x2e => 0x80,
                            0x26 => 0xc0,
                            _ => unreachable!(),
                        };
                        vec![0xcd, 0x3c, (opcode & 0x3f) | high]
                    } else {
                        vec![0xcd, opcode - 0xa4]
                    };
                    raw.extend_from_slice(&[modrm, 0x24, 0x68, 0xc3]);
                    let result = decode(&raw, 0x1350);
                    if expected.is_invalid() {
                        assert!(result.is_err(), "{raw:02x?}");
                        invalid += 1;
                    } else {
                        let decoded = result.unwrap().unwrap();
                        assert_eq!(decoded.len, expected.len() + 1, "{raw:02x?}");
                        assert_eq!(decoded.instruction.unwrap(), expected, "{raw:02x?}");
                        assert_eq!(decoded.original_bytes, raw[..decoded.len]);
                        assert_eq!(&decoded.transformed_bytes[1..], &direct[..expected.len()]);
                        valid += 1;
                    }
                }
            }
        }
        assert!(valid > 0 && invalid > 0);
        assert_eq!(valid + invalid, 8 * 256 * 5);
    }

    #[test]
    #[ignore = "independent disassembler: requires ndisasm on PATH; run explicitly with --ignored"]
    fn ndisasm_cross_checks_original_fixtures_and_following_instruction() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        fn disassemble(bytes: &[u8], ip: u64) -> Vec<(u64, Vec<u8>, String)> {
            let mut child = Command::new("ndisasm")
                .args(["-b", "16", "-o", &ip.to_string(), "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("ndisasm is required for this explicitly requested test");
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| {
                    let mut fields = line.split_whitespace();
                    let ip = u64::from_str_radix(fields.next().unwrap(), 16).unwrap();
                    let hex = fields.next().unwrap();
                    let bytes = (0..hex.len())
                        .step_by(2)
                        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
                        .collect();
                    (ip, bytes, fields.collect::<Vec<_>>().join(" "))
                })
                .collect()
        }

        for fixture in fixtures() {
            let decoded = decode(fixture.raw, 0x1350).unwrap().unwrap();
            let mut body = decoded.transformed_bytes[1..].to_vec();
            body.push(0xc3);
            let lines = disassemble(&body, 0x1351);
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert_eq!(
                lines[0],
                (0x1351, fixture.body.to_vec(), fixture.ndisasm.into())
            );
            assert_eq!(
                lines[1],
                (0x1350 + decoded.len as u64, vec![0xc3], "ret".into())
            );
            let mut canonical = decoded.transformed_bytes;
            canonical.push(0xc3);
            let lines = disassemble(&canonical, 0x1350);
            assert_eq!(
                lines.last().unwrap(),
                &(0x1350 + decoded.len as u64, vec![0xc3], "ret".into())
            );
            let all_bytes: Vec<u8> = lines.into_iter().flat_map(|(_, bytes, _)| bytes).collect();
            assert_eq!(all_bytes, canonical);
        }
        let sync = decode(&[0xcd, 0x3d], 0x1350).unwrap().unwrap();
        assert_eq!(
            disassemble(&sync.transformed_bytes, 0x1350),
            vec![(0x1350, vec![0x9b], "wait".into())]
        );
    }
}
