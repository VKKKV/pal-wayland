// SPDX-License-Identifier: GPL-3.0-or-later
//! AOT emission for explicit 32-bit effective addresses in 16-bit code.
//!
//! The caller guards the original bytes and charges `m.steps`; `emit` appends
//! the successful next_ip16 update. Emission needs Machine, SegmentedAddress
//! and Flags in scope, but no extra helpers, decoder or runtime dispatch.
//!
//! Base + index * scale + displacement wraps at **32** bits. Address-size does
//! not enlarge a real-mode segment: the complete byte/word/dword access must fit
//! 0..=0xffff. Narrowing to SegmentedAddress happens only AFTER that check.
//! Iced supplies the default SS for EBP/ESP bases (not an EBP index), or a
//! segment override. FS/GS and unlisted forms fail explicitly. LEA is not a
//! memory access: it computes the full EA without a segment-limit check, then
//! truncates only if its destination is a 16-bit register, as x86 requires.
//!
//! Fault/atomicity policy: LOCK and implicit-atomic XCHG are unsupported. Every
//! supported instruction has at most one memory operand. Its EA and operands
//! are captured before writes, flags are computed locally, and a fallible
//! Machine write happens before flags/IP commit. Machine/GuestMemory provides
//! all-or-nothing writes on returned errors (including physical bounds/A20).
//! Thus a fault leaves registers, upper halves, flags, IP and guest bytes intact;
//! the caller's already charged step is not rolled back. This is not a promise
//! of hardware/concurrent atomicity. Undefined flags are preserved, not claimed
//! to match any CPU; unaffected flags, including reserved bits, are preserved.
//!
//! Register-only address overrides cannot be identified from Instruction alone
//! (iced discards them); this module intentionally rejects non-memory forms.
//! Address-size32 strings, stacks and control transfers remain explicit errors.

#![forbid(unsafe_code)]

use iced_x86::{CodeSize, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::fmt::Write;

fn gpr32(r: Register) -> Result<u8, String> {
    use Register::*;
    Ok(match r {
        EAX => 0,
        ECX => 1,
        EDX => 2,
        EBX => 3,
        ESP => 4,
        EBP => 5,
        ESI => 6,
        EDI => 7,
        _ => return Err(format!("unsupported address32 GPR {r:?}")),
    })
}

// (width in bytes, underlying word field, byte shift)
fn narrow(r: Register) -> Result<(usize, &'static str, u32), String> {
    use Register::*;
    Ok(match r {
        AL => (1, "ax", 0),
        CL => (1, "cx", 0),
        DL => (1, "dx", 0),
        BL => (1, "bx", 0),
        AH => (1, "ax", 8),
        CH => (1, "cx", 8),
        DH => (1, "dx", 8),
        BH => (1, "bx", 8),
        AX => (2, "ax", 0),
        CX => (2, "cx", 0),
        DX => (2, "dx", 0),
        BX => (2, "bx", 0),
        SP => (2, "sp", 0),
        BP => (2, "bp", 0),
        SI => (2, "si", 0),
        DI => (2, "di", 0),
        _ => return Err(format!("unsupported address32 data register {r:?}")),
    })
}

fn register_width(r: Register) -> Result<usize, String> {
    if gpr32(r).is_ok() {
        Ok(4)
    } else {
        Ok(narrow(r)?.0)
    }
}

fn segment(r: Register) -> Result<&'static str, String> {
    Ok(match r {
        Register::CS => "m.regs.cs",
        Register::DS => "m.regs.ds",
        Register::ES => "m.regs.es",
        Register::SS => "m.regs.ss",
        _ => return Err(format!("unsupported address32 segment {r:?}")),
    })
}

fn offset(i: &Instruction) -> Result<String, String> {
    let base = i.memory_base();
    let index = i.memory_index();
    if base == Register::None && index == Register::None && i.memory_displ_size() != 4 {
        return Err("address32 emitter requires a 32-bit effective address".into());
    }
    if !matches!(i.memory_index_scale(), 1 | 2 | 4 | 8) || i.memory_displ_size() > 4 {
        return Err("unsupported address32 scale/displacement".into());
    }
    // A decoded signed displacement is represented by its modulo-2^32 value.
    let mut value = format!("{}u32", i.memory_displacement64() as u32);
    if base != Register::None {
        value = format!("({value}).wrapping_add(m.get32({}))", gpr32(base)?);
    }
    if index != Register::None {
        value = format!(
            "({value}).wrapping_add(m.get32({}).wrapping_mul({}u32))",
            gpr32(index)?,
            i.memory_index_scale()
        );
    }
    Ok(value)
}

