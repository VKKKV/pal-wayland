// SPDX-License-Identifier: GPL-3.0-or-later
//! AOT emission for 32-bit operands in 16-bit code, with 16-bit addresses/stacks.
//!
//! `emit` owns the final IP update, but not the initial step charge/code guard.
//! Include `helpers()` once in the generated module (with `Flags` in scope).
//! Runtime requirements: Machine::{get32,set32,read32,write32}, `regs`, `steps`,
//! and the existing Flags/SegmentedAddress types. No decoder or opcode dispatch
//! is present in the emitted executable.
//!
//! IMPORTANT: the caller must reject raw 0x67 prefixes before calling `emit`.
//! Iced discards an ineffectual address override on register-only instructions;
//! Instruction alone cannot identify that prefix. Observable non-16-bit EAs
//! and string indices are also rejected here. FS/GS, LOCK, 32-bit control flow,
//! segment-register transfers and unlisted instructions are unsupported.
//! Undefined flags retain their old value; all architecturally defined flags
//! are updated, and unaffected bits are preserved. Faults are terminal, as in
//! lift.rs; a multi-access instruction need not be restartable after a fault.
//! DIV/IDIV r/m32 capture operands first and preserve all architectural state
//! on divide/memory faults; i128 widening makes i64::MIN/-1 return a fault.

#![forbid(unsafe_code)]

use iced_x86::{Code, CodeSize, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::fmt::Write;

/// Private, decoder-free arithmetic helpers for the generated Rust module.
pub fn helpers() -> String {
    r#"
#[allow(dead_code)]
fn aot32_zsp(value: u32, flags: &mut Flags) {
    flags.set(Flags::ZF, value == 0);
    flags.set(Flags::SF, value & 0x8000_0000 != 0);
    flags.set(Flags::PF, (value as u8).count_ones() % 2 == 0);
}
#[allow(dead_code)]
fn aot32_arith(a: u32, b: u32, carry: bool, subtract: bool, flags: &mut Flags) -> u32 {
    let c = u64::from(carry);
    let (value, cf, signed) = if subtract {
        (a.wrapping_sub(b).wrapping_sub(c as u32),
         u64::from(a) < u64::from(b) + c,
         i64::from(a as i32) - i64::from(b as i32) - c as i64)
    } else {
        let wide = u64::from(a) + u64::from(b) + c;
        (wide as u32, wide > u64::from(u32::MAX),
         i64::from(a as i32) + i64::from(b as i32) + c as i64)
    };
    flags.set(Flags::CF, cf);
    flags.set(Flags::OF, signed < i64::from(i32::MIN) || signed > i64::from(i32::MAX));
    flags.set(Flags::AF, (a ^ b ^ value) & 0x10 != 0);
    aot32_zsp(value, flags);
    value
}
"#
    .into()
}

fn register_index(r: Register) -> Result<u8, String> {
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
        _ => return Err(format!("unsupported 32-bit register {r:?}")),
    })
}

fn word_register(r: Register) -> Result<&'static str, String> {
    use Register::*;
    Ok(match r {
        AX => "m.regs.ax",
        CX => "m.regs.cx",
        DX => "m.regs.dx",
        BX => "m.regs.bx",
        SP => "m.regs.sp",
        BP => "m.regs.bp",
        SI => "m.regs.si",
        DI => "m.regs.di",
        CS => "m.regs.cs",
        DS => "m.regs.ds",
        ES => "m.regs.es",
        SS => "m.regs.ss",
        _ => return Err(format!("unsupported 16-bit/segment register {r:?}")),
    })
}

fn offset(i: &Instruction) -> Result<String, String> {
    if i.memory_displ_size() > 2 || i.memory_index_scale() != 1 {
        return Err("unsupported 32-bit address-size override".into());
    }
    let mut expr = format!("{}u16", i.memory_displacement64() as u16);
    for r in [i.memory_base(), i.memory_index()] {
        if r != Register::None {
            if !matches!(r, Register::BX | Register::BP | Register::SI | Register::DI) {
                return Err(format!("unsupported address-size/register {r:?}"));
            }
            expr = format!("({expr}).wrapping_add({})", word_register(r)?);
        }
    }
    Ok(expr)
}

fn bind(i: &Instruction, op: u32, name: &str, out: &mut String) -> Result<(), String> {
    if i.op_kind(op) == OpKind::Memory {
        writeln!(
            out,
            "let {name} = SegmentedAddress::new({}, {});",
            word_register(i.memory_segment())?,
            offset(i)?
        )
        .unwrap();
    }
    Ok(())
}

fn read(i: &Instruction, op: u32, name: &str) -> Result<String, String> {
    Ok(match i.op_kind(op) {
        OpKind::Register => format!("m.get32({})", register_index(i.op_register(op))?),
        OpKind::Memory if i.memory_size().size() == 4 => format!("m.read32({name})?"),
        OpKind::Immediate32 => format!("{}u32", i.immediate32()),
        OpKind::Immediate8to32 => format!("{}u32", i.immediate8to32() as u32),
        _ => return Err(format!("unsupported 32-bit operand {:?}", i.op_kind(op))),
    })
}

fn store(i: &Instruction, op: u32, name: &str, value: &str) -> Result<String, String> {
    Ok(match i.op_kind(op) {
        OpKind::Register => format!(
            "m.set32({}, {value});\n",
            register_index(i.op_register(op))?
        ),
        OpKind::Memory if i.memory_size().size() == 4 => format!("m.write32({name}, {value})?;\n"),
        _ => return Err("operand is not a writable dword".into()),
    })
}

fn strings(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    let (source, dest, compare) = match i.mnemonic() {
        Movsd => (true, true, false),
        Stosd => (false, true, false),
        Lodsd => (true, false, false),
        Cmpsd => (true, true, true),
        Scasd => (false, true, true),
        _ => return Err("unsupported dword string".into()),
    };
    // In particular, do not confuse SSE MOVSD/CMPSD with string instructions.
    let expected = match i.mnemonic() {
        Movsd => [OpKind::MemoryESDI, OpKind::MemorySegSI],
        Cmpsd => [OpKind::MemorySegSI, OpKind::MemoryESDI],
        Stosd => [OpKind::MemoryESDI, OpKind::Register],
        _ => [
            OpKind::Register,
            if source {
                OpKind::MemorySegSI
            } else {
                OpKind::MemoryESDI
            },
        ],
    };
    if i.op_count() != 2
        || [i.op0_kind(), i.op1_kind()] != expected
        || (matches!(i.mnemonic(), Stosd) && i.op1_register() != Register::EAX)
        || (matches!(i.mnemonic(), Lodsd | Scasd) && i.op0_register() != Register::EAX)
    {
        return Err("unsupported dword string form/address size".into());
    }
    let repeated = i.has_rep_prefix() || i.has_repne_prefix();
    let mut s = String::new();
    if repeated {
        s.push_str(
            "for unit in 0..64 {\nif m.regs.cx == 0 { break; }\nif unit != 0 { m.steps += 1; }\n",
        );
    }
    if source {
        let segment = if i.segment_prefix() == Register::None {
            Register::DS
        } else {
            i.segment_prefix()
        };
        writeln!(
            s,
            "let source = SegmentedAddress::new({}, m.regs.si);",
            word_register(segment)?
        )
        .unwrap();
    }
    if dest {
        s.push_str("let dest = SegmentedAddress::new(m.regs.es, m.regs.di);\n");
    }
    // Preflight the entire unit without reading: destination failures must not
    // load source latches, including a failed second operand of CMPSD.
    if dest {
        s.push_str("m.validate_span(dest,4)?;\n");
    }
    if source {
        s.push_str("m.validate_span(source,4)?;\n");
    }
    match i.mnemonic() {
        Movsd => s.push_str("let value = m.read32(source)?; m.write32(dest, value)?;\n"),
        Stosd => s.push_str("let value = m.get32(0); m.write32(dest, value)?;\n"),
        Lodsd => s.push_str("let value = m.read32(source)?; m.set32(0, value);\n"),
        Cmpsd => s.push_str("let a = m.read32(source)?; let b = m.read32(dest)?; aot32_arith(a,b,false,true,&mut m.regs.flags);\n"),
        Scasd => s.push_str("let a = m.get32(0); let b = m.read32(dest)?; aot32_arith(a,b,false,true,&mut m.regs.flags);\n"),
        _ => unreachable!(),
    }
    s.push_str(
        "let delta = if m.regs.flags.contains(Flags::DF) { 0u16.wrapping_sub(4) } else { 4 };\n",
    );
    if source {
        s.push_str("m.regs.si = m.regs.si.wrapping_add(delta);\n");
    }
    if dest {
        s.push_str("m.regs.di = m.regs.di.wrapping_add(delta);\n");
    }
    if repeated {
        s.push_str("m.regs.cx = m.regs.cx.wrapping_sub(1);\n");
        if compare {
            let stop = if i.has_repne_prefix() {
                "m.regs.flags.zf()"
            } else {
                "!m.regs.flags.zf()"
            };
            writeln!(
                s,
                "if {stop} {{ m.regs.ip = {}; return Ok(()); }}",
                i.next_ip16()
            )
            .unwrap();
        }
        s.push_str("}\nif m.regs.cx != 0 { return Ok(()); }\n");
    }
    Ok(s)
}

fn shift(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    let mut s = String::new();
    bind(i, 0, "dst", &mut s)?;
    let count = match i.op1_kind() {
        OpKind::Immediate8 => format!("{}u32", i.immediate8()),
        OpKind::Register if i.op1_register() == Register::CL => "u32::from(m.regs.cx as u8)".into(),
        _ => return Err("unsupported dword shift count".into()),
    };
    writeln!(
        s,
        "let mut value = {}; let count = ({count}) & 31;",
        read(i, 0, "dst")?
    )
    .unwrap();
    // All dword rotations use the same 5-bit count (RCL/RCR never reach 32).
    s.push_str("if count != 0 {\nlet original = value;\nfor _ in 0..count {\n");
    match i.mnemonic() {
        Shl | Sal | Rol | Rcl => {
            let low = match i.mnemonic() {
                Rol => "high",
                Rcl => "m.regs.flags.cf()",
                _ => "false",
            };
            writeln!(s, "let high = value & 0x8000_0000 != 0; value = value.wrapping_shl(1) | u32::from({low}); m.regs.flags.set(Flags::CF, high);").unwrap();
        }
        Shr | Sar | Ror | Rcr => {
            let high = match i.mnemonic() {
                Sar => "value & 0x8000_0000 != 0",
                Ror => "low",
                Rcr => "m.regs.flags.cf()",
                _ => "false",
            };
            writeln!(s, "let low = value & 1 != 0; let high = {high}; value = (value >> 1) | (u32::from(high) << 31); m.regs.flags.set(Flags::CF, low);").unwrap();
        }
        _ => return Err("unsupported dword shift".into()),
    }
    s.push_str("}\n");
    if matches!(i.mnemonic(), Shl | Sal | Shr | Sar) {
        s.push_str("aot32_zsp(value, &mut m.regs.flags);\n");
    }
    let of = match i.mnemonic() {
        Shl | Sal | Rol | Rcl => "(value & 0x8000_0000 != 0) != m.regs.flags.cf()",
        Shr => "original & 0x8000_0000 != 0",
        Sar => "false",
        _ => "((value >> 31) ^ (value >> 30)) & 1 != 0",
    };
    writeln!(s, "if count == 1 {{ m.regs.flags.set(Flags::OF, {of}); }}").unwrap();
    s.push_str(&store(i, 0, "dst", "value")?);
    s.push_str("}\n");
    Ok(s)
}

fn double_shift(i: &Instruction) -> Result<String, String> {
    if i.op_count() != 3 || i.op1_kind() != OpKind::Register {
        return Err("unsupported dword double-shift form".into());
    }
    let count = match i.op2_kind() {
        OpKind::Immediate8 => format!("{}u32", i.immediate8()),
        OpKind::Register if i.op2_register() == Register::CL => "u32::from(m.regs.cx as u8)".into(),
        _ => return Err("unsupported dword double-shift count".into()),
    };
    let mut s = String::new();
    // Capture EA, both operands and CL before any write, including ECX/self
    // aliases. Operand size 32 does not change the 16-bit effective address.
    bind(i, 0, "dst", &mut s)?;
    writeln!(
        s,
        "let original = {}; let source = {}; let count = ({count}) & 31;",
        read(i, 0, "dst")?,
        read(i, 1, "unused")?
    )
    .unwrap();
    // Guard before computing 32-count: Rust must never shift a u32 by 32.
    s.push_str("if count != 0 {\n");
    if i.mnemonic() == Mnemonic::Shld {
        s.push_str("let value = (original << count) | (source >> (32 - count));\nlet carry = (original >> (32 - count)) & 1 != 0;\n");
    } else {
        s.push_str("let value = (original >> count) | (source << (32 - count));\nlet carry = (original >> (count - 1)) & 1 != 0;\n");
    }
    s.push_str("m.regs.flags.set(Flags::CF, carry);\naot32_zsp(value, &mut m.regs.flags);\nif count == 1 { m.regs.flags.set(Flags::OF, (original ^ value) & 0x8000_0000 != 0); }\n");
    s.push_str(&store(i, 0, "dst", "value")?);
    s.push_str("}\n");
    Ok(s)
}

/// Unsigned one-operand MUL writes the full product to implicit EDX:EAX.
fn unsigned_multiply(i: &Instruction) -> Result<String, String> {
    if i.code() != Code::Mul_rm32 || i.op_count() != 1 {
        return Err("unsupported unsigned dword multiply form".into());
    }
    let mut s = String::new();
    // Capture EA and both inputs before either implicit output is overwritten,
    // especially MUL EAX / MUL EDX. Validate the full read before VGA latches.
    bind(i, 0, "src", &mut s)?;
    if i.op0_kind() == OpKind::Memory {
        s.push_str("m.validate_span(src, 4)?;\n");
    }
    writeln!(s, "let a = m.get32(0);\nlet b = {};", read(i, 0, "src")?).unwrap();
    // All unsigned dword products fit u64, including MAX * MAX.
    s.push_str("let product = u64::from(a) * u64::from(b);\nlet high = (product >> 32) as u32;\nm.set32(0, product as u32);\nm.set32(2, high);\n");
    // SF/ZF/AF/PF are undefined; retain them and all unaffected flag bits.
    s.push_str("m.regs.flags.set(Flags::CF | Flags::OF, high != 0);\n");
    Ok(s)
}

/// The two-/three-operand IMUL forms write only their explicit GPR result.
/// One-operand IMUL (implicit EDX:EAX) remains unsupported.
fn multiply(i: &Instruction) -> Result<String, String> {
    let immediate = match i.code() {
        Code::Imul_r32_rm32 if i.op_count() == 2 => false,
        Code::Imul_r32_rm32_imm8 | Code::Imul_r32_rm32_imm32 if i.op_count() == 3 => true,
        _ => return Err("unsupported dword multiply form".into()),
    };
    if i.op0_kind() != OpKind::Register {
        return Err("dword IMUL requires a register destination".into());
    }
    register_index(i.op0_register())?;
    let mut s = String::new();
    // Bind the old EA before an aliased destination can overwrite BX/BP/SI/DI.
    // Preflight the full dword before a device read can load VGA latches.
    bind(i, 1, "src", &mut s)?;
    if i.op1_kind() == OpKind::Memory {
        s.push_str("m.validate_span(src, 4)?;\n");
    }
    let (a, b) = if immediate {
        (read(i, 1, "src")?, read(i, 2, "unused")?)
    } else {
        (read(i, 0, "unused")?, read(i, 1, "src")?)
    };
    writeln!(s, "let a = {a};\nlet b = {b};").unwrap();
    // Every signed dword product fits i64. Immediate8to32 was sign-extended
    // by read(); truncation is permitted only after forming the full product.
    s.push_str("let product = i64::from(a as i32) * i64::from(b as i32);\nlet value = product as u32;\nlet overflow = product < i64::from(i32::MIN) || product > i64::from(i32::MAX);\n");
    s.push_str(&store(i, 0, "unused", "value")?);
    // SF/ZF/AF/PF are undefined; preserve them by policy, not as CPU outputs.
    s.push_str("m.regs.flags.set(Flags::CF | Flags::OF, overflow);\n");
    Ok(s)
}

/// DIV/IDIV have one explicit dword operand and implicit EDX:EAX results.
/// Check the exact decoded code so word/byte forms cannot leak into this path.
fn divide(i: &Instruction) -> Result<String, String> {
    if !matches!(i.code(), Code::Div_rm32 | Code::Idiv_rm32) || i.op_count() != 1 {
        return Err("unsupported dword divide form".into());
    }
    let mut s = String::new();
    // Capture the 16-bit EA and divisor before either implicit result is written,
    // including DIV/IDIV EAX and EDX. Failed reads leave architectural state intact.
    bind(i, 0, "src", &mut s)?;
    writeln!(s, "let operand = {};", read(i, 0, "src")?).unwrap();
    s.push_str("let bits = (u64::from(m.get32(2)) << 32) | u64::from(m.get32(0));\n");
    if i.code() == Code::Div_rm32 {
        s.push_str("let dividend = bits; let divisor = u64::from(operand);\n");
    } else {
        // Widen BEFORE division: i64::MIN / -1 must report #DE, not panic in
        // Rust (even optimized builds). Sign comes from bit 63 of EDX:EAX.
        s.push_str(
            "let dividend = bits as i64 as i128; let divisor = i128::from(operand as i32);\n",
        );
    }
    s.push_str("if divisor == 0 { return Err(m.fault(\"divide error: zero divisor\")); }\nlet quotient = dividend / divisor;\n");
    let overflow = if i.code() == Code::Div_rm32 {
        "quotient > u64::from(u32::MAX)"
    } else {
        "quotient < i128::from(i32::MIN) || quotient > i128::from(i32::MAX)"
    };
    writeln!(s, "if {overflow} {{ return Err(m.fault(\"divide error: quotient overflow\")); }}\nlet remainder = dividend % divisor;\nm.set32(0, quotient as u32);\nm.set32(2, remainder as u32);").unwrap();
    // Intel SDM DIV/IDIV: CF/OF/SF/ZF/AF/PF are undefined. Preserve them by
    // project policy; all other flags are unaffected. No state writes precede #DE.
    Ok(s)
}