fn width(i: &Instruction, op: u32) -> Result<usize, String> {
    let value = match i.op_kind(op) {
        OpKind::Register => register_width(i.op_register(op))?,
        OpKind::Memory => i.memory_size().size(),
        _ => return Err("expected address32 data register or memory".into()),
    };
    if !matches!(value, 1 | 2 | 4) {
        return Err(format!("unsupported address32 operand width {value}"));
    }
    Ok(value)
}

fn bind(i: &Instruction, op: u32, name: &str, out: &mut String) -> Result<(), String> {
    if i.op_kind(op) == OpKind::Memory {
        let size = width(i, op)?;
        writeln!(out, "let {name}_ea: u32 = {};", offset(i)?).unwrap();
        writeln!(out, "if {name}_ea > {}u32 {{ return Err(m.fault(format!(\"address32 {}-byte access EA={{:#010x}} exceeds 64-KiB segment boundary\", {name}_ea))); }}", 0x10000-size, size).unwrap();
        writeln!(
            out,
            "let {name} = SegmentedAddress::new({}, {name}_ea as u16);",
            segment(i.memory_segment())?
        )
        .unwrap();
    }
    Ok(())
}

// All data reads use u32, with a validated operation width.
fn read(i: &Instruction, op: u32, name: &str, size: usize) -> Result<String, String> {
    Ok(match i.op_kind(op) {
        OpKind::Register => {
            let r = i.op_register(op);
            if register_width(r)? != size {
                return Err("address32 register width mismatch".into());
            }
            if size == 4 {
                format!("m.get32({})", gpr32(r)?)
            } else {
                let (_, field, shift) = narrow(r)?;
                if size == 1 {
                    format!("u32::from((m.regs.{field} >> {shift}) as u8)")
                } else {
                    format!("u32::from(m.regs.{field})")
                }
            }
        }
        OpKind::Memory if width(i, op)? == size => {
            if size == 4 {
                format!("m.read32({name})?")
            } else {
                format!("u32::from(m.read{}({name})?)", size * 8)
            }
        }
        OpKind::Immediate8 if size == 1 => format!("{}u32", i.immediate8()),
        OpKind::Immediate16 if size == 2 => format!("{}u32", i.immediate16()),
        OpKind::Immediate32 if size == 4 => format!("{}u32", i.immediate32()),
        OpKind::Immediate8to16 if size == 2 => format!("{}u32", i.immediate8to16() as u16),
        OpKind::Immediate8to32 if size == 4 => format!("{}u32", i.immediate8to32() as u32),
        _ => {
            return Err(format!(
                "unsupported address32 operand {:?} width {size}",
                i.op_kind(op)
            ));
        }
    })
}

fn store(i: &Instruction, name: &str, size: usize, value: &str) -> Result<String, String> {
    if width(i, 0)? != size {
        return Err("address32 destination width mismatch".into());
    }
    Ok(match i.op0_kind() {
        OpKind::Memory => format!("m.write{}({name}, {value} as u{})?;\n", size * 8, size * 8),
        OpKind::Register if size == 4 => {
            format!("m.set32({}, {value});\n", gpr32(i.op0_register())?)
        }
        OpKind::Register => {
            let (_, field, shift) = narrow(i.op0_register())?;
            if size == 2 {
                format!("m.regs.{field} = {value} as u16;\n")
            } else {
                format!(
                    "m.regs.{field} = (m.regs.{field} & !{}u16) | (u16::from({value} as u8) << {shift});\n",
                    0xffu16 << shift
                )
            }
        }
        _ => return Err("unsupported address32 destination".into()),
    })
}