/// Emit one instruction's statements. A pending REP keeps the current IP and
/// returns Ok from the enclosing step function; other successes set next_ip16.
/// The caller must guard original bytes and charge the first step beforehand.
pub fn emit(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    if i.is_invalid() || i.code_size() != CodeSize::Code16 {
        return Err("dword emitter requires a valid decoded 16-bit instruction".into());
    }
    if i.flow_control() != FlowControl::Next {
        return Err("unsupported 32-bit control flow".into());
    }
    if i.has_lock_prefix() {
        return Err("LOCK/atomic memory semantics are unsupported".into());
    }
    if matches!(i.segment_prefix(), Register::FS | Register::GS) {
        return Err("FS/GS segments are unsupported".into());
    }
    // Check EAs even on instructions (such as LEA) that do not use bind().
    if i.op_kinds().any(|kind| kind == OpKind::Memory) {
        offset(i)?;
    }
    let mnemonic = i.mnemonic();
    let is_string = matches!(mnemonic, Movsd | Stosd | Lodsd | Cmpsd | Scasd);
    if (i.has_rep_prefix() || i.has_repne_prefix()) && !is_string {
        return Err("REP on a non-string dword instruction is unsupported".into());
    }
    let mut s = String::new();
    match mnemonic {
        Mov | Xchg => {
            bind(i, 0, "dst", &mut s)?;
            bind(i, 1, "src", &mut s)?;
            if mnemonic == Xchg {
                writeln!(s, "let old = {};", read(i, 0, "dst")?).unwrap();
            }
            writeln!(s, "let value = {};", read(i, 1, "src")?).unwrap();
            s.push_str(&store(i, 0, "dst", "value")?);
            if mnemonic == Xchg { s.push_str(&store(i, 1, "src", "old")?); }
        }
        Lea => {
            if i.op1_kind() != OpKind::Memory || i.op0_kind() != OpKind::Register {
                return Err("unsupported dword LEA form".into());
            }
            writeln!(s, "let value = u32::from({});", offset(i)?).unwrap();
            s.push_str(&store(i, 0, "unused", "value")?);
        }
        Add | Adc | Sub | Sbb | Cmp | And | Or | Xor | Test => {
            bind(i, 0, "dst", &mut s)?;
            bind(i, 1, "src", &mut s)?;
            writeln!(s, "let a = {}; let b = {};", read(i, 0, "dst")?, read(i, 1, "src")?).unwrap();
            if matches!(mnemonic, And | Or | Xor | Test) {
                let operator = match mnemonic { And | Test => "&", Or => "|", _ => "^" };
                writeln!(s, "let value = a {operator} b; m.regs.flags.set(Flags::CF | Flags::OF, false); aot32_zsp(value, &mut m.regs.flags);").unwrap();
            } else {
                let carry = if matches!(mnemonic, Adc | Sbb) { "m.regs.flags.cf()" } else { "false" };
                writeln!(s, "let carry = {carry}; let value = aot32_arith(a,b,carry,{},&mut m.regs.flags);", matches!(mnemonic, Sub | Sbb | Cmp)).unwrap();
            }
            if !matches!(mnemonic, Cmp | Test) { s.push_str(&store(i, 0, "dst", "value")?); }
        }
        Inc | Dec | Neg | Not => {
            bind(i, 0, "dst", &mut s)?;
            writeln!(s, "let a = {};", read(i, 0, "dst")?).unwrap();
            match mnemonic {
                Not => s.push_str("let value = !a;\n"),
                Neg => s.push_str("let value = aot32_arith(0,a,false,true,&mut m.regs.flags);\n"),
                _ => writeln!(s, "let cf = m.regs.flags.cf(); let value = aot32_arith(a,1,false,{},&mut m.regs.flags); m.regs.flags.set(Flags::CF, cf);", mnemonic == Dec).unwrap(),
            }
            s.push_str(&store(i, 0, "dst", "value")?);
        }
        Push => {
            bind(i, 0, "src", &mut s)?;
            s.push_str("let sp = m.regs.sp.wrapping_sub(4); let stack = SegmentedAddress::new(m.regs.ss,sp); m.validate_ram_span(stack,4)?;\n");
            writeln!(s, "let value = {}; m.write32(stack,value)?; m.regs.sp = sp;", read(i, 0, "src")?).unwrap();
        }
        Pop => {
            // With address-size 16 no destination EA can depend on SP.
            bind(i, 0, "dst", &mut s)?;
            s.push_str("let stack = SegmentedAddress::new(m.regs.ss,m.regs.sp); m.validate_ram_span(stack,4)?; let value = m.read32(stack)?; m.regs.sp = m.regs.sp.wrapping_add(4);\n");
            s.push_str(&store(i, 0, "dst", "value")?);
        }
        Cwde => s.push_str("let value = m.regs.ax as i16 as i32 as u32; m.set32(0,value);\n"),
        Cdq => s.push_str("let value = if m.get32(0) & 0x8000_0000 != 0 { u32::MAX } else { 0 }; m.set32(2,value);\n"),
        Mul => s.push_str(&unsigned_multiply(i)?),
        Imul => s.push_str(&multiply(i)?),
        Div | Idiv => s.push_str(&divide(i)?),
        Shl | Sal | Shr | Sar | Rol | Ror | Rcl | Rcr => s.push_str(&shift(i)?),
        Shld | Shrd => s.push_str(&double_shift(i)?),
        Movsd | Stosd | Lodsd | Cmpsd | Scasd => s.push_str(&strings(i)?),
        _ => return Err(format!("unsupported 32-bit operand instruction {:?}", i.code())),
    }
    writeln!(s, "m.regs.ip = {};", i.next_ip16()).unwrap();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::{Decoder, DecoderOptions};

    fn decode(bytes: &[u8]) -> Instruction {
        let mut decoder = Decoder::with_ip(16, bytes, 0x100, DecoderOptions::NONE);
        let instruction = decoder.decode();
        assert!(!instruction.is_invalid(), "{bytes:02x?}");
        assert_eq!(instruction.len(), bytes.len(), "{bytes:02x?}");
        instruction
    }

    #[test]
    fn explicit_unsupported_forms_never_emit_truncated_semantics() {
        for bytes in [
            &[0x67, 0x66, 0x8b, 0x00][..],   // [EAX]
            &[0x67, 0x66, 0xa1, 0, 0, 0, 0], // absolute disp32
            &[0x67, 0xf3, 0x66, 0xab],       // REP STOSD with EDI/ECX
            &[0x66, 0xe8, 0, 0, 0, 0],
            &[0x66, 0xc3],
            &[0x66, 0xff, 0xe0],
            &[0xf0, 0x66, 0x01, 0x07],
            &[0x64, 0x66, 0x8b, 0x07],
            &[0x66, 0xf7, 0xeb],       // implicit signed IMUL remains unsupported
            &[0x66, 0x8c, 0xd8],       // segment-to-GPR special width semantics
            &[0xf2, 0x0f, 0x10, 0xc0], // SSE MOVSD, not a string
            &[0xf3, 0x66, 0x31, 0xc0], // meaningless REP rejected
            &[0x0f, 0xa4, 0xd8, 1],    // word SHLD is not a dword
            &[0x0f, 0xac, 0xd8, 1],    // word SHRD is not a dword
            &[0x67, 0x66, 0x0f, 0xa4, 0x30, 1], // addr32 [EAX],ESI
            &[0x67, 0x66, 0x0f, 0xad, 0x2a], // addr32 [EDX],EBP
            &[0x64, 0x66, 0x0f, 0xa5, 0x30], // FS:[BX+SI]
            &[0xf3, 0x66, 0x0f, 0xa4, 0xd8, 1], // REP double shift
        ] {
            assert!(emit(&decode(bytes)).is_err(), "{bytes:02x?}");
        }
        let mut i = decode(&[0x66, 0x33, 0xc0]);
        i.set_code_size(CodeSize::Code32);
        assert!(emit(&i).is_err());
    }

    #[test]
    fn divide_forms_reject_wrong_width_and_address_size() {
        for bytes in [
            &[0xf7, 0xf1][..],
            &[0xf7, 0xf9], // word DIV/IDIV
            &[0xf6, 0xf1],
            &[0xf6, 0xf9], // byte DIV/IDIV
            &[0x67, 0x66, 0xf7, 0x30],
            &[0x67, 0x66, 0xf7, 0x38],
            &[0x64, 0x66, 0xf7, 0x37],
            &[0xf3, 0x66, 0xf7, 0xf9],
        ] {
            assert!(emit(&decode(bytes)).is_err(), "{bytes:02x?}");
        }
    }

    #[test]
    fn generated_divide_fixtures_execute_debug_release_and_kill_mutants() {
        let mut fixtures = Vec::new();
        for signed in [false, true] {
            let field = if signed { 7 } else { 6 };
            for register in 0..8 {
                fixtures.push((
                    signed,
                    register,
                    vec![0x66, 0xf7, 0xc0 | field << 3 | register],
                ));
            }
            for rm in 0..8 {
                fixtures.push((
                    signed,
                    8 + rm,
                    vec![0x66, 0xf7, 0x40 | field << 3 | rm, 0xfe],
                ));
            }
            fixtures.push((signed, 16, vec![0x66, 0xf7, field << 3 | 6, 0x01, 0x02]));
            fixtures.push((
                signed,
                17,
                vec![0x26, 0x66, 0xf7, 0x40 | field << 3 | 2, 0xfe],
            ));
        }
        let root = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = root.join(format!("pal-aot-div32-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut program = String::from(
            "#![allow(dead_code,unused_variables,unused_parens)]\n#![forbid(unsafe_code)]\n",
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
        program.push_str("use runtime::{Machine,Fault}; use semantics::*;\nconst FIXTURES: &[(bool,u8,&[u8])] = &[\n");
        for (signed, kind, bytes) in &fixtures {
            writeln!(program, "({signed},{kind},&{bytes:?}),").unwrap();
        }
        program.push_str("];\nfn step(m:&mut Machine,index:usize)->Result<(),Fault> { m.guard_code(FIXTURES[index].2)?; m.steps+=1; match index {\n");
        for (index, (_, _, bytes)) in fixtures.iter().enumerate() {
            let body = emit(&decode(bytes)).unwrap();
            assert!(!body.contains("Decoder") && !body.contains("iced_x86"));
            writeln!(program, "{index}=>{{ {body} }},").unwrap();
        }
        program.push_str("_=>unreachable!(), } Ok(()) }\n");
        writeln!(program, "fn cdq(m:&mut Machine)->Result<(),Fault> {{ m.guard_code(&[0x66,0x99])?; m.steps+=1; {} Ok(()) }}", emit(&decode(&[0x66, 0x99])).unwrap()).unwrap();
        program.push_str(DIVIDE_EXECUTION_TESTS);
        for release in [false, true] {
            let profile = if release { "release" } else { "debug" };
            let path = dir.join(format!("{profile}.rs"));
            std::fs::write(&path, &program).unwrap();
            let binary = dir.join(profile);
            let result = crate::lift::runtime_fixture_rustc()
                .args([
                    "--edition=2024",
                    "-Dwarnings",
                    "-C",
                    if release {
                        "opt-level=3"
                    } else {
                        "opt-level=0"
                    },
                    "-C",
                    if release {
                        "overflow-checks=no"
                    } else {
                        "overflow-checks=yes"
                    },
                ])
                .arg(&path)
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let result = std::process::Command::new(&binary).output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            println!("{profile}: {}", String::from_utf8_lossy(&result.stdout));
            for (name, old, new) in [
                ("zero", "if divisor == 0", "if false"),
                ("overflow", "if quotient > u64::from(u32::MAX)", "if false"),
                (
                    "signed_overflow",
                    "if quotient < i128::from(i32::MIN) || quotient > i128::from(i32::MAX)",
                    "if false",
                ),
                ("signed", "bits as i64 as i128", "i128::from(bits)"),
            ] {
                assert!(program.contains(old));
                let mut mutant = program.replace(old, new);
                // Keep the zero-check mutation executable rather than relying on
                // Rust's divide-by-zero panic as the regression signal.
                if name == "zero" {
                    mutant = mutant
                        .replace("dividend / divisor", "dividend / divisor.max(1)")
                        .replace("dividend % divisor", "dividend % divisor.max(1)");
                }
                let path = dir.join(format!("mutant_{name}_{profile}.rs"));
                std::fs::write(&path, mutant).unwrap();
                let binary = dir.join(format!("mutant_{name}_{profile}"));
                let result = crate::lift::runtime_fixture_rustc()
                    .args([
                        "--edition=2024",
                        "-Dwarnings",
                        "-C",
                        if release {
                            "opt-level=3"
                        } else {
                            "opt-level=0"
                        },
                    ])
                    .arg(path)
                    .arg("-o")
                    .arg(&binary)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "mutant compile: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let result = std::process::Command::new(binary)
                    .arg(name)
                    .output()
                    .unwrap();
                let stderr = String::from_utf8_lossy(&result.stderr);
                assert!(
                    !result.status.success() && stderr.contains("DIV32 semantic mismatch"),
                    "mutant {name} escaped or failed outside assertion: {stderr}"
                );
                println!("{profile}: mutation {name} rejected by execution assertion");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    const DIVIDE_EXECUTION_TESTS: &str = r#"
fn machine() -> Machine {
    let mut bytes=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {
        bytes[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());
    }
    let mut m=Machine::from_mz(&bytes,0x1010).unwrap();
    m.memory=GuestMemory::new(0x20000,A20::Disabled);
    m.regs=Registers{cs:0x100,ds:0x200,ss:0x300,es:0x400,..Registers::default()};
    m
}
fn prepare(m:&mut Machine,index:usize) {
    m.regs.ip=0x100;
    m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,0x100),FIXTURES[index].2).unwrap();
}
// Independent wide oracle: multiply/add concatenation; manual sign conversion
// and magnitude division rather than production signed casts, / and %.
fn oracle(signed:bool,low:u32,high:u32,operand:u32)->Result<(u32,u32),&'static str> {
    let base=1i128<<32;
    let raw=i128::from(high)*base+i128::from(low);
    let dividend=if signed && high>=0x8000_0000 {raw-base*base} else {raw};
    let divisor=if signed && operand>=0x8000_0000 {i128::from(operand)-base} else {i128::from(operand)};
    if divisor==0 {return Err("zero divisor");}
    let magnitude=dividend.abs()/divisor.abs();
    let q=if (dividend<0)!=(divisor<0) {-magnitude} else {magnitude};
    let (min,max)=if signed {(-base/2,base/2-1)} else {(0,base-1)};
    if q<min || q>max {return Err("quotient overflow");}
    let remainder=dividend-q*divisor;
    Ok((q.rem_euclid(base) as u32,remainder.rem_euclid(base) as u32))
}
fn ea(m:&Machine,kind:u8)->SegmentedAddress {
    let offset=match kind {
        8=>m.regs.bx.wrapping_add(m.regs.si),9=>m.regs.bx.wrapping_add(m.regs.di),
        10|17=>m.regs.bp.wrapping_add(m.regs.si),11=>m.regs.bp.wrapping_add(m.regs.di),
        12=>m.regs.si,13=>m.regs.di,14=>m.regs.bp,15=>m.regs.bx,16=>0x203,_=>0x203,
    }.wrapping_sub(2);
    let segment=if kind==17 {m.regs.es} else if matches!(kind,10|11|14) {m.regs.ss} else {m.regs.ds};
    SegmentedAddress::new(segment,offset)
}
fn verify(m:&mut Machine,index:usize,expected:Result<(u32,u32),&str>,full_memory:bool) {
    prepare(m,index);
    let before=m.regs;
    let gprs:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));
    let memory=if full_memory {Some(m.memory.clone())} else {None};
    let steps=m.steps;
    let result=step(m,index);
    match expected {
        Ok((q,r))=>{
            assert!(result.is_ok(),"DIV32 semantic mismatch: unexpected fault {result:?}");
            let mut regs=before;regs.ax=q as u16;regs.dx=r as u16;
            regs.ip=before.ip.wrapping_add(FIXTURES[index].2.len() as u16);
            assert_eq!(m.regs,regs,"DIV32 semantic mismatch: registers index={index}");
            for reg in 0..8 {assert_eq!(m.get32(reg),match reg {0=>q,2=>r,_=>gprs[reg as usize]},"DIV32 semantic mismatch: upper/GPR {reg}, index={index}");}
        },
        Err(reason)=>{
            assert!(matches!(result,Err(ref f) if f.0.contains(reason)),"DIV32 semantic mismatch: expected {reason}, got {result:?}");
            assert_eq!(m.regs,before,"DIV32 semantic mismatch: non-atomic fault");
            for reg in 0..8 {assert_eq!(m.get32(reg),gprs[reg as usize],"DIV32 semantic mismatch: fault upper/GPR");}
        },
    }
    assert_eq!(m.steps,steps+1);
    if let Some(memory)=memory {assert_eq!(m.memory,memory,"DIV32 semantic mismatch: memory changed");}
}
fn case(m:&mut Machine,index:usize,low:u32,high:u32,operand:u32,flags:u16,full_memory:bool) {
    for r in 0..8 {m.set32(r,0x9137_fff0+u32::from(r)*0x10000);}
    m.set32(0,low);m.set32(2,high);m.regs.flags=Flags::from_bits(flags);
    let (signed,kind,_)=FIXTURES[index];
    if kind<8 {m.set32(kind,operand);} else {
        let at=ea(m,kind);
        // Poison alternate DS/SS/ES to expose wrong default/override segments.
        for segment in [m.regs.ds,m.regs.ss,m.regs.es] {m.write32(SegmentedAddress::new(segment,at.offset),0).unwrap();}
        m.write32(at,operand).unwrap();
    }
    let captured=if kind<8 {m.get32(kind)} else {m.read32(ea(m,kind)).unwrap()};
    let expected=oracle(signed,m.get32(0),m.get32(2),captured);
    verify(m,index,expected,full_memory);
}
fn main() {
    let mut m=machine();
    let mode=std::env::args().nth(1).unwrap_or_default();
    // Mutation probes assert a specific result, not merely successful execution.
    if mode=="zero" {case(&mut m,1,7,0,0,0xa555,true);return;}
    if mode=="overflow" {case(&mut m,1,0,1,1,0xa555,true);return;}
    if mode=="signed_overflow" {case(&mut m,19,0,0x8000_0000,u32::MAX,0xa555,true);return;}
    if mode=="signed" {case(&mut m,19,(-7i32) as u32,u32::MAX,3,0xa555,true);return;}
    let edges=[0,1,2,3,0x400,0xf000,0xffff,0x10000,0x7fff_ffff,0x8000_0000,0x8000_0001,0xffff_fffe,u32::MAX];
    let mut cases=0u64;let mut faults=0u64;
    for index in 0..FIXTURES.len() {
        for low in edges {for high in [0,1,0x7fff_ffff,0x8000_0000,u32::MAX] {for operand in edges {for flags in [0,0xffff,0xa555,0x5aaa] {
            let steps=m.steps;
            case(&mut m,index,low,high,operand,flags,true);
            cases+=1;
            if m.regs.ip==0x100 {faults+=1;}
            assert_eq!(m.steps,steps+1);
        }}}}
    }
    let mut random=0x17af_834du32;
    for index in 0..FIXTURES.len() {for n in 0..4096 {
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let low=random;
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let operand=random;
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);
        // Mix arbitrary 64-bit dividends with many successful signed/unsigned cases.
        let high=if n%3==0 {random} else if FIXTURES[index].0 && low>=0x8000_0000 {u32::MAX} else {0};
        case(&mut m,index,low,high,operand,(random>>16) as u16,true);
        cases+=1;if m.regs.ip==0x100 {faults+=1;}
    }}
    // Explicit terminal faults and signed boundaries (including i64::MIN/-1).
    for (low,high,divisor) in [
        (0,0,0),(0,0x8000_0000,u32::MAX),(0x8000_0000,u32::MAX,u32::MAX),
        (0x8000_0000,u32::MAX,1),(0x7fff_ffff,0,1),(0x8000_0000,0,1),
        (0x7fff_ffff,u32::MAX,1),(1,0,u32::MAX),(u32::MAX,u32::MAX,2),
        (7,0,(-3i32) as u32),((-7i32) as u32,u32::MAX,3),
        ((-7i32) as u32,u32::MAX,(-3i32) as u32),(0xf000,0,0x400),
    ] {case(&mut m,19,low,high,divisor,0xffff,true);cases+=1;}
    // CDQ uses full EAX sign, not AX sign, and preserves all flags/GPRs.
    for low in edges {
        m.set32(0,low);m.set32(2,0x1357_2468);m.regs.flags=Flags::from_bits(0xa555);
        m.regs.ip=0x100;m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,0x100),&[0x66,0x99]).unwrap();
        let regs=m.regs;let before:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));let memory=m.memory.clone();let steps=m.steps;
        cdq(&mut m).unwrap();let high=if low>=0x8000_0000 {u32::MAX} else {0};
        let mut expected=regs;expected.dx=high as u16;expected.ip=0x102;
        assert_eq!(m.regs,expected);assert_eq!(m.memory,memory);assert_eq!(m.steps,steps+1);
        for r in 0..8 {assert_eq!(m.get32(r),if r==2 {high} else {before[r as usize]});}
        m.set32(1,3);verify(&mut m,19,oracle(true,low,high,3),true);cases+=1;
    }
    // Segment end, physical short buffer, A20-enabled HMA, A20-wrap failures.
    for index in [15,33] {
        for offset in [0xfffdu16,0xfffe,0xffff] {
            m.regs.bx=offset.wrapping_add(2);verify(&mut m,index,Err("segment boundary"),true);cases+=1;
        }
        m.regs.bx=0xfffe;let at=ea(&m,15);m.write32(at,3).unwrap();
        m.set32(0,7);m.set32(2,0);verify(&mut m,index,Ok((2,1)),true);cases+=1;
        for (a20,ds,bx,size) in [(A20::Disabled,0x200,0x103,0x2104),(A20::Enabled,0xffff,0x22,0x10000),(A20::Disabled,0xffff,0x11,0x2000)] {
            m.memory=GuestMemory::new(size,a20);m.regs.ds=ds;m.regs.bx=bx;
            verify(&mut m,index,Err("outside"),true);cases+=1;
        }
        m.memory=GuestMemory::new(0x20000,A20::Disabled);m.regs.ds=0x200;
    }
    // Original exact-byte guard rejects a changed ModR/M before step charging.
    prepare(&mut m,19);m.memory.write8(SegmentedAddress::new(m.regs.cs,0x102),0xf8).unwrap();
    let regs=m.regs;let memory=m.memory.clone();let steps=m.steps;let before:[u32;8]=std::array::from_fn(|r|m.get32(r as u8));
    assert!(step(&mut m,19).unwrap_err().0.contains("code changed"));
    assert_eq!(m.regs,regs);assert_eq!(m.memory,memory);assert_eq!(m.steps,steps);
    for r in 0..8 {assert_eq!(m.get32(r),before[r as usize]);}
    println!("DIV/IDIV: {} encodings, {cases} oracle executions, {faults} arithmetic faults checked atomically; full GPR/upper/flags/memory/IP checks; CDQ, EA16, guards, no decoder",FIXTURES.len());
}
"#;

    #[test]
    fn generated_dword_fixtures_compile_and_execute_with_real_runtime() {
        // Only original synthetic fixtures. The generated executable links the
        // actual runtime by source and no iced-x86/decoder/interpreter at all.
        let mut fixtures: Vec<(String, Vec<u8>)> = Vec::new();
        let mut add = |name: &str, bytes: &[u8]| fixtures.push((name.into(), bytes.into()));
        for (name, bytes) in [
            ("XOR_SELF", &[0x66, 0x33, 0xc0][..]),
            ("ADD", &[0x66, 0x01, 0xd8]),
            ("ADC", &[0x66, 0x11, 0xd8]),
            ("SUB", &[0x66, 0x29, 0xd8]),
            ("SBB", &[0x66, 0x19, 0xd8]),
            ("CMP", &[0x66, 0x39, 0xd8]),
            ("AND", &[0x66, 0x21, 0xd8]),
            ("OR", &[0x66, 0x09, 0xd8]),
            ("XOR", &[0x66, 0x31, 0xd8]),
            ("TEST", &[0x66, 0x85, 0xd8]),
            ("INC", &[0x66, 0x40]),
            ("DEC", &[0x66, 0x48]),
            ("NEG", &[0x66, 0xf7, 0xd8]),
            ("NOT", &[0x66, 0xf7, 0xd0]),
            ("ADD_IMM", &[0x66, 0x83, 0xc0, 0xff]),
            ("MOV_MEM", &[0x66, 0x89, 0x30]),      // [BX+SI],ESI
            ("MOV_LOAD", &[0x66, 0x8b, 0x30]),     // ESI,[BX+SI]
            ("XCHG_MEM", &[0x66, 0x87, 0x30]),     // [BX+SI],ESI, aliased EA
            ("MOV_SS", &[0x66, 0x8b, 0x42, 0xfe]), // EAX,[BP+SI-2]
            ("MOV_ES", &[0x26, 0x66, 0x8b, 0x42, 0xfe]),
            ("MOV_MOFFS", &[0x66, 0xa1, 1, 2]),
            ("MOV_STORE_IMM", &[0x66, 0xc7, 0x07, 0xef, 0xcd, 0xab, 0x89]),
            ("ADD_MEM", &[0x66, 0x01, 0x07]), // [BX],EAX
            ("LEA", &[0x66, 0x8d, 0x30]),
            ("PUSH_ESP", &[0x66, 0x54]),
            ("POP_ESP", &[0x66, 0x5c]),
            ("PUSH_IMM", &[0x66, 0x6a, 0xff]),
            ("PUSH_MEM", &[0x66, 0xff, 0x37]),
            ("POP_MEM", &[0x66, 0x8f, 0x07]),
            ("CWDE", &[0x66, 0x98]),
            ("CDQ", &[0x66, 0x99]),
            ("REP_STOS", &[0xf3, 0x66, 0xab]),
            ("REP_MOVS", &[0xf3, 0x66, 0xa5]),
            ("REP_LODS", &[0xf3, 0x66, 0xad]),
            ("REPE_CMPS", &[0xf3, 0x66, 0xa7]),
            ("REPNE_CMPS", &[0xf2, 0x66, 0xa7]),
            ("REPE_SCAS", &[0xf3, 0x66, 0xaf]),
            ("REPNE_SCAS", &[0xf2, 0x66, 0xaf]),
            ("MOVS", &[0x66, 0xa5]),
            ("STOS", &[0x66, 0xab]),
            ("LODS", &[0x66, 0xad]),
            ("CMPS", &[0x66, 0xa7]),
            ("SCAS", &[0x66, 0xaf]),
            ("LODS_ES", &[0x26, 0x66, 0xad]),
        ] {
            add(name, bytes);
        }
        for register in 0..8u8 {
            add(
                &format!("MOV_R{register}"),
                &[0x66, 0xb8 + register, 0xef, 0xcd, 0xab, 0x89],
            );
            add(&format!("PUSH_R{register}"), &[0x66, 0x50 + register]);
            add(&format!("POP_R{register}"), &[0x66, 0x58 + register]);
        }
        for (name, field) in [
            ("ROL", 0),
            ("ROR", 1),
            ("RCL", 2),
            ("RCR", 3),
            ("SHL", 4),
            ("SHR", 5),
            ("SAR", 7),
        ] {
            add(name, &[0x66, 0xd3, 0xc0 | field << 3]);
            add(&format!("{name}_ONE"), &[0x66, 0xd1, 0xc0 | field << 3]);
            add(&format!("{name}_MEM"), &[0x66, 0xc1, 0x07 | field << 3, 1]);
        }
        // Every immediate byte is independently decoded and compiled, not
        // patched at runtime. CL covers self, count and 16-bit EA aliases.
        let mut double_shifts = Vec::new();
        for (left, immediate, cl) in [(true, 0xa4, 0xa5), (false, 0xac, 0xad)] {
            for count in 0..=255u8 {
                for (kind, modrm) in [(0, 0xd8), (4, 0x30)] {
                    let name = format!("DOUBLE_{}_IMM_{kind}_{count}", u8::from(left));
                    add(&name, &[0x66, 0x0f, immediate, modrm, count]);
                    double_shifts.push((name, left, Some(count), kind));
                }
            }
            for (kind, bytes) in [
                (0, vec![0x66, 0x0f, cl, 0xd8]),       // EAX,EBX,CL
                (1, vec![0x66, 0x0f, cl, 0xc0]),       // EAX,EAX,CL
                (2, vec![0x66, 0x0f, cl, 0xd9]),       // ECX,EBX,CL
                (3, vec![0x66, 0x0f, cl, 0xc8]),       // EAX,ECX,CL
                (4, vec![0x66, 0x0f, cl, 0x30]),       // [BX+SI],ESI,CL
                (5, vec![0x66, 0x0f, cl, 0x2a]),       // SS:[BP+SI],EBP,CL
                (6, vec![0x26, 0x66, 0x0f, cl, 0x2a]), // ES override
                (7, vec![0x66, 0x0f, cl, 0xc9]),       // ECX,ECX,CL
            ] {
                let name = format!("DOUBLE_{}_CL_{kind}", u8::from(left));
                add(&name, &bytes);
                double_shifts.push((name, left, None, kind));
            }
        }
        let root = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = root.join(format!("pal-aot-dword-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut program = String::from(
            "#![allow(dead_code,unused_variables,unused_parens)]\n#![forbid(unsafe_code)]\n",
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
        program.push_str("use runtime::{Machine,Fault};\nuse semantics::*;\n");
        program.push_str(&helpers());
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
            assert!(!body.contains("Decoder") && !body.contains("iced_x86"));
            writeln!(program, "{index} => {{ {body} }},").unwrap();
        }
        program.push_str("_ => unreachable!(),\n}\nOk(())\n}\n");
        program.push_str("const DOUBLE_SHIFTS: &[(usize,bool,Option<u8>,u8)] = &[\n");
        for (name, left, count, kind) in double_shifts {
            writeln!(program, "({name},{left},{count:?},{kind}),").unwrap();
        }
        program.push_str("];\n");
        program.push_str(EXECUTION_TESTS);
        program.push_str(DOUBLE_SHIFT_TESTS);
        // Unsafe is confined to a separately compiled host-CPU oracle; the
        // emitted executable and production module retain forbid(unsafe_code).
        if cfg!(target_arch = "x86_64") {
            let oracle_path = dir.join("host.rs");
            std::fs::write(&oracle_path, HOST_DOUBLE_SHIFT_ORACLE).unwrap();
            let host = std::process::Command::new("rustc")
                .args(["--edition=2024", "-Dwarnings"])
                .arg(&oracle_path)
                .arg("-o")
                .arg(dir.join("host"))
                .output()
                .unwrap();
            assert!(
                host.status.success(),
                "{}",
                String::from_utf8_lossy(&host.stderr)
            );
        }
        let path = dir.join("main.rs");
        std::fs::write(&path, program).unwrap();
        let compiler = crate::lift::runtime_fixture_rustc()
            .args(["--edition=2024", "-Dwarnings", "-C", "overflow-checks=yes"])
            .arg(&path)
            .arg("-o")
            .arg(dir.join("run"))
            .output()
            .unwrap();
        assert!(
            compiler.status.success(),
            "{}",
            String::from_utf8_lossy(&compiler.stderr)
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
        assert!(output.contains("dword AOT fixtures passed"), "{output}");
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
    m.regs = Registers { cs:0x200,ds:0x6000,es:0x7000,ss:0x8000,sp:0xfffc,..Registers::default() };
    m
}
fn prepare(m: &mut Machine, index:usize) {
    m.regs.ip = 0x100;
    m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,0x100),FIXTURES[index]).unwrap();
}
fn run(m: &mut Machine, index:usize) {
    prepare(m,index);
    let steps = m.steps;
    step(m,index).unwrap();
    assert_eq!(m.steps,steps+1);
    assert_eq!(m.regs.ip,0x100+FIXTURES[index].len() as u16);
}
// Mathematical i128 oracle: no production XOR overflow/AF formula.
fn arithmetic(a:u32,b:u32,carry:bool,sub:bool,initial:u16)->(u32,u16) {
    let modulus = 1i128 << 32;
    let midpoint = modulus/2;
    let a = i128::from(a); let b = i128::from(b); let c = i128::from(carry);
    let signed = |v| if v >= midpoint {v-modulus} else {v};
    let (wide,sign,nibble) = if sub {
        (a-b-c,signed(a)-signed(b)-c,a%16-b%16-c)
    } else {
        (a+b+c,signed(a)+signed(b)+c,a%16+b%16+c)
    };
    let result = wide.rem_euclid(modulus);
    let mut ones=0; let mut low=result%256;
    for _ in 0..8 { ones+=low%2; low/=2; }
    let mut flags=initial & !Flags::ARITHMETIC;
    for (bit,value) in [
        (Flags::CF,wide<0 || wide>=modulus), (Flags::OF,sign< -midpoint || sign>=midpoint),
        (Flags::AF,!(0..16).contains(&nibble)), (Flags::PF,ones%2==0),
        (Flags::ZF,result==0), (Flags::SF,result>=midpoint),
    ] { if value {flags|=bit;} }
    (result as u32,flags)
}
fn zsp(value:u32)->u16 {
    let mut bits=0;
    if value==0 {bits|=Flags::ZF;}
    if value>=0x8000_0000 {bits|=Flags::SF;}
    let mut ones=0; for n in 0..8 {ones+=(value>>n)&1;}
    if ones%2==0 {bits|=Flags::PF;}
    bits
}
fn check_arithmetic(m:&mut Machine,a:u32,b:u32,initial:u16) {
    for index in [ADD,ADC,SUB,SBB,CMP,AND,OR,XOR,TEST,INC,DEC,NEG,NOT,ADD_IMM,XOR_SELF] {
        m.set32(0,a); m.set32(3,b); m.regs.flags=Flags::from_bits(initial);
        let (value,flags)=match index {
            ADD|ADC|SUB|SBB|CMP => arithmetic(a,b,matches!(index,ADC|SBB) && initial&1!=0,matches!(index,SUB|SBB|CMP),initial),
            INC|DEC => {let (v,f)=arithmetic(a,1,false,index==DEC,initial);(v,(f&!1)|(initial&1))},
            NEG => arithmetic(0,a,false,true,initial),
            NOT => (!a,initial),
            ADD_IMM => arithmetic(a,u32::MAX,false,false,initial),
            _ => {
                let value=match index { AND|TEST=>a&b, OR=>a|b, XOR=>a^b, _=>0 };
                (value,(initial & !(Flags::CF|Flags::OF|Flags::SF|Flags::ZF|Flags::PF)) | zsp(value))
            }
        };
        run(m,index);
        assert_eq!(m.get32(0),if matches!(index,CMP|TEST) {a} else {value},"op={index} a={a:x} b={b:x}");
        assert_eq!(m.get32(3),b);
        assert_eq!(m.regs.flags.bits(),flags,"flags op={index} a={a:x} b={b:x} initial={initial:x}");
    }
}
// Noniterative wide-ring oracle for rotates; direct powers of two for shifts.
fn shifted(op:usize,value:u32,count:u32,initial:u16)->(u32,u16) {
    let n=count&31;
    if n==0 {return (value,initial);}
    let v=u64::from(value); let cf=u64::from(initial&1);
    let (result,carry)=match op {
        ROL => { let r=((v<<n)|(v>>(32-n)))&0xffff_ffff;(r,r&1!=0) },
        ROR => { let r=((v>>n)|(v<<(32-n)))&0xffff_ffff;(r,r&0x8000_0000!=0) },
        RCL => { let ring=(v<<1)|cf; let r=((ring<<n)|(ring>>(33-n)))&0x1_ffff_ffff;(r>>1,r&1!=0) },
        RCR => { let ring=(cf<<32)|v; let r=((ring>>n)|(ring<<(33-n)))&0x1_ffff_ffff;(r&0xffff_ffff,r>>32!=0) },
        SHL => ((v<<n)&0xffff_ffff,(v>>(32-n))&1!=0),
        SHR => (v>>n,(v>>(n-1))&1!=0),
        SAR => {
            let signed=if v>=0x8000_0000 {i128::from(v)-(1i128<<32)} else {i128::from(v)};
            (signed.div_euclid(1i128<<n) as u32 as u64,(v>>(n-1))&1!=0)
        },
        _=>unreachable!(),
    };
    let result=result as u32;
    let mut flags=initial & !Flags::CF;
    if carry {flags|=Flags::CF;}
    if matches!(op,SHL|SHR|SAR) {flags=(flags&!(Flags::ZF|Flags::SF|Flags::PF))|zsp(result);}
    if n==1 {
        let overflow=match op {
            SHL|ROL|RCL => (result>=0x8000_0000)!=carry,
            SHR => value>=0x8000_0000,
            SAR => false,
            _ => (result>=0x8000_0000)!=((result/0x4000_0000)%2!=0),
        };
        flags&=!Flags::OF; if overflow {flags|=Flags::OF;}
    }
    (result,flags)
}
fn main() {
    let mut m=machine();
    let edges=[0,1,2,15,16,127,128,255,256,0xffff,0x10000,0x7fff_ffff,0x8000_0000,0x8000_0001,0xffff_fffe,u32::MAX];
    for a in edges {for b in edges {for flags in [0,0xffff,0xa555,0x5aaa] {check_arithmetic(&mut m,a,b,flags);}}}
    let mut random=0x97a1_3852u32;
    for _ in 0..4096 {
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let a=random;
        random=random.wrapping_mul(1664525).wrapping_add(1013904223);let b=random;
        check_arithmetic(&mut m,a,b,(random>>16) as u16);
    }
    for op in [ROL,ROR,RCL,RCR,SHL,SHR,SAR] {
        for value in edges {for count in 0..=255 {for initial in [0,0xffff] {
            m.set32(0,value);m.regs.cx=count as u16;m.regs.flags=Flags::from_bits(initial);
            let expected=shifted(op,value,count,initial);run(&mut m,op);
            assert_eq!((m.get32(0),m.regs.flags.bits()),expected,"shift op={op} v={value:x} c={count}");
        }}}
    }
    for (op,one,mem) in [(ROL,ROL_ONE,ROL_MEM),(ROR,ROR_ONE,ROR_MEM),(RCL,RCL_ONE,RCL_MEM),(RCR,RCR_ONE,RCR_MEM),(SHL,SHL_ONE,SHL_MEM),(SHR,SHR_ONE,SHR_MEM),(SAR,SAR_ONE,SAR_MEM)] {
        for value in edges {
            m.set32(0,value);m.regs.flags=Flags::from_bits(0xffff);run(&mut m,one);
            assert_eq!((m.get32(0),m.regs.flags.bits()),shifted(op,value,1,0xffff));
            m.regs.bx=0x201;m.write32(SegmentedAddress::new(m.regs.ds,0x201),value).unwrap();
            m.regs.flags=Flags::from_bits(0xffff);run(&mut m,mem);
            assert_eq!((m.read32(SegmentedAddress::new(m.regs.ds,0x201)).unwrap(),m.regs.flags.bits()),shifted(op,value,1,0xffff));
        }
    }
    let movs=[MOV_R0,MOV_R1,MOV_R2,MOV_R3,MOV_R4,MOV_R5,MOV_R6,MOV_R7];
    let pushes=[PUSH_R0,PUSH_R1,PUSH_R2,PUSH_R3,PUSH_R4,PUSH_R5,PUSH_R6,PUSH_R7];
    let pops=[POP_R0,POP_R1,POP_R2,POP_R3,POP_R4,POP_R5,POP_R6,POP_R7];
    for register in 0..8u8 {
        m.regs.flags=Flags::from_bits(0xa55a);run(&mut m,movs[register as usize]);
        assert_eq!(m.get32(register),0x89ab_cdef);assert_eq!(m.regs.flags.bits(),0xa55a);
        m.set32(4,0xa5a5_fffc);m.set32(register,0x89ab_cdef);
        let value=m.get32(register);let old_sp=m.regs.sp;
        run(&mut m,pushes[register as usize]);
        assert_eq!(m.regs.sp,old_sp.wrapping_sub(4));
        assert_eq!(m.get32(4)>>16,if register==4 {0x89ab} else {0xa5a5});
        assert_eq!(m.read32(SegmentedAddress::new(m.regs.ss,m.regs.sp)).unwrap(),value);
        run(&mut m,pops[register as usize]);assert_eq!(m.get32(register),value);assert_eq!(m.regs.sp,old_sp);
    }
    m.set32(4,0x1234_0000);run(&mut m,PUSH_ESP);
    assert_eq!(m.get32(4),0x1234_fffc);assert_eq!(m.read32(SegmentedAddress::new(m.regs.ss,0xfffc)).unwrap(),0x1234_0000);
    run(&mut m,POP_ESP);assert_eq!(m.get32(4),0x1234_0000);
    run(&mut m,PUSH_IMM);assert_eq!(m.read32(SegmentedAddress::new(m.regs.ss,0xfffc)).unwrap(),u32::MAX);
    m.regs.bx=0x201;run(&mut m,POP_MEM);assert_eq!(m.read32(SegmentedAddress::new(m.regs.ds,0x201)).unwrap(),u32::MAX);
    run(&mut m,PUSH_MEM);run(&mut m,POP_R0);assert_eq!(m.get32(0),u32::MAX);
    for value in [0,0x7fff,0x8000,0xffff] {
        m.set32(0,0x1234_0000|value);run(&mut m,CWDE);assert_eq!(m.get32(0),value as u16 as i16 as i32 as u32);
        run(&mut m,CDQ);assert_eq!(m.get32(2),if value>=0x8000 {u32::MAX} else {0});
    }
    // EA capture, 16-bit wrapping and default SS/ES override.
    m.set32(3,0xcafe_fff0);m.set32(6,0xdead_0031);
    let ea=SegmentedAddress::new(m.regs.ds,0x21);
    run(&mut m,MOV_MEM);assert_eq!(m.read32(ea).unwrap(),0xdead_0031);
    m.write32(ea,0x1357_0203).unwrap();run(&mut m,XCHG_MEM);
    assert_eq!(m.get32(6),0x1357_0203);assert_eq!(m.read32(ea).unwrap(),0xdead_0031);
    m.set32(6,0x1111_0031);run(&mut m,MOV_LOAD);assert_eq!(m.get32(6),0xdead_0031);
    m.set32(6,0x1111_0031);run(&mut m,LEA);assert_eq!(m.get32(6),0x21);
    m.regs.bp=0xfff0;m.regs.si=0x31;
    m.write32(SegmentedAddress::new(m.regs.ss,0x1f),0x3456_789a).unwrap();
    m.write32(SegmentedAddress::new(m.regs.es,0x1f),0x7654_3210).unwrap();
    run(&mut m,MOV_SS);assert_eq!(m.get32(0),0x3456_789a);
    run(&mut m,MOV_ES);assert_eq!(m.get32(0),0x7654_3210);
    m.write32(SegmentedAddress::new(m.regs.ds,0x201),0x0102_0304).unwrap();
    run(&mut m,MOV_MOFFS);assert_eq!(m.get32(0),0x0102_0304);
    m.regs.bx=0x201;run(&mut m,MOV_STORE_IMM);
    assert_eq!(m.read32(SegmentedAddress::new(m.regs.ds,0x201)).unwrap(),0x89ab_cdef);
    m.set32(0,0x7654_3211);m.regs.flags=Flags::from_bits(0xffff);run(&mut m,ADD_MEM);
    assert_eq!(m.read32(SegmentedAddress::new(m.regs.ds,0x201)).unwrap(),0);
    assert_eq!(m.regs.flags.bits(),arithmetic(0x89ab_cdef,0x7654_3211,false,false,0xffff).1);
    // Operand-size is not address-size: CX/SI/DI alone change during REP.
    m.set32(0,0x89ab_cdef);m.set32(1,0xcafe_0082);m.set32(6,0x1357_0030);m.set32(7,0x2468_0020);
    m.regs.flags=Flags::from_bits(0xa155);prepare(&mut m,REP_STOS);let steps=m.steps;
    step(&mut m,REP_STOS).unwrap();assert_eq!((m.regs.cx,m.regs.di,m.regs.ip,m.steps-steps),(66,0x120,0x100,64));
    step(&mut m,REP_STOS).unwrap();step(&mut m,REP_STOS).unwrap();
    assert_eq!((m.get32(1),m.get32(7),m.get32(6),m.steps-steps),(0xcafe_0000,0x2468_0228,0x1357_0030,130));
    assert_eq!(m.regs.flags.bits(),0xa155);assert_eq!(m.regs.ip,0x103);
    for n in 0..130u16 {assert_eq!(m.read32(SegmentedAddress::new(m.regs.es,0x20+n*4)).unwrap(),0x89ab_cdef);}
    for index in [REP_STOS,REP_MOVS,REP_LODS,REPE_CMPS,REPNE_CMPS,REPE_SCAS,REPNE_SCAS] {
        m.set32(1,0xbeef_0000);m.regs.si=0xffff;m.regs.di=0xffff;let before=m.regs;
        run(&mut m,index);assert_eq!((m.regs.si,m.regs.di,m.regs.flags),(before.si,before.di,before.flags));
    }
    m.regs.flags=Flags::from_bits(Flags::DF);m.regs.di=0;m.regs.cx=2;
    prepare(&mut m,REP_STOS);step(&mut m,REP_STOS).unwrap();
    assert_eq!(m.regs.di,0xfff8);assert_eq!(m.read32(SegmentedAddress::new(m.regs.es,0xfffc)).unwrap(),0x89ab_cdef);
    m.regs.flags=Flags::from_bits(0);m.regs.si=0xfffc;m.regs.di=0xfffc;m.regs.cx=2;
    m.write32(SegmentedAddress::new(m.regs.ds,0xfffc),0x1234_5678).unwrap();
    m.write32(SegmentedAddress::new(m.regs.ds,0),0x8765_4321).unwrap();
    prepare(&mut m,REP_MOVS);step(&mut m,REP_MOVS).unwrap();
    assert_eq!((m.regs.si,m.regs.di),(4,4));assert_eq!(m.read32(SegmentedAddress::new(m.regs.es,0)).unwrap(),0x8765_4321);
    m.regs.si=0xfffc;m.regs.cx=2;prepare(&mut m,REP_LODS);step(&mut m,REP_LODS).unwrap();assert_eq!(m.get32(0),0x8765_4321);
    for (index,src,dst,expected_zf) in [
        (REPE_CMPS,[1,2,3],[1,9,3],false),(REPNE_CMPS,[1,2,3],[9,2,3],true),
        (REPE_SCAS,[2,2,2],[2,9,2],false),(REPNE_SCAS,[2,2,2],[9,2,2],true),
    ] {
        m.set32(0,2);m.regs.si=0x100;m.regs.di=0x200;m.regs.cx=3;
        for n in 0..3 {m.write32(SegmentedAddress::new(m.regs.ds,0x100+n*4),src[n as usize]).unwrap();m.write32(SegmentedAddress::new(m.regs.es,0x200+n*4),dst[n as usize]).unwrap();}
        prepare(&mut m,index);let steps=m.steps;step(&mut m,index).unwrap();
        assert_eq!((m.regs.cx,m.regs.di,m.steps-steps,m.regs.flags.zf()),(1,0x208,2,expected_zf));
        assert_eq!(m.regs.si,if matches!(index,REPE_CMPS|REPNE_CMPS) {0x108} else {0x100});
        assert_eq!(m.regs.ip,0x103);
    }
    for index in [MOVS,STOS,LODS,CMPS,SCAS,LODS_ES] {
        m.regs.cx=7;m.regs.si=0x100;m.regs.di=0x200;m.set32(0,0xaabb_ccdd);
        m.write32(SegmentedAddress::new(m.regs.ds,0x100),0x8000_0000).unwrap();
        m.write32(SegmentedAddress::new(m.regs.es,0x100),0x7654_3210).unwrap();
        m.write32(SegmentedAddress::new(m.regs.es,0x200),0x7fff_ffff).unwrap();
        m.regs.flags=Flags::from_bits(0x202);run(&mut m,index);assert_eq!(m.regs.cx,7);
        match index {
            MOVS=>assert_eq!(m.read32(SegmentedAddress::new(m.regs.es,0x200)).unwrap(),0x8000_0000),
            STOS=>assert_eq!(m.read32(SegmentedAddress::new(m.regs.es,0x200)).unwrap(),0xaabb_ccdd),
            LODS=>assert_eq!(m.get32(0),0x8000_0000),
            LODS_ES=>assert_eq!(m.get32(0),0x7654_3210),
            CMPS=>assert_eq!(m.regs.flags.bits(),arithmetic(0x8000_0000,0x7fff_ffff,false,true,0x202).1),
            SCAS=>assert_eq!(m.regs.flags.bits(),arithmetic(0xaabb_ccdd,0x7fff_ffff,false,true,0x202).1),
            _=>unreachable!(),
        }
    }
    // Runtime boundary policy: no dword silently wraps inside one access.
    m.regs.di=0xfffd;m.regs.cx=1;prepare(&mut m,REP_STOS);let before=m.regs;
    assert!(step(&mut m,REP_STOS).unwrap_err().0.contains("segment boundary"));assert_eq!(m.regs,before);
    m.regs.bx=0xfffd;prepare(&mut m,ADD_MEM);let before=m.regs;
    assert!(step(&mut m,ADD_MEM).is_err());assert_eq!(m.regs,before);
    m.regs.sp=1;prepare(&mut m,PUSH_IMM);let before=m.regs;
    assert!(step(&mut m,PUSH_IMM).is_err());assert_eq!(m.regs,before);
    // Existing byte guard must run before either a register write or charging.
    prepare(&mut m,XOR_SELF);m.memory.write8(SegmentedAddress::new(m.regs.cs,0x100),0x90).unwrap();let before=m.regs;let steps=m.steps;
    assert!(step(&mut m,XOR_SELF).unwrap_err().0.contains("code changed"));assert_eq!(m.regs,before);assert_eq!(m.steps,steps);
    check_double_shifts(&mut m, &edges);
    println!("dword AOT fixtures passed: {} encodings, {} execution steps; flags checked against independent wide/ring oracles; no decoder linked",FIXTURES.len(),m.steps);
}
"#;

    const DOUBLE_SHIFT_TESTS: &str = r#"
// Independent 64-bit concatenation oracle, using division/multiplication
// rather than the production split shifts and OR expression.
fn double_shift_oracle(left:bool,a:u32,b:u32,count:u8,initial:u16)->(u32,u16) {
    let n=u32::from(count)%32;
    if n==0 { return (a,initial); }
    let base=1u128<<32; let power=1u128<<n;
    let wide=if left {u128::from(a)*base+u128::from(b)} else {u128::from(b)*base+u128::from(a)};
    let (result,carry)=if left {
        (((wide*power)/base)%base,(wide/(1u128<<(64-n)))%2!=0)
    } else {
        ((wide/power)%base,(wide/(power/2))%2!=0)
    };
    let value=result as u32;
    let mut flags=(initial & !(Flags::CF|Flags::ZF|Flags::SF|Flags::PF))|zsp(value);
    if carry {flags|=Flags::CF;}
    if n==1 {
        flags&=!Flags::OF;
        if (a>=0x8000_0000)!=(value>=0x8000_0000) {flags|=Flags::OF;}
    }
    (value,flags)
}
fn double_shift_case(m:&mut Machine,index:usize,left:bool,kind:u8,a:u32,b:u32,count:u8,initial:u16,host:&mut Vec<u8>) {
    // Upper halves and all unrelated GPRs are checked, not only the result.
    for r in 0..8 {m.set32(r,0x9137_2468+u32::from(r)*0x10000);}
    m.set32(0,a);m.set32(3,b);m.set32(1,0xbeef_a500|u32::from(count));
    let ea=match kind {
        4 => {
            m.set32(6,b);m.set32(3,0xcafe_0000|u32::from(0x201u16.wrapping_sub(m.regs.si)));
            SegmentedAddress::new(m.regs.ds,m.regs.bx.wrapping_add(m.regs.si))
        },
        5|6 => {
            m.set32(5,b);m.set32(6,0xdead_0000|u32::from(0x201u16.wrapping_sub(m.regs.bp)));
            SegmentedAddress::new(if kind==5 {m.regs.ss} else {m.regs.es},m.regs.bp.wrapping_add(m.regs.si))
        },
        _ => SegmentedAddress::new(m.regs.ds,0x200),
    };
    let dest=if matches!(kind,2|7) {1} else {0};
    let original=if kind>=4 && kind<=6 {a} else {m.get32(dest)};
    let source=match kind {1=>m.get32(0),3|7=>m.get32(1),4=>m.get32(6),5|6=>m.get32(5),_=>m.get32(3)};
    if (4..=6).contains(&kind) {m.write32(ea,a).unwrap();}
    let guard=SegmentedAddress::new(ea.segment,ea.offset.wrapping_sub(1));
    m.memory.write8(guard,0x5a).unwrap();
    let after=SegmentedAddress::new(ea.segment,ea.offset.wrapping_add(4));
    m.memory.write8(after,0xa5).unwrap();
    let before:Vec<u32>=(0..8).map(|r|m.get32(r)).collect();
    m.regs.flags=Flags::from_bits(initial);
    let expected=double_shift_oracle(left,original,source,count,initial);
    run(m,index);
    let value=if (4..=6).contains(&kind) {m.read32(ea).unwrap()} else {m.get32(dest)};
    assert_eq!((value,m.regs.flags.bits()),expected,"double shift index={index} left={left} kind={kind} a={original:x} b={source:x} count={count} flags={initial:x}");
    for r in 0..8 {assert_eq!(m.get32(r),if r==dest && !(4..=6).contains(&kind) {expected.0} else {before[r as usize]},"unrelated GPR {r}, fixture {index}");}
    assert_eq!(m.memory.read8(guard).unwrap(),0x5a);
    assert_eq!(m.memory.read8(after).unwrap(),0xa5);
    for v in [u32::from(left),original,source,u32::from(count),u32::from(initial),value,u32::from(m.regs.flags.bits())] {host.extend_from_slice(&v.to_le_bytes());}
}
fn check_double_shifts(m:&mut Machine,edges:&[u32]) {
    let mut requests=Vec::new();
    let mut cases=0usize;
    for &(index,left,immediate,kind) in DOUBLE_SHIFTS {
        let counts:Vec<u8>=match immediate {Some(n)=>vec![n],None=>(0..=255).collect()};
        for count in counts {
            for &a in edges {for &b in edges {for initial in [0,0xffff,0xa555,0x5aaa] {
                double_shift_case(m,index,left,kind,a,b,count,initial,&mut requests);
                cases+=1;
            }}}
        }
    }
    // Random pairs for each CL form and every count, with varied flag bits.
    let mut random=0x736a_2491u32;
    for &(index,left,immediate,kind) in DOUBLE_SHIFTS {
        if immediate.is_some() {continue;}
        for count in 0..=255 {for _ in 0..16 {
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);let a=random;
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);let b=random;
            double_shift_case(m,index,left,kind,a,b,count,(random>>16) as u16,&mut requests);
            cases+=1;
        }}
    }
    // Nonzero dword access across segment end faults before flags/dest/IP
    // updates; mask-zero leaves flags and memory unchanged at a valid EA.
    for &(index,_,immediate,kind) in DOUBLE_SHIFTS {
        if immediate!=Some(1) || kind!=4 {continue;}
        m.set32(3,0xfffd);m.set32(6,0);prepare(m,index);let before=m.regs;
        assert!(step(m,index).unwrap_err().0.contains("segment boundary"));
        assert_eq!(m.regs,before);
    }
    println!("SHLD/SHRD independent u128 oracle: {cases} AOT executions passed (all imm8/CL counts, self/count/EA aliases)");
    #[cfg(target_arch="x86_64")]
    {
        let dir=std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let path=dir.join("host-requests.bin");std::fs::write(&path,requests).unwrap();
        let result=std::process::Command::new(dir.join("host")).arg(path).output().unwrap();
        assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
        print!("{}",String::from_utf8_lossy(&result.stdout));
    }
}
"#;

    const HOST_DOUBLE_SHIFT_ORACLE: &str = r#"