/// Emit statements for one decoded address-size32 instruction in Code16.
/// The generated enclosing function must return Result<(), runtime::Fault>.
/// Successful emission is supported semantics, never an interpreter fallback.
pub fn emit(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    if i.is_invalid() || i.code_size() != CodeSize::Code16 || i.flow_control() != FlowControl::Next
    {
        return Err("address32 emitter requires valid non-control-flow Code16".into());
    }
    if i.has_lock_prefix() || i.has_rep_prefix() || i.has_repne_prefix() {
        return Err("address32 LOCK/REP semantics are unsupported".into());
    }
    if matches!(i.segment_prefix(), Register::FS | Register::GS) {
        return Err("address32 FS/GS segments are unsupported".into());
    }
    if i.op_kinds().filter(|&k| k == OpKind::Memory).count() != 1 {
        return Err("address32 emitter requires exactly one explicit memory operand".into());
    }
    offset(i)?; // Reject 16/64-bit EAs even for LEA.
    segment(i.memory_segment())?;
    let mnemonic = i.mnemonic();
    let expected = if matches!(mnemonic, Inc | Dec | Neg | Not) {
        1
    } else {
        2
    };
    if i.op_count() != expected {
        return Err("unsupported address32 operand count".into());
    }
    let mut out = String::new();
    let size = width(i, 0)?;
    match mnemonic {
        Lea => {
            if i.op0_kind() != OpKind::Register || i.op1_kind() != OpKind::Memory || size == 1 {
                return Err("unsupported address32 LEA form".into());
            }
            writeln!(out, "let value: u32 = {};", offset(i)?).unwrap();
            out.push_str(&store(i, "unused", size, "value")?);
        }
        Mov | Movzx | Movsx => {
            let source_size = if mnemonic == Mov { size } else { width(i, 1)? };
            if mnemonic != Mov
                && (i.op0_kind() != OpKind::Register || source_size >= size || source_size > 2)
            {
                return Err("unsupported address32 extension form".into());
            }
            bind(i, 0, "dst", &mut out)?;
            bind(i, 1, "src", &mut out)?;
            writeln!(out, "let value: u32 = {};", read(i, 1, "src", source_size)?).unwrap();
            if mnemonic == Movsx {
                writeln!(
                    out,
                    "let value = value as i{} as i32 as u32;",
                    source_size * 8
                )
                .unwrap();
            }
            out.push_str(&store(i, "dst", size, "value")?);
        }
        Add | Adc | Sub | Sbb | Cmp | And | Or | Xor | Test | Inc | Dec | Neg | Not => {
            bind(i, 0, "dst", &mut out)?;
            if expected == 2 {
                bind(i, 1, "src", &mut out)?;
            }
            writeln!(out, "let a: u32 = {};", read(i, 0, "dst", size)?).unwrap();
            let b = if expected == 2 {
                read(i, 1, "src", size)?
            } else {
                "1u32".into()
            };
            writeln!(out, "let b: u32 = {b};\nlet mut flags = m.regs.flags;").unwrap();
            let mask = if size == 4 {
                u32::MAX
            } else {
                (1u32 << (size * 8)) - 1
            };
            let sign = 1u32 << (size * 8 - 1);
            match mnemonic {
                Not => writeln!(out, "let value = !a & {mask}u32;").unwrap(),
                And | Or | Xor | Test => {
                    let operator = match mnemonic {
                        And | Test => "&",
                        Or => "|",
                        _ => "^",
                    };
                    writeln!(
                        out,
                        "let value = a {operator} b;\nflags.set(Flags::CF | Flags::OF, false);"
                    )
                    .unwrap();
                }
                _ => {
                    if mnemonic == Neg {
                        out.push_str("let b = a; let a = 0u32;\n");
                    }
                    let subtract = matches!(mnemonic, Sub | Sbb | Cmp | Dec | Neg);
                    let carry = if matches!(mnemonic, Adc | Sbb) {
                        "flags.cf()"
                    } else {
                        "false"
                    };
                    writeln!(out, "let c = u64::from({carry});").unwrap();
                    if subtract {
                        writeln!(out,"let value = a.wrapping_sub(b).wrapping_sub(c as u32) & {mask}u32;\nlet cf = u64::from(a) < u64::from(b) + c;").unwrap();
                    } else {
                        writeln!(out,"let wide = u64::from(a) + u64::from(b) + c;\nlet value = wide as u32 & {mask}u32;\nlet cf = wide > {mask}u64;").unwrap();
                    }
                    // Wide signed sum/difference includes the original carry,
                    // avoiding an overflow error when b+carry wraps its width.
                    writeln!(out,"let signed_a = if a & {sign}u32 != 0 {{ i64::from(a) - {}i64 }} else {{ i64::from(a) }};\nlet signed_b = if b & {sign}u32 != 0 {{ i64::from(b) - {}i64 }} else {{ i64::from(b) }};",u64::from(mask)+1,u64::from(mask)+1).unwrap();
                    let operator = if subtract { "-" } else { "+" };
                    writeln!(out,"let signed = signed_a {operator} signed_b {operator} c as i64;\nflags.set(Flags::OF, signed < -{sign}i64 || signed >= {sign}i64);\nflags.set(Flags::AF, (a ^ b ^ value) & 0x10 != 0);").unwrap();
                    if !matches!(mnemonic, Inc | Dec) {
                        out.push_str("flags.set(Flags::CF, cf);\n");
                    }
                }
            }
            if mnemonic != Not {
                writeln!(out,"flags.set(Flags::ZF, value == 0);\nflags.set(Flags::SF, value & {sign}u32 != 0);\nflags.set(Flags::PF, (value as u8).count_ones().is_multiple_of(2));").unwrap();
            }
            if !matches!(mnemonic, Cmp | Test) {
                out.push_str(&store(i, "dst", size, "value")?);
            }
            out.push_str("m.regs.flags = flags;\n");
        }
        _ => return Err(format!("unsupported address32 instruction {:?}", i.code())),
    }
    writeln!(out, "m.regs.ip = {};", i.next_ip16()).unwrap();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::{Decoder, DecoderOptions};

    fn decode(bytes: &[u8]) -> Instruction {
        let mut d = Decoder::with_ip(16, bytes, 0x656, DecoderOptions::NONE);
        let i = d.decode();
        assert!(!i.is_invalid(), "{bytes:02x?}");
        assert_eq!(i.len(), bytes.len(), "{bytes:02x?}");
        i
    }

    #[test]
    fn unsupported_forms_are_errors_not_fake_success() {
        for bytes in [
            &[0x8b, 0x00][..],         // address16
            &[0x67, 0x8b, 0xc3],       // no observable EA
            &[0x67, 0xf3, 0xa5],       // address32 string
            &[0x67, 0xff, 0x30],       // PUSH [EAX]
            &[0x67, 0x8f, 0x00],       // POP [EAX]
            &[0x67, 0xff, 0x20],       // JMP [EAX]
            &[0x67, 0xf7, 0x20],       // MUL [EAX]
            &[0x67, 0x87, 0x18],       // implicitly atomic XCHG
            &[0xf0, 0x67, 0x01, 0x18], // LOCK ADD
            &[0xf3, 0x67, 0x8b, 0x00], // REP MOV
            &[0x64, 0x67, 0x8b, 0x00], // FS
            &[0x65, 0x67, 0x8b, 0x00], // GS
            &[0x67, 0x8c, 0x18],       // segment transfer
        ] {
            assert!(emit(&decode(bytes)).is_err(), "{bytes:02x?}");
        }
        let mut i = decode(&[0x67, 0x8b, 0x00]);
        i.set_code_size(CodeSize::Code32);
        assert!(emit(&i).is_err());
    }

    #[test]
    fn emitted_original_fixtures_compile_and_execute_real_runtime() {
        let mut fixtures: Vec<(String, Vec<u8>)> = vec![];
        let mut add = |name: &str, bytes: &[u8]| fixtures.push((name.into(), bytes.into()));
        for (name, bytes) in [
            ("TREE", &[0x2e, 0x67, 0x8b, 0x1c, 0x5d, 0x0e, 0, 0, 0][..]),
            ("TREE_STORE", &[0x2e, 0x67, 0x89, 0x1c, 0x5d, 0x0e, 0, 0, 0]),
            ("SS_EBP", &[0x67, 0x8b, 0x45, 0xfe]),
            ("SS_ESP", &[0x67, 0x8b, 0x04, 0x24]),
            ("ES_EBP", &[0x26, 0x67, 0x8b, 0x45, 0xfe]),
            ("INDEX_EBP", &[0x67, 0x8b, 0x04, 0x6d, 0, 0, 0, 0]), // EBP index => DS
            ("LOAD", &[0x67, 0x8b, 0x00]),
            ("STORE", &[0x67, 0x89, 0x18]),
            ("LOAD32", &[0x66, 0x67, 0x8b, 0x00]),
            ("STORE32", &[0x66, 0x67, 0x89, 0x18]),
            ("LOAD8", &[0x67, 0x8a, 0x20]),  // AH,[EAX]
            ("STORE8", &[0x67, 0x88, 0x20]), // [EAX],AH
            ("MOFFS", &[0x67, 0xa1, 1, 2, 0, 0]),
            ("HIGH_DISP", &[0x67, 0xa1, 1, 2, 1, 0]),
            ("IMM8", &[0x67, 0xc6, 0x00, 0x81]),
            ("IMM16", &[0x67, 0xc7, 0x00, 0x81, 0x80]),
            ("IMM32", &[0x66, 0x67, 0xc7, 0x00, 0x81, 0x80, 0xff, 0xff]),
            ("ZX8", &[0x67, 0x0f, 0xb6, 0x00]),
            ("SX8", &[0x67, 0x0f, 0xbe, 0x00]),
            ("ZX8_32", &[0x66, 0x67, 0x0f, 0xb6, 0x00]),
            ("SX8_32", &[0x66, 0x67, 0x0f, 0xbe, 0x00]),
            ("ZX16_32", &[0x66, 0x67, 0x0f, 0xb7, 0x00]),
            ("SX16_32", &[0x66, 0x67, 0x0f, 0xbf, 0x00]),
            ("LEA16", &[0x67, 0x8d, 0x04, 0xf0]), // EAX+ESI*8
            ("LEA32", &[0x66, 0x67, 0x8d, 0x04, 0xf0]),
            ("SIB_LOAD", &[0x67, 0x8b, 0x04, 0xf0]),
            ("ADD_REG", &[0x67, 0x03, 0x00]),         // AX,[EAX]
            ("ADD_SELF_SOURCE", &[0x67, 0x01, 0x00]), // [EAX],AX
            ("ADD_SIGN_IMM", &[0x67, 0x83, 0x00, 0xff]),
        ] {
            add(name, bytes);
        }
        for (suffix, prefix, opcode) in [
            ("8", vec![0x67], 0x00),
            ("16", vec![0x67], 0x01),
            ("32", vec![0x66, 0x67], 0x01),
        ] {
            for (name, delta) in [
                ("ADD", 0),
                ("OR", 8),
                ("ADC", 16),
                ("SBB", 24),
                ("AND", 32),
                ("SUB", 40),
                ("XOR", 48),
                ("CMP", 56),
            ] {
                let mut bytes = prefix.clone();
                bytes.extend([opcode + delta, 0x18]);
                add(&format!("{name}_{suffix}"), &bytes);
            }
            let mut bytes = prefix.clone();
            bytes.extend([if suffix == "8" { 0x84 } else { 0x85 }, 0x18]);
            add(&format!("TEST_{suffix}"), &bytes);
            for (name, field) in [("INC", 0), ("DEC", 1), ("NOT", 2), ("NEG", 3)] {
                let mut bytes = prefix.clone();
                bytes.extend([
                    if field <= 1 {
                        if suffix == "8" { 0xfe } else { 0xff }
                    } else {
                        if suffix == "8" { 0xf6 } else { 0xf7 }
                    },
                    field << 3,
                ]);
                add(&format!("{name}_{suffix}"), &bytes);
            }
        }
        for r in 0..8u8 {
            let bytes = if r == 4 {
                vec![0x67, 0x8b, 0x1c, 0x24]
            } else if r == 5 {
                vec![0x67, 0x8b, 0x5d, 0]
            } else {
                vec![0x67, 0x8b, 0x18 | r]
            };
            add(&format!("BASE_{r}"), &bytes);
        }
        for (field, size) in [(0, 1), (0x40, 2), (0x80, 4), (0xc0, 8)] {
            add(&format!("SCALE_{size}"), &[0x67, 0x8b, 0x04, field | 0x33]); // EBX+ESI*scale
        }
        let root = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = root.join(format!("pal-aot-address32-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut program = String::from(
            "#![forbid(unsafe_code)]\n#![allow(dead_code,unused_variables,unused_mut)]\n",
        );
        for name in [
            "semantics",
            "mz",
            "video",
            "text_video",
            "planar_video",
            "dos_files",
            "cdrom",
            "audio",
            "sound_blaster",
            "xms",
            "runtime",
        ] {
            writeln!(
                program,
                "#[path={:?}] pub mod {name};",
                crate::lift::runtime_fixture_source(name)
            )
            .unwrap();
        }
        program.push_str("use runtime::{Machine,Fault}; use semantics::*;\n");
        for (index, (name, _)) in fixtures.iter().enumerate() {
            writeln!(program, "const {name}: usize = {index};").unwrap();
        }
        program.push_str("const FIXTURES: &[&[u8]] = &[\n");
        for (_, bytes) in &fixtures {
            writeln!(program, "&{bytes:?},").unwrap();
        }
        program.push_str("];\nfn step(m: &mut Machine,index:usize)->Result<(),Fault> {\nm.guard_code(FIXTURES[index])?; m.steps += 1;\nmatch index {\n");
        for (index, (name, bytes)) in fixtures.iter().enumerate() {
            let body = emit(&decode(bytes)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                !body.contains("iced") && !body.contains("Decoder") && !body.contains("unsafe")
            );
            writeln!(program, "{index} => {{ {body} }},").unwrap();
        }
        program.push_str("_=>unreachable!(),\n}\nOk(())\n}\n");
        program.push_str(EXECUTION_TESTS);
        let path = dir.join("main.rs");
        std::fs::write(&path, program).unwrap();
        let compile = crate::lift::runtime_fixture_rustc()
            .args(["--edition=2024", "-Dwarnings", "-C", "overflow-checks=yes"])
            .arg(&path)
            .arg("-o")
            .arg(dir.join("run"))
            .output()
            .unwrap();
        assert!(
            compile.status.success(),
            "{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let result = std::process::Command::new(dir.join("run"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("address32 AOT fixtures passed"), "{output}");
        print!("{output}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    const EXECUTION_TESTS: &str = r#"
fn machine() -> Machine {
    let mut bytes = vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {
        bytes[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());
    }
    let mut m = Machine::from_mz(&bytes,0x1010).unwrap();
    m.regs = Registers {cs:0x3c99,ds:0x6000,ss:0x8000,es:0x7000,..Registers::default()};
    m
}
fn ea(m:&Machine,offset:u16)->SegmentedAddress {SegmentedAddress::new(m.regs.ds,offset)}
fn prepare(m:&mut Machine,index:usize) {
    m.regs.ip=0x656;
    m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,0x656),FIXTURES[index]).unwrap();
}
fn run(m:&mut Machine,index:usize) {
    prepare(m,index);let steps=m.steps;
    step(m,index).unwrap();assert_eq!(m.steps,steps+1);
    assert_eq!(m.regs.ip,0x656+FIXTURES[index].len() as u16);
}
fn snapshot(m:&Machine)->(Registers,Vec<u32>,Vec<u8>) {
    (m.regs,(0..8).map(|r|m.get32(r)).collect(),m.memory.as_bytes().to_vec())
}
fn fault(m:&mut Machine,index:usize,needle:&str) {
    prepare(m,index);let before=snapshot(m);let steps=m.steps;
    let err=step(m,index).unwrap_err();assert!(err.0.contains(needle),"{err}");
    assert_eq!(snapshot(m),before,"fault must be atomic, fixture {index}");assert_eq!(m.steps,steps+1);
}
fn get(m:&Machine,size:usize)->u32 {
    let p=ea(m,0x201);
    match size {1=>u32::from(m.read8(p).unwrap()),2=>u32::from(m.read16(p).unwrap()),_=>m.read32(p).unwrap()}
}
fn put(m:&mut Machine,size:usize,value:u32) {
    let p=ea(m,0x201);
    match size {1=>m.write8(p,value as u8).unwrap(),2=>m.write16(p,value as u16).unwrap(),_=>m.write32(p,value).unwrap()}
}
// Independent mathematical oracle, using signed/nibble ranges instead of the
// emitted XOR flag formula. Compare only defined flag bits, then separately
// check that the chosen undefined-flag preservation policy is respected.
fn oracle(op:usize,bits:u32,a:u32,b:u32,initial:u16)->(u32,u16,u16) {
    let modulus=1i128<<bits;let midpoint=modulus/2;
    let a=i128::from(a);let b=i128::from(b);let carry=i128::from(initial&1);
    let signed=|v:i128|if v>=midpoint {v-modulus} else {v};
    let logic=matches!(op,4|5|6|8);let mut flags=initial;
    let (wide,of,af)=match op {
        0|1 => {let c=if op==1 {carry} else {0};(a+b+c,signed(a)+signed(b)+c,a%16+b%16+c)},
        2|3|7 => {let c=if op==3 {carry} else {0};(a-b-c,signed(a)-signed(b)-c,a%16-b%16-c)},
        9 => (a+1,signed(a)+1,a%16+1),
        10 => (a-1,signed(a)-1,a%16-1),
        11 => (-a,-signed(a),-(a%16)),
        12 => (modulus-1-a,0,0),
        4|8 => (a&b,0,0),5 => (a|b,0,0),6 => (a^b,0,0),_=>unreachable!(),
    };
    let result=wide.rem_euclid(modulus) as u32;
    if op==12 {return (result,initial,0);}
    let mask=if logic {Flags::ARITHMETIC & !Flags::AF} else if matches!(op,9|10) {Flags::ARITHMETIC & !Flags::CF} else {Flags::ARITHMETIC};
    flags&=!mask;
    let mut ones=0;for n in 0..8 {ones+=(result>>n)&1;}
    for (bit,set) in [(Flags::CF,!logic && (wide<0 || wide>=modulus)),
        (Flags::OF,!logic && (of< -midpoint || of>=midpoint)),(Flags::AF,!(0..16).contains(&af)),
        (Flags::ZF,result==0),(Flags::SF,i128::from(result)>=midpoint),(Flags::PF,ones%2==0)] {
        if mask&bit!=0 && set {flags|=bit;}
    }
    (result,flags,mask)
}
fn check_arithmetic(m:&mut Machine) {
    for (size,ops) in [(1,[ADD_8,ADC_8,SUB_8,SBB_8,AND_8,OR_8,XOR_8,CMP_8,TEST_8,INC_8,DEC_8,NEG_8,NOT_8]),
        (2,[ADD_16,ADC_16,SUB_16,SBB_16,AND_16,OR_16,XOR_16,CMP_16,TEST_16,INC_16,DEC_16,NEG_16,NOT_16]),
        (4,[ADD_32,ADC_32,SUB_32,SBB_32,AND_32,OR_32,XOR_32,CMP_32,TEST_32,INC_32,DEC_32,NEG_32,NOT_32])] {
        let bits=size as u32*8;let mask=((1u64<<bits)-1) as u32;
        let edges=[0,1,15,16,127,128,255,256,0x7fff,0x8000,0xffff,0x7fff_ffff,0x8000_0000,u32::MAX];
        let mut random=0x3482_9137u32;
        let mut pairs:Vec<(u32,u32)>=edges.into_iter().flat_map(|a|edges.into_iter().map(move |b|(a&mask,b&mask))).collect();
        for _ in 0..512 {
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);let a=random&mask;
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);pairs.push((a,random&mask));
        }
        for (a,b) in pairs {for initial in [0,0xffff,0xa555,0x5aaa] {
            for (op,index) in ops.into_iter().enumerate() {
                m.set32(0,0x201);m.set32(3,b);put(m,size,a);m.regs.flags=Flags::from_bits(initial);
                let p=ea(m,0x200);m.write8(p,0x5a).unwrap();let q=ea(m,0x201+size as u16);m.write8(q,0xa5).unwrap();
                let before:Vec<_>=(0..8).map(|r|m.get32(r)).collect();
                let (value,flags,defined)=oracle(op,bits,a,b,initial);run(m,index);
                assert_eq!(get(m,size),if matches!(op,7|8) {a} else {value},"size={size} op={op} a={a:x} b={b:x}");
                assert_eq!(m.regs.flags.bits()&defined,flags&defined,"defined flags size={size} op={op} a={a:x} b={b:x}");
                assert_eq!(m.regs.flags.bits()&!defined,initial&!defined,"unaffected/undefined preservation");
                assert_eq!((0..8).map(|r|m.get32(r)).collect::<Vec<_>>(),before);
                assert_eq!((m.read8(p).unwrap(),m.read8(q).unwrap()),(0x5a,0xa5));
            }
        }}
    }
}
fn main() {
    let mut m=machine();
    // Actual blocking encoding, with CS override and BX/EBX destination alias.
    m.set32(3,0x123);m.regs.flags=Flags::from_bits(0xa55a);
    let p=SegmentedAddress::new(m.regs.cs,0x254);m.write16(p,0xabcd).unwrap();
    run(&mut m,TREE);assert_eq!(m.get32(3),0xabcd);assert_eq!(m.regs.flags.bits(),0xa55a);
    m.set32(3,0x8000_0123);run(&mut m,TREE);assert_eq!(m.get32(3),0x8000_abcd); // scale overflow wraps at 32 bits
    m.set32(3,0x123);run(&mut m,TREE_STORE);assert_eq!(m.read16(p).unwrap(),0x123);
    // High half must be used, never silently truncated to BX/EAX low halves.
    m.set32(3,0x1_0123);fault(&mut m,TREE,"segment boundary");
    m.set32(0,0x1_0201);fault(&mut m,LOAD,"segment boundary");
    fault(&mut m,HIGH_DISP,"segment boundary");
    for (index,size) in [(LOAD8,1u32),(LOAD,2),(STORE,2),(LOAD32,4),(STORE32,4),(ADD_32,4)] {
        m.set32(0,0x10000-size);m.set32(3,0x89ab_cdef);
        let p=SegmentedAddress::new(m.regs.ds,(0x10000-size) as u16);m.write8(p,0x81).unwrap();
        run(&mut m,index);
        m.set32(0,0x10001-size);fault(&mut m,index,"segment boundary");
    }
    // SS depends on the base, not the index; an explicit override wins.
    m.set32(5,0x203);m.set32(4,0x201);
    m.write16(SegmentedAddress::new(m.regs.ss,0x201),0x1234).unwrap();
    m.write16(SegmentedAddress::new(m.regs.es,0x201),0xabcd).unwrap();
    run(&mut m,SS_EBP);assert_eq!(m.regs.ax,0x1234);
    run(&mut m,SS_ESP);assert_eq!(m.regs.ax,0x1234);
    run(&mut m,ES_EBP);assert_eq!(m.regs.ax,0xabcd);
    m.set32(5,0x100);let p=ea(&m,0x200);m.write16(p,0x4567).unwrap();
    run(&mut m,INDEX_EBP);assert_eq!(m.regs.ax,0x4567);
    for (r,index) in [BASE_0,BASE_1,BASE_2,BASE_3,BASE_4,BASE_5,BASE_6,BASE_7].into_iter().enumerate() {
        m.set32(r as u8,0x201);let segment=if matches!(r,4|5) {m.regs.ss} else {m.regs.ds};
        let p=SegmentedAddress::new(segment,0x201);m.write16(p,0x9293).unwrap();
        run(&mut m,index);assert_eq!(m.regs.bx,0x9293);
        m.set32(r as u8,0x1_0201);fault(&mut m,index,"segment boundary");
    }
    // Signed disp8, each scale, and independent u64 modulo-ring EA oracle.
    let mut random=0x5a23_8197u32;
    for (scale,index) in [(1,SCALE_1),(2,SCALE_2),(4,SCALE_4),(8,SCALE_8)] {
        for _ in 0..512 {
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);let source=random;
            let target=(random>>16)%0xfffe;
            let base=(u64::from(target)+(1u64<<32)-(u64::from(source)*scale)%(1u64<<32))%(1u64<<32);
            m.set32(3,base as u32);m.set32(6,source);
            let p=ea(&m,target as u16);m.write16(p,0x9182).unwrap();
            run(&mut m,index);assert_eq!(m.regs.ax,0x9182);
        }
    }
    m.set32(5,1);fault(&mut m,SS_EBP,"segment boundary");
    // LEA ignores segment limits; 16-bit destination preserves upper EAX.
    m.set32(0,0xffff_fffe);m.set32(6,0x2000_0040);run(&mut m,LEA32);assert_eq!(m.get32(0),0x1fe);
    m.set32(0,0x1234_0201);m.set32(6,0);run(&mut m,LEA16);assert_eq!(m.get32(0),0x1234_0201);
    m.set32(0,0xffff_fffe);m.set32(6,0x2000_0040);let p=ea(&m,0x1fe);m.write16(p,0x3579).unwrap();
    run(&mut m,SIB_LOAD);assert_eq!(m.get32(0),0xffff_3579);
    for (index,expected) in [(ZX8,0x81),(SX8,0xff81),(ZX8_32,0x81),(SX8_32,0xffff_ff81),(ZX16_32,0x8081),(SX16_32,0xffff_8081)] {
        m.set32(0,0x201);let p=ea(&m,0x201);m.write16(p,0x8081).unwrap();m.regs.flags=Flags::from_bits(0xffff);
        run(&mut m,index);assert_eq!(m.get32(0),expected);assert_eq!(m.regs.flags.bits(),0xffff);
    }
    m.set32(0,0x201);let p=ea(&m,0x201);m.write8(p,0x91).unwrap();run(&mut m,LOAD8);assert_eq!(m.get32(0),0x9101);
    m.set32(0,0xabcd_0201);fault(&mut m,STORE8,"segment boundary");
    m.set32(0,0x201);run(&mut m,STORE8);assert_eq!(m.read8(p).unwrap(),2);
    for (index,expected,size) in [(IMM8,0x81,1),(IMM16,0x8081,2),(IMM32,0xffff_8081,4)] {
        m.set32(0,0x201);run(&mut m,index);assert_eq!(get(&m,size),expected);
    }
    m.set32(0,0x201);m.write16(p,0x3456).unwrap();run(&mut m,MOFFS);assert_eq!(m.regs.ax,0x3456);
    m.set32(0,0x201);m.write16(p,1).unwrap();run(&mut m,ADD_REG);assert_eq!(m.get32(0),0x202);
    m.set32(0,0x201);m.write16(p,1).unwrap();run(&mut m,ADD_SELF_SOURCE);assert_eq!(m.read16(p).unwrap(),0x202);assert_eq!(m.get32(0),0x201);
    m.write16(p,0).unwrap();run(&mut m,ADD_SIGN_IMM);assert_eq!(m.read16(p).unwrap(),0xffff);
    check_arithmetic(&mut m);
    // Physical backing failures must not partially write or change flags.
    m.memory=GuestMemory::from_bytes(vec![0x91;0x3000],A20::Enabled);m.regs.cs=0x10;m.regs.ds=0x100;m.set32(0,0x201);
    prepare(&mut m,STORE32); // code stays in backed memory
    m.memory=GuestMemory::from_bytes(m.memory.as_bytes()[..0x1203].to_vec(),A20::Enabled);
    fault(&mut m,STORE32,"outside");fault(&mut m,ADD_32,"outside");
    // Accepted access spanning the A20 boundary uses runtime per-byte gating.
    m.memory=GuestMemory::from_bytes(vec![0;0x100000],A20::Disabled);m.regs.ds=0xffff;m.set32(0,0xf);m.set32(3,0x89ab_cdef);
    run(&mut m,STORE32);assert_eq!(m.memory.as_bytes()[0xfffff],0xef);assert_eq!(&m.memory.as_bytes()[..3],&[0xcd,0xab,0x89]);
    run(&mut m,LOAD32);assert_eq!(m.get32(0),0x89ab_cdef);
    prepare(&mut m,TREE);m.write8(SegmentedAddress::new(m.regs.cs,0x656),0x90).unwrap();
    let before=snapshot(&m);let steps=m.steps;assert!(step(&mut m,TREE).unwrap_err().0.contains("code changed"));assert_eq!(snapshot(&m),before);assert_eq!(m.steps,steps);
    println!("address32 AOT fixtures passed: {} encodings, {} execution steps; independent EA/defined-flags oracles; fault/alias/SS/SIB/A20 checks; no decoder linked",FIXTURES.len(),m.steps);
}
"#;
}