use std::arch::asm;
fn host(left:bool,memory:bool,a:u32,b:u32,count:u8,initial:u16)->(u32,u16) {
    let mut value=a;let flags:u64;
    // Only arithmetic status bits are seeded: never set TF/DF/privileged bits.
    // Restore the host flags before leaving the asm block.
    let seed=0x202u64|u64::from(initial&0x8d5);
    unsafe {
        match (left,memory) {
            (true,false)=>asm!("pushfq","pop r11","push {seed}","popfq","shld {value:e}, {source:e}, cl","pushfq","pop {flags}","push r11","popfq",value=inout(reg)value,source=in(reg)b,in("cl")count,seed=in(reg)seed,flags=lateout(reg)flags,out("r11")_),
            (false,false)=>asm!("pushfq","pop r11","push {seed}","popfq","shrd {value:e}, {source:e}, cl","pushfq","pop {flags}","push r11","popfq",value=inout(reg)value,source=in(reg)b,in("cl")count,seed=in(reg)seed,flags=lateout(reg)flags,out("r11")_),
            (true,true)=>asm!("pushfq","pop r11","push {seed}","popfq","shld dword ptr [{ptr}], {source:e}, cl","pushfq","pop {flags}","push r11","popfq",ptr=in(reg)&mut value,source=in(reg)b,in("cl")count,seed=in(reg)seed,flags=lateout(reg)flags,out("r11")_),
            (false,true)=>asm!("pushfq","pop r11","push {seed}","popfq","shrd dword ptr [{ptr}], {source:e}, cl","pushfq","pop {flags}","push r11","popfq",ptr=in(reg)&mut value,source=in(reg)b,in("cl")count,seed=in(reg)seed,flags=lateout(reg)flags,out("r11")_),
        }
    }
    (value,flags as u16)
}
fn main() {
    let bytes=std::fs::read(std::env::args_os().nth(1).unwrap()).unwrap();
    assert_eq!(bytes.len()%28,0);
    let mut comparisons=0usize;
    for record in bytes.chunks_exact(28) {
        let r:Vec<u32>=record.chunks_exact(4).map(|b|u32::from_le_bytes(b.try_into().unwrap())).collect();
        let n=r[3]&31;
        // AF is undefined for nonzero counts, OF for counts >1. The AOT's
        // chosen preservation is tested by the software oracle, not hardware.
        let mask=if n==0 {0x8d5} else if n==1 {0x8c5} else {0xc5};
        for memory in [false,true] {
            let actual=host(r[0]!=0,memory,r[1],r[2],r[3] as u8,r[4] as u16);
            assert_eq!((actual.0,u32::from(actual.1)&mask),(r[5],r[6]&mask),"host left={} memory={memory} a={:x} b={:x} count={} initial={:x}",r[0],r[1],r[2],r[3],r[4]);
            comparisons+=1;
        }
    }
    println!("SHLD/SHRD host x86_64 differential: {comparisons} register/memory comparisons passed (defined flags only)");
}
"#;
}
