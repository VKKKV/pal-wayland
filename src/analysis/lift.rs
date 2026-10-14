// SPDX-License-Identifier: GPL-3.0-or-later
//! Reachability-only 16-bit analysis and ahead-of-time Rust emission.
//!
//! Decode happens ONLY here, in the analysis tool. Emitted code has no decoder,
//! opcode dispatch, or interpreter fallback. Dynamic transfers can only reach
//! already generated CS:IP cases. Input is relocated, physical-zero-based memory;
//! code fetch never aliases through A20 or across a 64-KiB segment boundary.
//! The supported integer model uses 286+ masked shift counts and PUSH SP values.
//! Undefined arithmetic flags have no promised value. Unaffected flags are preserved.
//! Faults are terminal; multi-access instructions need not be restartable after
//! a fault. All generated writes (including stack writes) use Machine methods;
//! stale code is rejected before execution, not necessarily at write time.

#![deny(unreachable_patterns, unused_variables, non_snake_case)]

use iced_x86::{
    Decoder, DecoderError, DecoderOptions, FlowControl, Formatter, Instruction, IntelFormatter,
    Mnemonic, OpKind, Register,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub cs: u16,
    pub ip: u16,
}
impl Address {
    pub fn physical(self) -> usize {
        usize::from(self.cs) * 16 + usize::from(self.ip)
    }
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.cs, self.ip)
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub address: Address,
    pub physical: usize,
    pub bytes: Vec<u8>,
    /// Decoded IR is analysis-only. Invalid instructions have a diagnostic.
    pub instruction: Instruction,
    pub text: String,
    pub successors: Vec<Address>,
    pub diagnostic: Option<String>,
    pub borland: Option<crate::fpu_macro::DecodedMacro>,
    pub mutable_immediate: Option<crate::mutable_immediate::ImmediateSlot>,
}
#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<Address, Node>,
    pub unresolved: Vec<String>,
}

fn direct(i: &Instruction, cs: u16) -> Option<Address> {
    match i.op0_kind() {
        OpKind::NearBranch16 => Some(Address {
            cs,
            ip: i.near_branch16(),
        }),
        OpKind::FarBranch16 => Some(Address {
            cs: i.far_branch_selector(),
            ip: i.far_branch16(),
        }),
        _ => None,
    }
}

/// Follow direct branches, both conditional edges, and call continuations.
/// Returns partial graphs with explicit diagnostics at decode/budget frontiers.
/// Distinct reachable entries may overlap (e.g. a CMP immediate hiding MOV).
/// Preserve both CS:IP interpretations, report overlaps, and guard each complete
/// instruction separately. No branch is inferred solely from overlapping data.
pub fn recover(memory: &[u8], seeds: &[Address], max_instructions: usize) -> Result<Graph, String> {
    recover_in_ranges(
        memory,
        seeds,
        max_instructions,
        std::slice::from_ref(&(0..memory.len().min(0x100000))),
    )
}

/// Restrict decoding to caller-confirmed physical code/image ranges. No target
/// in PSP, zero-filled RAM, or other excluded memory is guessed to be code.
/// Ranges must be nonempty, disjoint, and within the supplied real-mode memory.
pub fn recover_in_ranges(
    memory: &[u8],
    seeds: &[Address],
    max_instructions: usize,
    ranges: &[std::ops::Range<usize>],
) -> Result<Graph, String> {
    recover_configured(memory, seeds, max_instructions, ranges, false)
}

/// Opt-in recognition for inputs proven to use Borland inline FPU conventions.
pub fn recover_borland_in_ranges(
    memory: &[u8],
    seeds: &[Address],
    max: usize,
    ranges: &[std::ops::Range<usize>],
) -> Result<Graph, String> {
    recover_configured(memory, seeds, max, ranges, true)
}
fn recover_configured(
    memory: &[u8],
    seeds: &[Address],
    max_instructions: usize,
    ranges: &[std::ops::Range<usize>],
    borland_enabled: bool,
) -> Result<Graph, String> {
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|r| r.start);
    if ranges.is_empty()
        || ranges
            .iter()
            .any(|r| r.is_empty() || r.end > memory.len().min(0x100000))
        || ranges.windows(2).any(|w| w[0].end > w[1].start)
    {
        return Err("code ranges must be nonempty, disjoint, and inside real-mode memory".into());
    }
    if seeds.is_empty() {
        return Err("at least one confirmed code seed is required".into());
    }
    if max_instructions == 0 {
        return Err("instruction budget must be nonzero".into());
    }
    let mut graph = Graph::default();
    let mut pending = VecDeque::from(seeds.to_vec());
    let mut seen = BTreeSet::new();
    let mut owners = BTreeMap::<usize, Address>::new();
    let mut formatter = IntelFormatter::new();
    while let Some(at) = pending.pop_front() {
        if !seen.insert(at) {
            continue;
        }
        if graph.nodes.len() >= max_instructions {
            graph.unresolved.push(format!(
                "{at}: instruction budget {max_instructions} exhausted; pending targets not decoded"
            ));
            break;
        }
        let physical = at.physical();
        if physical >= memory.len() || physical >= 0x100000 {
            graph.unresolved.push(format!(
                "{at}: physical {physical:#x} outside supplied real-mode memory"
            ));
            continue;
        }
        let Some(range) = ranges.iter().find(|r| r.contains(&physical)) else {
            graph.unresolved.push(format!(
                "{at}: physical {physical:#x} outside confirmed code ranges"
            ));
            continue;
        };
        let available = (range.end - physical)
            .min(0x100000 - physical)
            .min(0x10000 - usize::from(at.ip))
            .min(15);
        let mut decoder = Decoder::with_ip(
            16,
            &memory[physical..physical + available],
            u64::from(at.ip),
            DecoderOptions::NONE,
        );
        let mut instruction = decoder.decode();
        let mut len = instruction.len().max(1).min(available);
        let borland_result = if borland_enabled {
            crate::fpu_macro::decode(&memory[physical..physical + available], at.ip)
        } else {
            Ok(None)
        };
        let (borland, macro_error) = match borland_result {
            Ok(value) => (value, None),
            Err(error) => (None, Some(error)),
        };
        if let Some(ref decoded) = borland {
            len = decoded.len;
            if let Some(body) = decoded.instruction {
                instruction = body;
            }
        }
        let bytes = memory[physical..physical + len].to_vec();
        for p in physical..physical + len {
            if let Some(other) = owners.insert(p, at) {
                graph.unresolved.push(format!(
                    "overlapping code at physical {p:#x}: {at} and {other}; both guarded interpretations retained"
                ));
            }
        }
        let mut diagnostic = if instruction.is_invalid() {
            Some(match decoder.last_error() {
                DecoderError::NoMoreBytes if available == 0x10000 - usize::from(at.ip) => {
                    "instruction crosses 64-KiB code segment boundary".into()
                }
                DecoderError::NoMoreBytes => {
                    "truncated instruction at memory/code-range boundary".into()
                }
                _ => "invalid instruction encoding".into(),
            })
        } else if borland.is_none()
            && instruction.mnemonic() == Mnemonic::Int
            && (0x34..=0x3d).contains(&instruction.immediate8())
        {
            Some("unsupported Borland software FPU interrupt; following bytes may be inline operands".into())
        } else {
            None
        };
        if let Some(error) = macro_error {
            diagnostic = Some(error);
        }
        let mut text = String::new();
        formatter.format(&instruction, &mut text);
        let mut successors = Vec::new();
        let next = Address {
            cs: at.cs,
            ip: at.ip.wrapping_add(len as u16),
        };
        if diagnostic.is_none() {
            match instruction.flow_control() {
                FlowControl::Next | FlowControl::Interrupt => successors.push(next),
                FlowControl::UnconditionalBranch
                | FlowControl::ConditionalBranch
                | FlowControl::Call => {
                    if let Some(target) = direct(&instruction, at.cs) {
                        successors.push(target);
                    } else {
                        diagnostic = Some("unsupported non-16-bit direct control transfer".into());
                    }
                    if matches!(
                        instruction.flow_control(),
                        FlowControl::ConditionalBranch | FlowControl::Call
                    ) {
                        successors.push(next);
                    }
                }
                FlowControl::IndirectCall => {
                    graph.unresolved.push(format!(
                        "{at}: indirect call target requires a confirmed seed ({text})"
                    ));
                    successors.push(next);
                }
                FlowControl::IndirectBranch | FlowControl::Return => {
                    graph
                        .unresolved
                        .push(format!("{at}: dynamic target not followed ({text})"));
                }
                _ => graph
                    .unresolved
                    .push(format!("{at}: control flow stopped ({text})")),
            }
        }
        if let Some(reason) = &diagnostic {
            successors.clear();
            graph.unresolved.push(format!("{at}: {reason}"));
        }
        pending.extend(successors.iter().copied());
        graph.nodes.insert(
            at,
            Node {
                address: at,
                physical,
                bytes,
                instruction,
                text,
                successors,
                diagnostic,
                borland,
                mutable_immediate: None,
            },
        );
    }
    Ok(graph)
}

fn reg(r: Register) -> Result<(&'static str, u32, u32), String> {
    use Register::*;
    Ok(match r {
        AX => ("ax", 16, 0),
        BX => ("bx", 16, 0),
        CX => ("cx", 16, 0),
        DX => ("dx", 16, 0),
        SP => ("sp", 16, 0),
        BP => ("bp", 16, 0),
        SI => ("si", 16, 0),
        DI => ("di", 16, 0),
        CS => ("cs", 16, 0),
        DS => ("ds", 16, 0),
        ES => ("es", 16, 0),
        SS => ("ss", 16, 0),
        AL => ("ax", 8, 0),
        AH => ("ax", 8, 8),
        BL => ("bx", 8, 0),
        BH => ("bx", 8, 8),
        CL => ("cx", 8, 0),
        CH => ("cx", 8, 8),
        DL => ("dx", 8, 0),
        DH => ("dx", 8, 8),
        _ => return Err(format!("unsupported register {r:?}")),
    })
}
fn read_reg(r: Register) -> Result<String, String> {
    let (name, bits, shift) = reg(r)?;
    Ok(if bits == 16 {
        format!("m.regs.{name}")
    } else {
        format!("((m.regs.{name} >> {shift}) as u8)")
    })
}
fn write_reg(r: Register, value: &str) -> Result<String, String> {
    let (name, bits, shift) = reg(r)?;
    if r == Register::CS {
        return Err("writing CS with a data instruction is unsupported".into());
    }
    Ok(if bits == 16 {
        format!("m.regs.{name} = {value};\n")
    } else {
        format!(
            "m.regs.{name} = (m.regs.{name} & {:#06x}) | (u16::from({value}) << {shift});\n",
            !(0xffu16 << shift)
        )
    })
}
fn offset(i: &Instruction) -> Result<String, String> {
    let mut expr = format!("{}u16", i.memory_displacement64() as u16);
    for r in [i.memory_base(), i.memory_index()] {
        if r != Register::None {
            if !matches!(r, Register::BX | Register::BP | Register::SI | Register::DI) {
                return Err(format!("unsupported address register {r:?}"));
            }
            expr = format!("({expr}).wrapping_add({})", read_reg(r)?);
        }
    }
    if i.memory_index_scale() != 1 {
        return Err("scaled address is not 16-bit".into());
    }
    Ok(expr)
}
fn memory_address(i: &Instruction) -> Result<String, String> {
    Ok(format!(
        "SegmentedAddress::new({}, {})",
        read_reg(i.memory_segment())?,
        offset(i)?
    ))
}
fn width(i: &Instruction, op: u32) -> Result<u32, String> {
    match i.op_kind(op) {
        OpKind::Register => Ok(reg(i.op_register(op))?.1),
        OpKind::Memory => match i.memory_size().size() {
            1 => Ok(8),
            2 => Ok(16),
            _ => Err("unsupported memory width".into()),
        },
        OpKind::Immediate8 => Ok(8),
        OpKind::Immediate16 | OpKind::Immediate8to16 => Ok(16),
        _ => Err(format!("unsupported operand {:?}", i.op_kind(op))),
    }
}
/// Memory EA is captured before any register modification (XCHG/LES/LDS).
fn bind(i: &Instruction, op: u32, name: &str, out: &mut String) -> Result<(), String> {
    if i.op_kind(op) == OpKind::Memory {
        writeln!(out, "let {name} = {};", memory_address(i)?).unwrap();
    }
    Ok(())
}
fn read_op(i: &Instruction, op: u32, name: &str) -> Result<String, String> {
    match i.op_kind(op) {
        OpKind::Register => read_reg(i.op_register(op)),
        OpKind::Memory => Ok(format!("m.read{}({name})?", width(i, op)?)),
        OpKind::Immediate8 => Ok(format!("{}u8", i.immediate8())),
        OpKind::Immediate16 => Ok(format!("{}u16", i.immediate16())),
        OpKind::Immediate8to16 => Ok(format!("{}u16", i.immediate8to16() as u16)),
        _ => Err(format!("unsupported operand {:?}", i.op_kind(op))),
    }
}
fn write_op(i: &Instruction, op: u32, name: &str, val: &str) -> Result<String, String> {
    match i.op_kind(op) {
        OpKind::Register => write_reg(i.op_register(op), val),
        OpKind::Memory => Ok(format!("m.write{}({name}, {val})?;\n", width(i, op)?)),
        _ => Err("operand is not writable".into()),
    }
}
fn zsp(bits: u32, val: &str) -> String {
    format!(
        "m.regs.flags.set(Flags::ZF, {val} == 0);\nm.regs.flags.set(Flags::SF, ({val} & (1u{bits} << {})) != 0);\nm.regs.flags.set(Flags::PF, ({val} as u8).count_ones().is_multiple_of(2));\n",
        bits - 1
    )
}
fn condition(m: Mnemonic) -> Option<&'static str> {
    use Mnemonic::*;
    Some(match m {
        Ja => "!m.regs.flags.cf() && !m.regs.flags.zf()",
        Jae => "!m.regs.flags.cf()",
        Jb => "m.regs.flags.cf()",
        Jbe => "m.regs.flags.cf() || m.regs.flags.zf()",
        Je => "m.regs.flags.zf()",
        Jne => "!m.regs.flags.zf()",
        Jg => "!m.regs.flags.zf() && m.regs.flags.sf() == m.regs.flags.of()",
        Jge => "m.regs.flags.sf() == m.regs.flags.of()",
        Jl => "m.regs.flags.sf() != m.regs.flags.of()",
        Jle => "m.regs.flags.zf() || m.regs.flags.sf() != m.regs.flags.of()",
        Jo => "m.regs.flags.of()",
        Jno => "!m.regs.flags.of()",
        Js => "m.regs.flags.sf()",
        Jns => "!m.regs.flags.sf()",
        Jp => "m.regs.flags.pf()",
        Jnp => "!m.regs.flags.pf()",
        Jcxz => "m.regs.cx == 0",
        Loop => "m.regs.cx != 0",
        Loope => "m.regs.cx != 0 && m.regs.flags.zf()",
        Loopne => "m.regs.cx != 0 && !m.regs.flags.zf()",
        _ => return None,
    })
}

fn string_body(n: &Node) -> Result<String, String> {
    use Mnemonic::*;
    let i = &n.instruction;
    let (bits, src, dst, cmp) = match i.mnemonic() {
        Movsb => (8, true, true, false),
        Movsw => (16, true, true, false),
        Cmpsb => (8, true, true, true),
        Cmpsw => (16, true, true, true),
        Stosb => (8, false, true, false),
        Stosw => (16, false, true, false),
        Lodsb => (8, true, false, false),
        Lodsw => (16, true, false, false),
        Scasb => (8, false, true, true),
        Scasw => (16, false, true, true),
        _ => return Err("unsupported string operation".into()),
    };
    let repeated = i.has_rep_prefix() || i.has_repne_prefix();
    let mut s = String::new();
    if repeated {
        // Bounded batches: at most 64 units per host call, each charged to steps.
        // The same static case resumes without fetching or decoding guest code.
        s.push_str(
            "for unit in 0..64 {\nif m.regs.cx == 0 { break; }\nif unit != 0 { m.steps += 1; }\n",
        );
    }
    if src {
        let segment = if i.segment_prefix() == Register::None {
            Register::DS
        } else {
            i.segment_prefix()
        };
        writeln!(
            s,
            "let source = SegmentedAddress::new({}, m.regs.si);",
            read_reg(segment)?
        )
        .unwrap();
    }
    if dst {
        s.push_str("let dest = SegmentedAddress::new(m.regs.es, m.regs.di);\n");
    }
    // Validators do not read device memory: a rejected unit must not load latches.
    if dst {
        writeln!(s, "m.validate_span(dest,{})?;", bits / 8).unwrap();
    }
    if src {
        writeln!(s, "m.validate_span(source,{})?;", bits / 8).unwrap();
    }
    let acc = if bits == 8 {
        "m.regs.ax as u8"
    } else {
        "m.regs.ax"
    };
    match i.mnemonic() {
        Movsb|Movsw=>writeln!(s,"let value = m.read{bits}(source)?; m.write{bits}(dest, value)?;").unwrap(),
        Stosb|Stosw=>writeln!(s,"m.write{bits}(dest, {acc})?;").unwrap(),
        Lodsb|Lodsw=>{
            writeln!(s,"let value = m.read{bits}(source)?;").unwrap();
            s.push_str(&write_reg(if bits==8 {Register::AL} else {Register::AX},"value")?);
        }
        Cmpsb|Cmpsw=>writeln!(s,"let a = m.read{bits}(source)?; let b = m.read{bits}(dest)?; cmp{bits}(a,b,&mut m.regs.flags);").unwrap(),
        Scasb|Scasw=>writeln!(s,"let a = {acc}; let b = m.read{bits}(dest)?; cmp{bits}(a,b,&mut m.regs.flags);").unwrap(),
        _=>unreachable!(),
    }
    writeln!(s,"let delta = if m.regs.flags.contains(Flags::DF) {{ 0u16.wrapping_sub({}) }} else {{ {} }};", bits/8,bits/8).unwrap();
    if src {
        s.push_str("m.regs.si = m.regs.si.wrapping_add(delta);\n");
    }
    if dst {
        s.push_str("m.regs.di = m.regs.di.wrapping_add(delta);\n");
    }
    if repeated {
        s.push_str("m.regs.cx = m.regs.cx.wrapping_sub(1);\n");
        if cmp {
            let stop = if i.has_repne_prefix() {
                "m.regs.flags.zf()"
            } else {
                "!m.regs.flags.zf()"
            };
            writeln!(
                s,
                "if {stop} {{ m.regs.ip = {}; return Ok(()); }}",
                n.address.ip.wrapping_add(i.len() as u16)
            )
            .unwrap();
        }
        s.push_str("}\nif m.regs.cx != 0 { return Ok(()); }\n");
    }
    Ok(s)
}

fn shift_body(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    let bits = width(i, 0)?;
    let mut s = String::new();
    bind(i, 0, "ea", &mut s)?;
    writeln!(
        s,
        "let mut value = {}; let masked = ({} as u32) & 31;",
        read_op(i, 0, "ea")?,
        read_op(i, 1, "unused")?
    )
    .unwrap();
    let through = matches!(i.mnemonic(), Rcl | Rcr);
    if through {
        writeln!(s, "let count = masked % {};", bits + 1).unwrap();
    } else {
        s.push_str("let count = masked;\n");
    }
    s.push_str("if count != 0 {\nlet original = value;\nfor _ in 0..count {\n");
    match i.mnemonic() {
        Shl | Sal | Rol | Rcl => {
            writeln!(s, "let high = value & (1u{bits} << {}) != 0;", bits - 1).unwrap();
            let low = match i.mnemonic() {
                Rol => "high",
                Rcl => "m.regs.flags.cf()",
                _ => "false",
            };
            writeln!(s,"value = value.wrapping_shl(1) | u{bits}::from({low}); m.regs.flags.set(Flags::CF, high);").unwrap();
        }
        Shr | Sar | Ror | Rcr => {
            s.push_str("let low = value & 1 != 0;\n");
            let high = match i.mnemonic() {
                Ror => "low".to_string(),
                Rcr => "m.regs.flags.cf()".to_string(),
                Sar => format!("value & (1u{bits} << {}) != 0", bits - 1),
                _ => "false".to_string(),
            };
            writeln!(s,"let high = {high}; value = (value >> 1) | (u{bits}::from(high) << {}); m.regs.flags.set(Flags::CF, low);",bits-1).unwrap();
        }
        _ => return Err("unsupported shift".into()),
    }
    s.push_str("}\n");
    if matches!(i.mnemonic(), Shl | Sal | Shr | Sar) {
        s.push_str(&zsp(bits, "value"));
    }
    let overflow = match i.mnemonic() {
        Shl | Sal | Rol | Rcl => format!(
            "(value & (1u{bits} << {}) != 0) != m.regs.flags.cf()",
            bits - 1
        ),
        Shr => format!("original & (1u{bits} << {}) != 0", bits - 1),
        Sar => "false".into(),
        _ => format!(
            "((value >> {}) ^ (value >> {})) & 1 != 0",
            bits - 1,
            bits - 2
        ),
    };
    writeln!(
        s,
        "if masked == 1 {{ m.regs.flags.set(Flags::OF, {overflow}); }}"
    )
    .unwrap();
    s.push_str(&write_op(i, 0, "ea", "value")?);
    s.push_str("}\n");
    Ok(s)
}

/// Word double shifts define results only for masked counts 0..=16.
/// Do not extrapolate a host CPU's behavior for 17..=31: Intel specifies the
/// destination and arithmetic flags as undefined. Reject before any EA access
/// or architectural mutation. Count zero still reads/checks a memory operand,
/// like the existing dword emitter, but never writes it or changes flags.
fn double_shift16_body(i: &Instruction) -> Result<String, String> {
    if i.op_count() != 3
        || width(i, 0)? != 16
        || i.op1_kind() != OpKind::Register
        || width(i, 1)? != 16
    {
        return Err("unsupported word double-shift form".into());
    }
    let count = match i.op2_kind() {
        OpKind::Immediate8 => format!("{}u32", i.immediate8()),
        OpKind::Register if i.op2_register() == Register::CL => "u32::from(m.regs.cx as u8)".into(),
        _ => return Err("unsupported word double-shift count".into()),
    };
    let mut s = String::new();
    writeln!(s, "let count = ({count}) & 31;").unwrap();
    s.push_str("if count > 16 { return Err(m.fault(\"unsupported undefined 16-bit SHLD/SHRD count > 16\")); }\n");
    // Snapshot EA, destination, source and CL before any aliased write.
    bind(i, 0, "dst", &mut s)?;
    writeln!(
        s,
        "let original = {}; let source = {};",
        read_op(i, 0, "dst")?,
        read_op(i, 1, "unused")?
    )
    .unwrap();
    s.push_str("if count != 0 {\n");
    // Widen before shifting: count 16 is valid and produces source exactly,
    // whereas shifting a u16 by 16 would panic in a checked host build.
    if i.mnemonic() == Mnemonic::Shld {
        s.push_str("let value = ((u32::from(original) << count) | (u32::from(source) >> (16 - count))) as u16;\nlet carry = (u32::from(original) >> (16 - count)) & 1 != 0;\n");
    } else {
        s.push_str("let value = ((u32::from(original) >> count) | (u32::from(source) << (16 - count))) as u16;\nlet carry = (u32::from(original) >> (count - 1)) & 1 != 0;\n");
    }
    // Write first so a memory fault cannot change arithmetic flags.
    s.push_str(&write_op(i, 0, "dst", "value")?);
    s.push_str("m.regs.flags.set(Flags::CF, carry);\n");
    s.push_str(&zsp(16, "value"));
    // AF is undefined; OF is undefined for count > 1. Preserve both rather
    // than claiming a CPU-specific value. All other flags remain untouched.
    s.push_str(
        "if count == 1 { m.regs.flags.set(Flags::OF, (original ^ value) & 0x8000 != 0); }\n}\n",
    );
    Ok(s)
}

/// Emit only widened host arithmetic: signed DX:AX / -1 must fault for a
/// guest quotient overflow, never panic on a host signed MIN / -1 operation.
/// MUL/IMUL define CF and OF only; preserve the other (undefined) flags.
/// DIV/IDIV leave all undefined flags unchanged, including on a divide fault.
fn mul_div_body(i: &Instruction) -> Result<String, String> {
    use Mnemonic::*;
    let mnemonic = i.mnemonic();
    let count = i.op_count();
    let bits = width(i, 0)?;
    if !matches!(bits, 8 | 16)
        || !(count == 1 || (mnemonic == Imul && bits == 16 && matches!(count, 2 | 3)))
    {
        return Err("unsupported multiply/divide operand form".into());
    }
    let mut s = String::new();
    if count > 1 {
        if i.op0_kind() != OpKind::Register
            || width(i, 1)? != bits
            || (count == 3 && width(i, 2)? != bits)
        {
            return Err("unsupported IMUL operand form".into());
        }
        // Iced exposes the imm8 encoding as a sign-extended Immediate8to16.
        // Capture both sources before touching an aliased destination or EA.
        bind(i, 1, "src", &mut s)?;
        let (left, right) = if count == 2 {
            (read_op(i, 0, "unused")?, read_op(i, 1, "src")?)
        } else {
            (read_op(i, 1, "src")?, read_op(i, 2, "unused")?)
        };
        writeln!(
            s,
            "let a = ({left}) as i16 as i64; let b = ({right}) as i16 as i64;"
        )
        .unwrap();
        s.push_str("let product = a * b;\nlet overflow = product != product as i16 as i64;\n");
        s.push_str(&write_op(i, 0, "unused", "product as u16")?);
        s.push_str("m.regs.flags.set(Flags::CF | Flags::OF, overflow);\n");
        return Ok(s);
    }
    bind(i, 0, "src", &mut s)?;
    writeln!(s, "let operand = {};", read_op(i, 0, "src")?).unwrap();
    match mnemonic {
        Mul | Imul => {
            if mnemonic == Mul {
                writeln!(s, "let product = (m.regs.ax as u{bits} as u64) * (operand as u64);\nlet overflow = product > u{bits}::MAX as u64;").unwrap();
            } else {
                writeln!(s, "let product = (m.regs.ax as i{bits} as i64) * (operand as i{bits} as i64);\nlet overflow = product != product as i{bits} as i64;").unwrap();
            }
            s.push_str("m.regs.ax = product as u16;\n");
            if bits == 16 {
                s.push_str("m.regs.dx = (product >> 16) as u16;\n");
            }
            s.push_str("m.regs.flags.set(Flags::CF | Flags::OF, overflow);\n");
        }
        Div | Idiv => {
            let dividend = if bits == 8 {
                "m.regs.ax"
            } else {
                "((u32::from(m.regs.dx) << 16) | u32::from(m.regs.ax))"
            };
            if mnemonic == Div {
                writeln!(
                    s,
                    "let dividend = {dividend} as u64; let divisor = operand as u64;"
                )
                .unwrap();
            } else {
                writeln!(s, "let dividend = {dividend} as i{} as i64; let divisor = operand as i{bits} as i64;", bits * 2).unwrap();
            }
            s.push_str("if divisor == 0 { return Err(m.fault(\"divide error: zero divisor\")); }\nlet quotient = dividend / divisor;\n");
            let overflow = if mnemonic == Div {
                format!("quotient > u{bits}::MAX as u64")
            } else {
                format!("quotient < i{bits}::MIN as i64 || quotient > i{bits}::MAX as i64")
            };
            writeln!(s, "if {overflow} {{ return Err(m.fault(\"divide error: quotient overflow\")); }}\nlet remainder = dividend % divisor;").unwrap();
            if bits == 8 {
                s.push_str(
                    "m.regs.ax = u16::from(quotient as u8) | (u16::from(remainder as u8) << 8);\n",
                );
            } else {
                s.push_str("m.regs.ax = quotient as u16;\nm.regs.dx = remainder as u16;\n");
            }
        }
        _ => return Err("unsupported multiply/divide instruction".into()),
    }
    Ok(s)
}

fn body(n: &Node) -> Result<String, String> {
    use Mnemonic::*;
    let i = &n.instruction;
    if let Some(decoded) = &n.borland {
        return Ok(format!(
            "m.software_interrupt({}, {})?;\n",
            decoded.vector,
            n.address.ip.wrapping_add(2)
        ));
    }
    let m = i.mnemonic();
    let next = n.address.ip.wrapping_add(i.len() as u16);
    if let Some(problem) = &n.diagnostic {
        return Err(problem.clone());
    }
    let address32 = n
        .bytes
        .iter()
        .take_while(|&&b| {
            matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
            )
        })
        .any(|&b| b == 0x67);
    if address32 {
        return crate::lift_addr32::emit(i);
    }
    let operand32 = n
        .bytes
        .iter()
        .take_while(|&&b| {
            matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
            )
        })
        .any(|&b| b == 0x66);
    if operand32 {
        return crate::lift32::emit(i);
    }
    if i.has_lock_prefix() {
        return Err("LOCK/atomic memory semantics are unsupported".into());
    }
    let strings = matches!(
        m,
        Movsb | Movsw | Cmpsb | Cmpsw | Stosb | Stosw | Lodsb | Lodsw | Scasb | Scasw
    );
    if (i.has_rep_prefix() || i.has_repne_prefix()) && !strings && !matches!(m, Outsb | Outsw) {
        return Err("REP prefix on a non-string instruction is unsupported".into());
    }
    let mut s = String::new();
    if let Some(cond) = condition(m) {
        let target = direct(i, n.address.cs).ok_or("unsupported conditional target")?;
        if matches!(m, Loop | Loope | Loopne) {
            s.push_str("m.regs.cx = m.regs.cx.wrapping_sub(1);\n");
        }
        writeln!(
            s,
            "m.regs.ip = if {cond} {{ {} }} else {{ {next} }};",
            target.ip
        )
        .unwrap();
        return Ok(s);
    }
    match m {
        Mov|Xchg=>{
            let a=width(i,0)?; if a!=width(i,1)? {return Err("operand widths differ".into());}
            bind(i,0,"dst",&mut s)?;bind(i,1,"src",&mut s)?;
            if m==Xchg {writeln!(s,"let old = {};",read_op(i,0,"dst")?).unwrap();}
            let source=if let Some(slot)=&n.mutable_immediate {crate::mutable_immediate::emit_read(slot)} else {read_op(i,1,"src")?};
            writeln!(s,"let value = {source};").unwrap();
            s.push_str(&write_op(i,0,"dst","value")?);
            if i.op0_register()==Register::SS{s.push_str("m.irq_shadow=1;\n");}
            if m==Xchg {s.push_str(&write_op(i,1,"src","old")?);}
        }
        Movzx|Movsx=>{
            if width(i,0)?!=16||width(i,1)?!=8{return Err("unsupported MOVZX/MOVSX widths".into());}
            bind(i,1,"src",&mut s)?;let cast=if m==Movsx {"as i8 as i16 as u16"}else{"as u16"};
            writeln!(s,"let value = {} {cast};",read_op(i,1,"src")?).unwrap();s.push_str(&write_op(i,0,"dst","value")?);
        }
        Lea=>{
            if i.op1_kind()!=OpKind::Memory {return Err("LEA requires memory addressing".into());}
            writeln!(s,"let value = {};",offset(i)?).unwrap();s.push_str(&write_op(i,0,"dst","value")?);
        }
        Les|Lds=>{
            if i.op1_kind()!=OpKind::Memory || width(i,0)?!=16 {return Err("unsupported far pointer load".into());}
            bind(i,1,"src",&mut s)?;
            s.push_str("m.validate_span(src,4)?;\nlet value = m.read16(src)?; let segment = m.read16(src.wrapping_add(2))?;\n");
            s.push_str(&write_op(i,0,"dst","value")?);writeln!(s,"m.regs.{} = segment;",if m==Les {"es"} else {"ds"}).unwrap();
        }
        Add|Adc|Sub|Sbb|Cmp|And|Or|Xor|Test=>{
            let bits=width(i,0)?;if bits!=width(i,1)? {return Err("operand widths differ".into());}
            bind(i,0,"dst",&mut s)?;bind(i,1,"src",&mut s)?;
            writeln!(s,"let a = {}; let b = {};",read_op(i,0,"dst")?,read_op(i,1,"src")?).unwrap();
            if matches!(m,And|Or|Xor|Test) {
                let op=match m {And|Test=>"&",Or=>"|",_=>"^"};
                writeln!(s,"let value = a {op} b; m.regs.flags.set(Flags::CF | Flags::OF, false);").unwrap();s.push_str(&zsp(bits,"value"));
            } else {
                let name=match m {Add=>"add",Adc=>"adc",Sub|Cmp=>"sub",_=>"sbb"};
                writeln!(s,"let value = {name}{bits}(a,b,&mut m.regs.flags);").unwrap();
            }
            if !matches!(m,Cmp|Test) {s.push_str(&write_op(i,0,"dst","value")?);}
        }
        Inc|Dec|Neg|Not=>{
            let bits=width(i,0)?;bind(i,0,"dst",&mut s)?;writeln!(s,"let a = {};",read_op(i,0,"dst")?).unwrap();
            match m {
                Not=>s.push_str("let value = !a;\n"),
                Neg=>writeln!(s,"let value = sub{bits}(0,a,&mut m.regs.flags);").unwrap(),
                _=>{let op=if m==Inc {"add"} else {"sub"};writeln!(s,"let cf = m.regs.flags.cf(); let value = {op}{bits}(a,1,&mut m.regs.flags); m.regs.flags.set(Flags::CF,cf);").unwrap();}
            }
            s.push_str(&write_op(i,0,"dst","value")?);
        }
        Push=>{bind(i,0,"src",&mut s)?;if width(i,0)?!=16 {return Err("only 16-bit PUSH supported".into());}s.push_str("m.validate_ram_span(SegmentedAddress::new(m.regs.ss,m.regs.sp.wrapping_sub(2)),2)?;\n");writeln!(s,"let value = {}; aot_push(m,value)?;",read_op(i,0,"src")?).unwrap();}
        Pop=>{bind(i,0,"dst",&mut s)?;if width(i,0)?!=16 {return Err("only 16-bit POP supported".into());}s.push_str("let value = m.pop16()?;\n");s.push_str(&write_op(i,0,"dst","value")?);if i.op0_register()==Register::SS{s.push_str("m.irq_shadow=1;\n");}}
        Pushf=>s.push_str("aot_push(m,Machine::normalize_flags(m.regs.flags.bits()))?;\n"),
        Popf=>s.push_str("let value = m.pop16()?; m.regs.flags = Flags::from_bits(Machine::normalize_flags(value));\n"),
        Pusha=>{s.push_str("let sp = m.regs.sp;\nfor unit in 0..8u16 { m.validate_ram_span(SegmentedAddress::new(m.regs.ss,sp.wrapping_sub(2 + unit * 2)),2)?; }\n");for r in ["ax","cx","dx","bx"] {writeln!(s,"aot_push(m,m.regs.{r})?;").unwrap();}s.push_str("aot_push(m,sp)?;\n");for r in ["bp","si","di"] {writeln!(s,"aot_push(m,m.regs.{r})?;").unwrap();}}
        Popa=>{s.push_str("for unit in 0..8u16 { m.validate_ram_span(SegmentedAddress::new(m.regs.ss,m.regs.sp.wrapping_add(unit * 2)),2)?; }\n");for r in ["di","si","bp","skip","bx","dx","cx","ax"] {if r=="skip" {s.push_str("m.regs.sp = m.regs.sp.wrapping_add(2);\n");}else {writeln!(s,"m.regs.{r} = m.pop16()?;").unwrap();}}}
        Enter=>{
            if i.immediate8_2nd()&31!=0{return Err("ENTER nesting not implemented".into());}
            s.push_str("let old_bp=m.regs.bp;aot_push(m,old_bp)?;m.regs.bp=m.regs.sp;\n");writeln!(s,"m.regs.sp=m.regs.sp.wrapping_sub({});",i.immediate16()).unwrap();
        }
        Leave=>s.push_str("m.leave16()?;\n"),
        Call|Jmp=>{
            let far=matches!(i.op0_kind(),OpKind::FarBranch16) || (i.op0_kind()==OpKind::Memory && i.memory_size().size()==4);
            if m==Call {s.push_str("m.validate_ram_span(SegmentedAddress::new(m.regs.ss,m.regs.sp.wrapping_sub(2)),2)?;\n");if far {s.push_str("m.validate_ram_span(SegmentedAddress::new(m.regs.ss,m.regs.sp.wrapping_sub(4)),2)?;\n");}}
            if let Some(target)=direct(i,n.address.cs) {writeln!(s,"let target_ip = {}u16; let target_cs = {}u16;",target.ip,target.cs).unwrap();}
            else if far {
                bind(i,0,"src",&mut s)?;s.push_str("m.validate_span(src,4)?;\nlet target_ip = m.read16(src)?; let target_cs = m.read16(src.wrapping_add(2))?;\n");
            }else {bind(i,0,"src",&mut s)?;if width(i,0)?!=16 {return Err("unsupported transfer width".into());}writeln!(s,"let target_ip = {};",read_op(i,0,"src")?).unwrap();}
            if m==Call {if far {s.push_str("aot_push(m,m.regs.cs)?;\n");}writeln!(s,"aot_push(m,{next})?;").unwrap();}
            if far {s.push_str("m.regs.cs = target_cs;\n");}s.push_str("m.regs.ip = target_ip;\n");return Ok(s);
        }
        Ret|Retf=>{
            let cleanup=if i.op_count()==0 {0} else {i.immediate16()};
            if m==Retf {writeln!(s,"m.far_return({cleanup})?;").unwrap();}
            else {writeln!(s,"m.regs.ip = m.pop16()?; m.regs.sp = m.regs.sp.wrapping_add({cleanup});").unwrap();}
            return Ok(s);
        }
        Cli|Sti|Cld|Std|Clc|Stc=>{
            if m==Sti{s.push_str("if !m.regs.flags.contains(Flags::IF){m.irq_shadow=1;}\n");}
            let (flag,value)=match m {Cli=>("IF",false),Sti=>("IF",true),Cld=>("DF",false),Std=>("DF",true),Clc=>("CF",false),_=>("CF",true)};
            writeln!(s,"m.regs.flags.set(Flags::{flag},{value});").unwrap();
        }
        Cmc=>s.push_str("m.regs.flags.set(Flags::CF,!m.regs.flags.cf());\n"),
        Cbw=>s.push_str("m.regs.ax = (m.regs.ax as u8 as i8 as i16) as u16;\n"),
        Cwd=>s.push_str("m.regs.dx = if m.regs.ax & 0x8000 != 0 { 0xffff } else { 0 };\n"),
        Lahf=>s.push_str("m.regs.set_ah((m.regs.flags.bits() as u8 & 0xd5) | 2);\n"),
        Sahf=>s.push_str("m.regs.flags = Flags::from_bits((m.regs.flags.bits() & !0xd5) | (u16::from(m.regs.ah()) & 0xd5));\n"),
        Outsb|Outsw=>{
            let bits=if m==Outsb{8}else{16};let source_segment=if i.segment_prefix()==Register::None {Register::DS}else{i.segment_prefix()};
            let repeated=i.has_rep_prefix()||i.has_repne_prefix();
            if repeated {s.push_str("for unit in 0..64 { if m.regs.cx==0 {break;} if unit!=0 {m.steps+=1;}\n");}
            writeln!(s,"let at=SegmentedAddress::new({},m.regs.si);let value=m.read{bits}(at)? as u16;m.port_out(m.regs.dx,value,{bits})?;let delta=if m.regs.flags.contains(Flags::DF){{0u16.wrapping_sub({})}}else{{{}}};m.regs.si=m.regs.si.wrapping_add(delta);",read_reg(source_segment)?,bits/8,bits/8).unwrap();
            if repeated {s.push_str("m.regs.cx=m.regs.cx.wrapping_sub(1); } if m.regs.cx!=0{return Ok(()); }\n");}
        }
        Int=>{writeln!(s,"m.software_interrupt({}, {next})?;",i.immediate8()).unwrap();return Ok(s);}
        Iret=>{s.push_str("m.interrupt_return()?;\n");return Ok(s);}
        In=>{
            let bits=width(i,0)?;writeln!(s,"let port = {} as u16; let value = m.port_in(port,{})? as u{bits};",read_op(i,1,"src")?,bits).unwrap();s.push_str(&write_op(i,0,"dst","value")?);
        }
        Out=>{let bits=width(i,1)?;writeln!(s,"let port = {} as u16; let value = {} as u16; m.port_out(port,value,{})?;",read_op(i,0,"dst")?,read_op(i,1,"src")?,bits).unwrap();}
        Xlatb=>{
            let segment=if i.segment_prefix()==Register::None {Register::DS} else {i.segment_prefix()};
            writeln!(s,"let address = SegmentedAddress::new({},m.regs.bx.wrapping_add(u16::from(m.regs.al()))); let value = m.read8(address)?; m.regs.set_al(value);",read_reg(segment)?).unwrap();
        }
        Mul|Imul|Div|Idiv=>s.push_str(&mul_div_body(i)?),
        Shl|Sal|Shr|Sar|Rol|Ror|Rcl|Rcr=>s.push_str(&shift_body(i)?),
        Shld|Shrd=>s.push_str(&double_shift16_body(i)?),
        Movsb|Movsw|Cmpsb|Cmpsw|Stosb|Stosw|Lodsb|Lodsw|Scasb|Scasw=>s.push_str(&string_body(n)?),
        Nop=>(),
        _=>return Err(format!("unsupported instruction {} ({:?})",n.text,i.code())),
    }
    writeln!(s, "m.regs.ip = {next};").unwrap();
    Ok(s)
}

/// One explicitly confirmed, build-time-only memory overlay.
///
/// The complete replacement window is guarded against `expected` before any
/// bytes are changed. Window lengths must be equal; instruction lengths within
/// that window may differ (for example a one-byte RET replaced by a far CALL).
#[derive(Clone, Debug)]
pub struct StaticOverlay {
    pub address: Address,
    pub expected: Vec<u8>,
    pub replacement: Vec<u8>,
}

/// Create a separate static code view without modifying the supplied image.
/// Overlays are simultaneous, nonoverlapping, exact-byte edits, never a runtime
/// patch policy. Recover each returned view separately and pass its graph to
/// [`emit_rust_variants`]. The caller still supplies confirmed code ranges/seeds.
pub fn apply_static_overlays(memory: &[u8], overlays: &[StaticOverlay]) -> Result<Vec<u8>, String> {
    let mut windows = Vec::new();
    for overlay in overlays {
        let start = overlay.address.physical();
        let len = overlay.expected.len();
        let end = start.checked_add(len).ok_or("overlay address overflow")?;
        if len == 0
            || len != overlay.replacement.len()
            || len > 0x10000 - usize::from(overlay.address.ip)
            || end > memory.len().min(0x100000)
        {
            return Err(format!(
                "invalid static overlay window at {}",
                overlay.address
            ));
        }
        if memory[start..end] != overlay.expected {
            return Err(format!(
                "static overlay expected bytes mismatch at {}",
                overlay.address
            ));
        }
        windows.push((start, end, overlay));
    }
    windows.sort_by_key(|window| window.0);
    if windows.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("static overlay windows overlap (including segment aliases)".into());
    }
    let mut view = memory.to_vec();
    for (start, end, overlay) in windows {
        view[start..end].copy_from_slice(&overlay.replacement);
    }
    Ok(view)
}

/// Emit a finite union of independently recovered static code views.
///
/// CS:IP selects a static case; only COMPLETE exact instruction bytes select
/// conflicting variants. No runtime decoding, JIT, guard-disabling switch, or
/// default-view fallback is generated. Unknown bytes fault before counting or
/// executing an instruction. Different instruction lengths are supported.
/// Identical executable cases are deduplicated. Ambiguous byte-prefix cases,
/// inconsistent IR and mutable-immediate exemptions on conflicting variants
/// are rejected at build time, rather than depending on candidate order.
/// Nonconflicting mutable MOV cases retain their full immutable-range guards.
/// IRQ/service entry and instruction accounting run once at the root.
pub fn emit_rust_variants(graphs: &[Graph]) -> Result<String, String> {
    if graphs.is_empty() {
        return Err("at least one static code view is required".into());
    }
    let mut nodes = BTreeMap::<Address, Vec<&Node>>::new();
    for graph in graphs {
        for (at, node) in &graph.nodes {
            validate_variant_node(*at, node)?;
            let cases = nodes.entry(*at).or_default();
            if let Some(existing) = cases.iter().find(|existing| existing.bytes == node.bytes) {
                if existing.mutable_immediate != node.mutable_immediate
                    || body(existing) != body(node)
                    || existing.borland.as_ref().map(|b| b.kind)
                        != node.borland.as_ref().map(|b| b.kind)
                {
                    return Err(format!(
                        "inconsistent semantics for identical variant bytes at {at}"
                    ));
                }
            } else {
                cases.push(node);
            }
        }
    }
    for (at, cases) in &mut nodes {
        cases.sort_by(|a, b| a.bytes.cmp(&b.bytes));
        if cases.len() > 1 {
            if cases.iter().any(|node| node.mutable_immediate.is_some()) {
                return Err(format!(
                    "conflicting variants require exact bytes, not mutable immediates, at {at}"
                ));
            }
            if cases
                .windows(2)
                .any(|pair| pair[1].bytes.starts_with(&pair[0].bytes))
            {
                return Err(format!("ambiguous byte-prefix variants at {at}"));
            }
        }
    }
    emit_nodes(&nodes)
}

fn validate_variant_node(at: Address, node: &Node) -> Result<(), String> {
    if at != node.address
        || node.physical != at.physical()
        || node.bytes.is_empty()
        || node.bytes.len() > 15
        || node.bytes.len() > 0x10000 - usize::from(at.ip)
        || node.physical + node.bytes.len() > 0x100000
    {
        return Err(format!("inconsistent variant IR node at {at}"));
    }
    let instruction = if let Some(borland) = &node.borland {
        let verified = crate::fpu_macro::decode(&node.bytes, at.ip)?
            .ok_or_else(|| format!("inconsistent Borland variant at {at}"))?;
        if verified.ip != borland.ip
            || verified.vector != borland.vector
            || verified.kind != borland.kind
            || verified.len != borland.len
            || verified.original_bytes != borland.original_bytes
            || verified.transformed_bytes != borland.transformed_bytes
            || verified.len != node.bytes.len()
        {
            return Err(format!("inconsistent Borland variant at {at}"));
        }
        verified.instruction.unwrap_or_else(|| {
            Decoder::with_ip(16, &node.bytes, u64::from(at.ip), DecoderOptions::NONE).decode()
        })
    } else {
        let mut decoder = Decoder::with_ip(16, &node.bytes, u64::from(at.ip), DecoderOptions::NONE);
        let instruction = decoder.decode();
        if decoder.position() != node.bytes.len() {
            return Err(format!(
                "variant guard must cover exactly one complete instruction at {at}"
            ));
        }
        instruction
    };
    if !instruction.eq_all_bits(&node.instruction) {
        return Err(format!("variant bytes do not match decoded IR at {at}"));
    }
    Ok(())
}

/// Emit a standalone module importing only the runtime and semantics modules.
/// Unsupported cases still compile, and fault at their exact original CS:IP.
/// `steps` counts ordinary instructions or REP elements (64-element batches).
/// The host must enforce its budget between calls; one REP call can overshoot
/// that budget by at most 63 elements. No guest code is fetched to execute it.
pub fn emit_rust(graph: &Graph) -> Result<String, String> {
    let nodes = graph
        .nodes
        .iter()
        .map(|(at, node)| (*at, vec![node]))
        .collect();
    emit_nodes(&nodes)
}

fn emit_nodes(nodes: &BTreeMap<Address, Vec<&Node>>) -> Result<String, String> {
    // Descriptor identity is shared by the selected OUT and its paired LOOP.
    // Reject variants and address-size overrides: this contract owns CX, not ECX.
    let mut palette_cycles = BTreeMap::new();
    for (at, cases) in nodes {
        if cases.len() != 1 {
            continue;
        }
        let n = cases[0];
        if n.instruction.mnemonic() != Mnemonic::Out
            || n.instruction.op0_register() != Register::DX
            || n.instruction.op1_register() != Register::AL
        {
            continue;
        }
        let next = Address {
            cs: at.cs,
            ip: at.ip.wrapping_add(n.instruction.len() as u16),
        };
        let Some(lp) = nodes.get(&next).filter(|v| v.len() == 1).map(|v| v[0]) else {
            continue;
        };
        if lp.instruction.mnemonic() != Mnemonic::Loop
            || lp.instruction.op0_kind() != OpKind::NearBranch16
            || lp.bytes[..lp.bytes.len().saturating_sub(1)].contains(&0x67)
        {
            continue;
        }
        let Some(target) = direct(&lp.instruction, at.cs) else {
            continue;
        };
        let Some(load) = nodes.get(&target).filter(|v| v.len() == 1).map(|v| v[0]) else {
            continue;
        };
        if load.instruction.mnemonic() != Mnemonic::Lodsb
            || load.instruction.has_rep_prefix()
            || load.instruction.has_repne_prefix()
            || load.instruction.op1_kind() != OpKind::MemorySegSI
            || target.ip.wrapping_add(load.instruction.len() as u16) != at.ip
            || [n, lp, load]
                .iter()
                .any(|n| n.mutable_immediate.is_some() || n.borland.is_some() || body(n).is_err())
        {
            continue;
        }
        for (at, n) in [(*at, n), (next, lp), (target, load)] {
            validate_variant_node(at, n)?;
        }
        palette_cycles.insert(*at, (next, target));
    }
    let palette_loops: BTreeMap<_, _> = palette_cycles
        .iter()
        .map(|(at, (lp, target))| (*lp, (*at, *target)))
        .collect();
    let mut out = String::from(
        "// Generated AOT code. No runtime instruction decoder.\nuse pal_wayland::runtime::{Machine,Fault};\nuse pal_wayland::semantics::*;\n#[allow(dead_code)]\nfn aot_push(m: &mut Machine, value: u16) -> Result<(),Fault> {\nlet sp = m.regs.sp.wrapping_sub(2);\nm.validate_ram_span(SegmentedAddress::new(m.regs.ss,sp),2)?;\nm.write16(SegmentedAddress::new(m.regs.ss,sp),value)?; m.regs.sp = sp; Ok(())\n}\npub fn step(m: &mut Machine) -> Result<(),Fault> {\nif m.poll_timer_irq()? {return Ok(()); }\nif (m.regs.cs,m.regs.ip)==(0xf000,0x200) && m.irq_enabled {m.bios_timer_return()?;m.steps+=1;return Ok(()); }\nif (m.regs.cs,m.regs.ip)==(0xf000,0x100) && m.xms_entry_registered {for delta in [0,2] {m.validate_ram_span(SegmentedAddress::new(m.regs.ss,m.regs.sp.wrapping_add(delta)),2)?;}m.xms_call()?;m.far_return(0)?;m.steps+=1;return Ok(()); }\nm.validate_ram_span(SegmentedAddress::new(m.regs.cs,m.regs.ip),1)?;\nmatch (m.regs.cs,m.regs.ip >> 8) {\n",
    );
    let groups: BTreeSet<_> = nodes.keys().map(|a| (a.cs, a.ip >> 8)).collect();
    for (cs, page) in &groups {
        writeln!(out, "({cs},{page}) => block_{cs}_{page}(m),").unwrap();
    }
    out.push_str("_ => Err(m.fault(\"unknown static target\")),\n}\n}\n");
    out.push_str("/// Conservative static host-service boundary, tested before guest execution.\npub fn playback_boundary(m: &Machine) -> bool {\nmatch (m.regs.cs,m.regs.ip) {\n");
    for (at, cases) in nodes {
        if cases
            .iter()
            .any(|n| n.instruction.mnemonic() == Mnemonic::Int)
        {
            writeln!(out, "({}, {}) => true,", at.cs, at.ip).unwrap();
        }
    }
    out.push_str("(0xf000,0x100) => m.xms_entry_registered,\n_ => false,\n}\n}\n");
    out.push_str(&crate::lift32::helpers());
    for (cs, page) in groups {
        writeln!(out,"#[allow(unused_variables,unused_parens)]\nfn block_{cs}_{page}(m: &mut Machine)->Result<(),Fault>{{\nmatch (m.regs.cs,m.regs.ip) {{").unwrap();
        for (at, cases) in nodes
            .iter()
            .filter(|(a, _)| a.cs == cs && a.ip >> 8 == page)
        {
            let comment = if cases.len() == 1 {
                cases[0].text.as_str()
            } else {
                "static variants"
            };
            writeln!(out, "({}, {}) => {{ // {at}: {comment}", at.cs, at.ip).unwrap();
            for n in cases {
                if at != &n.address
                    || n.bytes.is_empty()
                    || n.bytes.len() > 15
                    || n.physical != at.physical()
                {
                    return Err(format!("inconsistent IR node at {at}"));
                }
                if cases.len() == 1 {
                    writeln!(
                        out,
                        "m.validate_ram_span(SegmentedAddress::new(m.regs.cs,m.regs.ip),{})?;",
                        n.bytes.len()
                    )
                    .unwrap();
                }
                if cases.len() > 1 {
                    writeln!(
                        out,
                        "if m.guard_code(&{:?}).is_ok() {{ // {}",
                        n.bytes, n.text
                    )
                    .unwrap();
                }
                if cases.len() == 1
                    && n.borland
                        .as_ref()
                        .is_some_and(|b| b.kind == crate::fpu_macro::MacroKind::Sync)
                {
                    // Observed software INT3D handler rewinds IP, writes MOV AX,AX,
                    // then IRET. Compile this one exact alternative, never decode it.
                    writeln!(out,"if m.read16(SegmentedAddress::new(m.regs.cs,m.regs.ip))? == 0xc089 {{ m.guard_code(&[0x89,0xc0])?; m.steps+=1; m.regs.ip={}; return Ok(()); }}",at.ip.wrapping_add(2)).unwrap();
                }
                if let Some(slot) = &n.mutable_immediate {
                    let verified = crate::mutable_immediate::analyze(&n.instruction, &n.bytes)?;
                    if *slot != verified {
                        return Err(format!("mutable immediate does not match {at}"));
                    }
                    for range in slot.immutable_ranges() {
                        for index in range {
                            writeln!(out,"if m.read8(SegmentedAddress::new(m.regs.cs,m.regs.ip.wrapping_add({})))? != {} {{return Err(m.fault(\"code changed outside mutable immediate\"));}}",index,n.bytes[index]).unwrap();
                        }
                    }
                    out.push_str("m.steps += 1;\n");
                } else {
                    writeln!(out, "m.guard_code(&{:?})?;\nm.steps += 1;", n.bytes).unwrap();
                }
                // This descriptor is emitted from the verified selected IR.
                // Address-size32 strings have a different destination contract.
                let rep_width = if n.instruction.has_rep_prefix()
                    && n.instruction.op0_kind() == OpKind::MemoryESDI
                {
                    match n.instruction.mnemonic() {
                        Mnemonic::Movsb | Mnemonic::Stosb => Some(1),
                        Mnemonic::Movsw | Mnemonic::Stosw => Some(2),
                        Mnemonic::Movsd | Mnemonic::Stosd => Some(4),
                        _ => None,
                    }
                } else {
                    None
                };
                if rep_width.is_some() {
                    validate_variant_node(*at, n)?;
                }
                match body(n) {
                    Ok(s) => {
                        if let Some(width) = rep_width {
                            writeln!(out, "m.begin_vram_rep({width})?;").unwrap();
                        }
                        if let Some((lp, _)) = palette_cycles.get(at) {
                            // Both hooks run only after the exact instruction guard.
                            writeln!(out, "m.palette_stream_out({})?;", lp.ip).unwrap();
                            writeln!(out, "m.regs.ip = {};", lp.ip).unwrap();
                        } else {
                            out.push_str(&s);
                        }
                        if let Some((owner, target)) = palette_loops.get(at) {
                            writeln!(
                                out,
                                "m.finish_palette_stream({}, {}, {}, {});",
                                owner.cs, owner.ip, at.ip, target.ip
                            )
                            .unwrap();
                        }
                        if rep_width.is_some() {
                            writeln!(out, "m.finish_vram_rep({}, {})?;", at.cs, at.ip).unwrap();
                        }
                    }
                    Err(reason) => {
                        writeln!(out, "return Err(m.fault({:?}));", format!("{at}: {reason}"))
                            .unwrap()
                    }
                }
                if cases.len() > 1 {
                    out.push_str("return Ok(());\n}\n");
                }
            }
            if cases.len() > 1 {
                out.push_str(
                    "return Err(m.fault(\"unknown static instruction variant (code changed)\"));\n",
                );
            }
            out.push_str("}\n");
        }
        out.push_str("_ => return Err(m.fault(\"unknown static target\")),\n}\nOk(())\n}\n");
    }
    Ok(out)
}

// Resolve authored fixture modules independently of their physical grouping.
#[cfg(test)]
pub(crate) fn runtime_fixture_source(name: &str) -> std::path::PathBuf {
    let directory = match name {
        "semantics" | "mz" | "runtime" | "fpu" => "src",
        "video" | "text_video" | "planar_video" | "dos_files" | "cdrom" | "audio"
        | "sound_blaster" | "xms" => "src/devices",
        _ => panic!("unknown runtime fixture module {name}"),
    };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(directory)
        .join(format!("{name}.rs"))
}

// Source-including fixtures bypass Cargo's --extern wiring. Resolve rustix
// beside this Cargo test executable, not from a global/default target directory.
#[cfg(test)]
pub(crate) fn runtime_fixture_rustc() -> std::process::Command {
    let executable = std::env::current_exe().unwrap();
    let deps = executable.parent().unwrap();
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .unwrap();
    let version = manifest
        .lines()
        .find(|line| line.starts_with("rustix = "))
        .and_then(|line| line.split("version = \"=").nth(1))
        .and_then(|version| version.split('"').next())
        .expect("fixture linking requires the manifest's pinned rustix version");
    let source = format!("/rustix-{version}/src/lib.rs");
    let mut candidates: Vec<_> = std::fs::read_dir(deps)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("librustix-") && name.ends_with(".rlib"))
        })
        .filter(|path| {
            let name = path.file_stem().unwrap().to_str().unwrap();
            let dep_info = deps.join(format!("{}.d", name.strip_prefix("lib").unwrap()));
            std::fs::read_to_string(dep_info).is_ok_and(|info| info.contains(&source))
        })
        .collect();
    // Feature/profile variants can coexist; prefer the most recently built
    // matching version. Its transitive crates are in the same dependency dir.
    candidates.sort_by_key(|path| {
        (
            std::fs::metadata(path).unwrap().modified().unwrap(),
            path.clone(),
        )
    });
    let rustix = candidates.last().unwrap_or_else(|| {
        panic!(
            "no compiled rustix {version} rlib beside {}",
            executable.display()
        )
    });
    let mut command = std::process::Command::new("rustc");
    command
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("--extern")
        .arg(format!("rustix={}", rustix.display()));
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(ip: u16) -> Address {
        Address { cs: 0, ip }
    }
    #[test]
    fn follows_both_edges_and_call_continuation_not_skipped_data() {
        // JZ 7; CALL 8; JMP 9; RET; RET; RET; unreachable invalid byte.
        let g = recover(
            &[0x74, 5, 0xe8, 3, 0, 0xeb, 2, 0xc3, 0xc3, 0xc3, 0x0f],
            &[at(0)],
            100,
        )
        .unwrap();
        assert_eq!(
            g.nodes.keys().copied().collect::<Vec<_>>(),
            vec![at(0), at(2), at(5), at(7), at(8), at(9)]
        );
        assert_eq!(g.nodes[&at(0)].successors, vec![at(7), at(2)]);
        assert_eq!(g.nodes[&at(2)].successors, vec![at(8), at(5)]);
        assert_eq!(g.nodes[&at(2)].bytes, vec![0xe8, 3, 0]);
    }
    #[test]
    fn ranges_and_borland_inline_operands_are_not_scanned() {
        let memory = [0xea, 0, 0, 0, 0, 0x90, 0xcd, 0x35, 0xb8, 1, 0, 0xc3];
        let g = recover_in_ranges(&memory, &[at(5)], 100, std::slice::from_ref(&(5..12))).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert!(g.nodes[&at(6)].successors.is_empty());
        assert!(g.unresolved[0].contains("Borland software FPU"));
        let g = recover_in_ranges(&memory, &[at(0)], 100, std::slice::from_ref(&(5..12))).unwrap();
        assert!(g.nodes.is_empty());
        assert!(g.unresolved[0].contains("outside confirmed"));
        assert!(recover_in_ranges(&memory, &[at(0)], 10, &[0..6, 5..12]).is_err());
    }
    #[test]
    fn borland_macro_boundaries_are_explicit_opt_in_and_dispatch_guest_handler() {
        let code = [0xcd, 0x3b, 0x07, 0xc3];
        let g =
            recover_borland_in_ranges(&code, &[at(0)], 10, std::slice::from_ref(&(0..4))).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[&at(0)].bytes, [0xcd, 0x3b, 0x07]);
        assert!(g.nodes.contains_key(&at(3)));
        assert!(!g.nodes.contains_key(&at(2)));
        assert_eq!(
            g.nodes[&at(0)].instruction.code(),
            iced_x86::Code::Fild_m16int
        );
        let output = emit_rust(&g).unwrap();
        assert!(output.contains("m.software_interrupt(59, 2)?"));
        assert_eq!(recover(&code, &[at(0)], 10).unwrap().nodes.len(), 1);
    }
    #[test]
    fn already_relocated_far_target_is_not_rebased_twice() {
        let mut memory = vec![0; 0x202];
        memory[..5].copy_from_slice(&[0xea, 1, 0, 0x20, 0]);
        memory[0x201] = 0xcb;
        let g = recover(&memory, &[at(0)], 100).unwrap();
        let target = Address { cs: 0x20, ip: 1 };
        assert_eq!(g.nodes[&at(0)].successors, vec![target]);
        assert_eq!(g.nodes[&target].physical, 0x201);
    }
    #[test]
    fn invalid_truncated_and_32_bit_paths_are_explicit() {
        for (bytes, term) in [
            (&[0xf0, 0x90][..], "invalid"),
            (&[0xb8, 1][..], "truncated"),
        ] {
            let g = recover(bytes, &[at(0)], 10).unwrap();
            assert_eq!(g.nodes.len(), 1);
            assert!(g.unresolved[0].contains(term), "{:?}", g.unresolved);
            assert!(emit_rust(&g).unwrap().contains("return Err(m.fault("));
        }
    }
    #[test]
    fn dword_operand_is_followed_but_32bit_address_is_rejected() {
        let g = recover(&[0x66, 0x33, 0xc0, 0xf3, 0x66, 0xab, 0xc3], &[at(0)], 20).unwrap();
        assert_eq!(g.nodes.len(), 3);
        let text = emit_rust(&g).unwrap();
        assert!(text.contains("m.set32(0"));
        assert!(text.contains("m.write32(dest"));
    }
    #[test]
    fn segment_boundary_and_memory_bounds_do_not_alias() {
        let mut memory = vec![0; 0x10000];
        memory[0xffff] = 0xb8;
        let g = recover(&memory, &[at(0xffff)], 10).unwrap();
        assert!(g.unresolved[0].contains("segment boundary"));
        let g = recover(
            &memory,
            &[Address {
                cs: 0xffff,
                ip: 0xffff,
            }],
            10,
        )
        .unwrap();
        assert!(g.nodes.is_empty());
        assert!(g.unresolved[0].contains("outside"));
    }
    #[test]
    fn confirmed_overlapping_entries_and_aliases_keep_independent_guards() {
        let graph = recover(&[0x3d, 0xb0, 1, 0xc3], &[at(0), at(1)], 10).unwrap();
        assert_eq!(graph.nodes[&at(0)].bytes, [0x3d, 0xb0, 1]);
        assert_eq!(graph.nodes[&at(1)].bytes, [0xb0, 1]);
        assert!(graph.unresolved.iter().any(|s| s.contains("overlapping")));
        let code = emit_rust(&graph).unwrap();
        assert!(code.contains("m.guard_code(&[61, 176, 1])"));
        assert!(code.contains("m.guard_code(&[176, 1])"));
        let mut mem = vec![0; 17];
        mem[16] = 0xc3;
        let aliases = recover(&mem, &[at(16), Address { cs: 1, ip: 0 }], 10).unwrap();
        assert_eq!(aliases.nodes.len(), 2);
    }
    #[test]
    fn budget_and_indirect_targets_are_explicit() {
        let g = recover(&[0x90, 0x90, 0xc3], &[at(0)], 1).unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert!(g.unresolved[0].contains("budget"));
        let g = recover(&[0xff, 0xe0, 0xc3], &[at(0)], 10).unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert!(g.unresolved[0].contains("dynamic"));
    }
    #[test]
    fn generated_code_compiles_and_executes_without_iced_runtime() {
        // Original fixtures, not bytes taken from PAL. The fixture binary uses
        // the real runtime/semantics modules and is compiled without iced-x86.
        let fixtures: &[&[u8]] = &[
            &[
                0xb4, 0x12, 0xb0, 0xfe, 0x04, 3, 0x14, 0, 0x1c, 1, 0x2c, 1, 0x0c, 0x80, 0x24, 0xf0,
                0x34, 0x11, 0xa8, 0x80, 0x40, 0x48, 0xf7, 0xd0, 0xf7, 0xd8, 0x50, 0x5b, 0x93, 0x9c,
                0x9d, 0x60, 0x61, 0x98, 0x99, 0x9f, 0x9e, 0xfa, 0xfb, 0xfc, 0xfd, 0xf8, 0xf9, 0xf5,
                0x90, 0xc3,
            ],
            &[0xf3, 0xa4, 0xc3],
            &[0xf3, 0xa6, 0xc3],
            &[0xd9, 0xe8, 0xc3],
            &[
                0x8b, 0x42, 0xfe, 0x89, 0x42, 0xfe, 0x87, 0x42, 0xfe, 0x8d, 0x42, 0xfe, 0xc4, 0x1c,
                0xc5, 0x1c, 0x8f, 0x04, 0xff, 0x34, 0xc3,
            ],
            &[
                0xd0, 0xc0, 0xd0, 0xc8, 0xd0, 0xd0, 0xd0, 0xd8, 0xd0, 0xe0, 0xd0, 0xe8, 0xd0, 0xf8,
                0xd3, 0xe0, 0xc1, 0xe8, 4, 0xc3,
            ],
            &[
                0xa4, 0xa5, 0xa6, 0xa7, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xf3, 0xa4, 0xf3, 0xa5,
                0xf2, 0xae, 0xf3, 0xa7, 0x26, 0xac, 0xd7, 0xc3,
            ],
            &[
                0xe4, 0x80, 0xe5, 0x80, 0xe6, 0x80, 0xe7, 0x80, 0xec, 0xed, 0xee, 0xef, 0xcd, 0x21,
                0xc3,
            ],
            &[0xff, 0xd0, 0xff, 0x14, 0xff, 0xe0],
            &[0xff, 0x1c, 0xff, 0x2c],
            &[0xca, 4, 0],
            &[0xc9, 0xc3],
            &[0x9a, 6, 0, 0, 0x0e, 0xc3, 0xcb],
            &[0xe8, 1, 0, 0xc3, 0xc3],
        ];
        let mut graph = Graph::default();
        for (index, bytes) in fixtures.iter().enumerate() {
            let cs = (index as u16 + 2) * 0x100;
            let base = usize::from(cs) * 16;
            let mut memory = vec![0; base + bytes.len()];
            memory[base..].copy_from_slice(bytes);
            let g = recover_in_ranges(
                &memory,
                &[Address { cs, ip: 0 }],
                100,
                std::slice::from_ref(&(base..memory.len())),
            )
            .unwrap();
            for node in g.nodes.values() {
                if index != 3 {
                    assert!(body(node).is_ok(), "{}: {:?}", node.text, body(node));
                }
            }
            graph.nodes.extend(g.nodes);
        }
        for opcode in (0x70..=0x7f).chain(0xe0..=0xe3) {
            let cs = u16::from(opcode) * 0x10 + 0x1000;
            let base = usize::from(cs) * 16;
            let mut memory = vec![0; base + 4];
            memory[base..].copy_from_slice(&[opcode, 1, 0xc3, 0xc3]);
            let g = recover(&memory, &[Address { cs, ip: 0 }], 10).unwrap();
            assert!(g.nodes.values().all(|n| body(n).is_ok()));
            graph.nodes.extend(g.nodes);
        }
        let root = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = root.join(format!("pal-aot-lift-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut program = String::from("#![allow(dead_code)]\nextern crate self as pal_wayland;\n");
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
        writeln!(
            program,
            "mod generated {{ {} }}",
            emit_rust(&graph).unwrap()
        )
        .unwrap();
        program.push_str("const FIXTURES: &[&[u8]] = &[\n");
        for bytes in fixtures {
            writeln!(program, "&{bytes:?},").unwrap();
        }
        program.push_str("];\n");
        program.push_str(r#"
use runtime::Machine;
use semantics::*;
fn machine(index: usize) -> Machine {
    let mut bytes=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {bytes[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
    let mut m=Machine::from_mz(&bytes,0x1010).unwrap();
    m.regs=Registers{cs:(index as u16+2)*0x100,ss:0x8000,sp:0xfffe,ds:0x6000,es:0x7000,..Registers::default()};
    m.memory.write_bytes(SegmentedAddress::new(m.regs.cs,0),FIXTURES[index]).unwrap();m
}
fn main() {
    let mut m=machine(0);
    for (ax,cf) in [(0x1200,false),(0x12fe,false),(0x1201,true),(0x1202,false),(0x1201,false),(0x1200,false),(0x1280,false),(0x1280,false),(0x1291,false)] {
        generated::step(&mut m).unwrap();assert_eq!(m.regs.ax,ax);assert_eq!(m.regs.flags.cf(),cf);
    }
    while m.regs.ip<FIXTURES[0].len() as u16-1 {generated::step(&mut m).unwrap();}assert_eq!(m.regs.sp,0xfffe);
    let mut m=machine(1);m.regs.cx=130;m.regs.si=10;m.regs.di=20;
    let data:Vec<u8>=(0..130).map(|v|v as u8).collect();m.memory.write_bytes(SegmentedAddress::new(m.regs.ds,10),&data).unwrap();
    generated::step(&mut m).unwrap();assert_eq!((m.regs.cx,m.regs.ip,m.steps),(66,0,64));
    generated::step(&mut m).unwrap();generated::step(&mut m).unwrap();assert_eq!((m.regs.cx,m.regs.ip,m.steps),(0,2,130));
    let mut copied=vec![0;130];m.memory.read_bytes(SegmentedAddress::new(m.regs.es,20),&mut copied).unwrap();assert_eq!(copied,data);
    let mut m=machine(2);m.regs.cx=5;m.memory.write_bytes(SegmentedAddress::new(m.regs.ds,0),&[1,2,3,4,5]).unwrap();
    m.memory.write_bytes(SegmentedAddress::new(m.regs.es,0),&[1,2,9,4,5]).unwrap();generated::step(&mut m).unwrap();
    assert_eq!((m.regs.cx,m.regs.si,m.regs.di,m.regs.ip,m.steps),(2,3,3,2,3));assert!(!m.regs.flags.zf());
    let mut m=machine(1);m.regs.cx=0;generated::step(&mut m).unwrap();assert_eq!((m.regs.si,m.regs.di,m.regs.ip,m.steps),(0,0,2,1));
    let mut m=machine(1);m.regs.cx=2;m.regs.si=1;m.regs.di=1;m.regs.flags.set(Flags::DF,true);
    m.memory.write_bytes(SegmentedAddress::new(m.regs.ds,0),&[11,22]).unwrap();generated::step(&mut m).unwrap();
    assert_eq!((m.regs.si,m.regs.di),(0xffff,0xffff));assert_eq!(m.read16(SegmentedAddress::new(m.regs.es,0)).unwrap(),0x160b);
    let mut m=machine(3);assert!(generated::step(&mut m).unwrap_err().0.contains("unsupported instruction"));
    let mut m=machine(0);m.memory.write8(SegmentedAddress::new(m.regs.cs,0),0x90).unwrap();
    assert!(generated::step(&mut m).unwrap_err().0.contains("code changed"));assert_eq!((m.steps,m.regs.ax),(0,0));
    let mut m=machine(0);m.regs.cs=0xffff;assert!(generated::step(&mut m).unwrap_err().0.contains("unknown static target"));
    let mut m=machine(5);m.regs.ax=0x81;generated::step(&mut m).unwrap();assert_eq!(m.regs.al(),3);assert!(m.regs.flags.cf());assert!(m.regs.flags.of());
    generated::step(&mut m).unwrap();assert_eq!(m.regs.al(),0x81);assert!(m.regs.flags.cf());
    let mut m=machine(12);let cs=m.regs.cs;generated::step(&mut m).unwrap();assert_eq!((m.regs.cs,m.regs.ip,m.regs.sp),(cs,6,0xfffa));
    assert_eq!(m.read16(SegmentedAddress::new(m.regs.ss,0xfffa)).unwrap(),5);
    assert_eq!(m.read16(SegmentedAddress::new(m.regs.ss,0xfffc)).unwrap(),cs);
    generated::step(&mut m).unwrap();assert_eq!((m.regs.cs,m.regs.ip,m.regs.sp),(cs,5,0xfffe));
    let mut m=machine(13);generated::step(&mut m).unwrap();assert_eq!((m.regs.ip,m.regs.sp),(4,0xfffc));
    generated::step(&mut m).unwrap();assert_eq!((m.regs.ip,m.regs.sp),(3,0xfffe));
    println!("AOT fixture execution passed (no decoder linked)");
}
"#);
        std::fs::write(dir.join("main.rs"), program).unwrap();
        let compiler = runtime_fixture_rustc()
            .args(["--edition=2024", "-Dwarnings"])
            .arg(dir.join("main.rs"))
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
        assert!(String::from_utf8_lossy(&result.stdout).contains("AOT fixture execution passed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mul_div_all_forms_compile_and_execute_against_wide_oracle() {
        // Original encodings; no PAL bytes or host x86 instructions as oracle.
        // Source 0..7 is a GPR (including AH..BH for byte forms). The memory
        // forms exercise DS, default SS, ES override and unaligned disp16.
        let mut fixtures = Vec::new();
        let encode = |opcode: &[u8], field: u8, source: u8| {
            let mut bytes = Vec::new();
            if source == 10 {
                bytes.push(0x26);
            }
            bytes.extend_from_slice(opcode);
            let mode = match source {
                0..=7 => 0xc0 | source,
                8 => 0x00,  // [BX+SI]
                9 => 0x42,  // [BP+SI-2]
                10 => 0x01, // ES:[BX+DI]
                11 => 0x06, // [0201h]
                _ => unreachable!(),
            };
            bytes.push(mode | (field << 3));
            if source == 9 {
                bytes.push(0xfe);
            } else if source == 11 {
                bytes.extend_from_slice(&[1, 2]);
            }
            bytes
        };
        // (bytes, bits, signed, division, explicit destination, source, imm)
        for (field, signed, division) in [
            (4, false, false),
            (5, true, false),
            (6, false, true),
            (7, true, true),
        ] {
            for bits in [8, 16] {
                for source in 0..12 {
                    fixtures.push((
                        encode(&[if bits == 8 { 0xf6 } else { 0xf7 }], field, source),
                        bits,
                        signed,
                        division,
                        None,
                        source,
                        None,
                    ));
                }
            }
        }
        // All destinations and sources, both immediate encodings. The three
        // operand form must ignore the old destination; imm8 must sign extend.
        for destination in 0..8 {
            for source in 0..12 {
                fixtures.push((
                    encode(&[0x0f, 0xaf], destination, source),
                    16,
                    true,
                    false,
                    Some(destination),
                    source,
                    None,
                ));
                for immediate in [i16::MIN, -129, -1, 0, 1, 127, i16::MAX] {
                    let mut bytes = encode(&[0x69], destination, source);
                    bytes.extend_from_slice(&immediate.to_le_bytes());
                    fixtures.push((
                        bytes,
                        16,
                        true,
                        false,
                        Some(destination),
                        source,
                        Some(immediate),
                    ));
                }
                for immediate in [i8::MIN, -1, 0, 1, i8::MAX] {
                    let mut bytes = encode(&[0x6b], destination, source);
                    bytes.push(immediate as u8);
                    fixtures.push((
                        bytes,
                        16,
                        true,
                        false,
                        Some(destination),
                        source,
                        Some(i16::from(immediate)),
                    ));
                }
            }
        }
        let mut graph = Graph::default();
        let mut fixture_source = String::from("const FIXTURES: &[Fixture] = &[\n");
        let mut codes = BTreeSet::new();
        for (index, (bytes, bits, signed, division, destination, source, immediate)) in
            fixtures.iter().enumerate()
        {
            let ip = u16::try_from(index * 8).unwrap();
            let address = Address { cs: 0x200, ip };
            let base = address.physical();
            let mut memory = vec![0; base + bytes.len()];
            memory[base..].copy_from_slice(bytes);
            let g = recover(&memory, &[address], 1).unwrap();
            let node = &g.nodes[&address];
            assert_eq!(node.bytes, *bytes);
            assert!(body(node).is_ok(), "{}: {:?}", node.text, body(node));
            assert_eq!(
                node.instruction.op_count(),
                if immediate.is_some() {
                    3
                } else if destination.is_some() {
                    2
                } else {
                    1
                }
            );
            codes.insert(node.instruction.code());
            graph.nodes.extend(g.nodes);
            writeln!(fixture_source, "Fixture {{ bytes: &{bytes:?}, ip: {ip}, bits: {bits}, signed: {signed}, division: {division}, destination: {destination:?}, source: {source}, immediate: {immediate:?} }},").unwrap();
        }
        // All legal 8/16-bit iced-x86 codes (r/m covers register and memory).
        use iced_x86::Code;
        assert_eq!(
            codes,
            BTreeSet::from([
                Code::Mul_rm8,
                Code::Mul_rm16,
                Code::Imul_rm8,
                Code::Imul_rm16,
                Code::Div_rm8,
                Code::Div_rm16,
                Code::Idiv_rm8,
                Code::Idiv_rm16,
                Code::Imul_r16_rm16,
                Code::Imul_r16_rm16_imm16,
                Code::Imul_r16_rm16_imm8,
            ])
        );
        fixture_source.push_str("];\n");
        let generated = emit_rust(&graph).unwrap();
        assert!(!generated.contains("iced_x86"));
        assert!(!generated.contains("unsupported"));
        let root = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = root.join(format!("pal-aot-mul-div-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut program = String::from("#![allow(dead_code)]\nextern crate self as pal_wayland;\n");
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
        writeln!(program, "mod generated {{ {generated} }}\n{fixture_source}").unwrap();
        program.push_str(r#"
use runtime::Machine;
use semantics::*;
struct Fixture {
    bytes: &'static [u8], ip: u16, bits: u32, signed: bool, division: bool,
    destination: Option<u8>, source: u8, immediate: Option<i16>,
}
fn word(r: &Registers, index: u8) -> u16 {
    match index { 0=>r.ax, 1=>r.cx, 2=>r.dx, 3=>r.bx, 4=>r.sp, 5=>r.bp, 6=>r.si, 7=>r.di, _=>unreachable!() }
}
fn set_word(r: &mut Registers, index: u8, value: u16) {
    match index { 0=>r.ax=value, 1=>r.cx=value, 2=>r.dx=value, 3=>r.bx=value, 4=>r.sp=value, 5=>r.bp=value, 6=>r.si=value, 7=>r.di=value, _=>unreachable!() }
}
fn address(r: &Registers, source: u8) -> SegmentedAddress {
    match source {
        8=>SegmentedAddress::new(r.ds,r.bx.wrapping_add(r.si)),
        9=>SegmentedAddress::new(r.ss,r.bp.wrapping_add(r.si).wrapping_sub(2)),
        10=>SegmentedAddress::new(r.es,r.bx.wrapping_add(r.di)),
        11=>SegmentedAddress::new(r.ds,0x201),
        _=>unreachable!(),
    }
}
fn operand(m: &Machine, f: &Fixture) -> u16 {
    if f.source < 8 {
        if f.bits == 8 { (word(&m.regs,f.source%4) >> (8*(f.source/4))) & 255 }
        else { word(&m.regs,f.source) }
    } else if f.bits == 8 { u16::from(m.read8(address(&m.regs,f.source)).unwrap()) }
    else { m.read16(address(&m.regs,f.source)).unwrap() }
}
// Independent i128 oracle: sign conversion is subtraction, not an emitter
// cast; overflow is a mathematical range check; remainder is d - q*v.
fn integer(raw: u32, bits: u32, signed: bool) -> i128 {
    let modulus = 1i128 << bits;
    let value = i128::from(raw) % modulus;
    if signed && value >= modulus/2 { value - modulus } else { value }
}
fn oracle(r: Registers, f: &Fixture, source: u16) -> Result<Registers,&'static str> {
    let mut expected = r;
    let mask = (1u32 << f.bits) - 1;
    let divisor = integer(u32::from(source),f.bits,f.signed);
    let (min,max) = if f.signed { (-(1i128 << (f.bits-1)),(1i128 << (f.bits-1))-1) } else { (0,i128::from(mask)) };
    if f.division {
        let raw = if f.bits == 8 { u32::from(r.ax) } else { u32::from(r.dx)*65536 + u32::from(r.ax) };
        let dividend = integer(raw,2*f.bits,f.signed);
        if divisor == 0 { return Err("zero divisor"); }
        let magnitude = dividend.abs()/divisor.abs();
        let quotient = if (dividend<0) != (divisor<0) { -magnitude } else { magnitude };
        if quotient < min || quotient > max { return Err("quotient overflow"); }
        let remainder = dividend - quotient*divisor;
        assert!(remainder.abs() < divisor.abs());
        assert!(remainder == 0 || (remainder<0) == (dividend<0));
        if f.bits == 8 { expected.ax = ((remainder as u16 & 255)<<8) | (quotient as u16 & 255); }
        else { expected.ax=quotient as u16; expected.dx=remainder as u16; }
    } else {
        let (left,right) = if let Some(destination) = f.destination {
            if let Some(immediate) = f.immediate { (divisor,i128::from(immediate)) }
            else { (integer(u32::from(word(&r,destination)),f.bits,true),divisor) }
        } else { (integer(u32::from(r.ax),f.bits,f.signed),divisor) };
        let product=left*right;
        if let Some(destination) = f.destination { set_word(&mut expected,destination,product as u16); }
        else {
            expected.ax=product as u16;
            if f.bits==16 { expected.dx=(product.div_euclid(65536)) as u16; }
        }
        let defined = Flags::CF | Flags::OF;
        expected.flags=Flags::from_bits((r.flags.bits() & !defined) | if product<min || product>max {defined} else {0});
    }
    expected.ip = expected.ip.wrapping_add(f.bytes.len() as u16);
    Ok(expected)
}
fn check(m: &mut Machine, f: &Fixture, dividend: u32, source: u16, flags: u16) {
    m.regs=Registers {
        ax:dividend as u16, dx:if f.bits==8 {0x5aa5} else {(dividend>>16) as u16},
        bx:0xfff0, cx:0x9abc, si:0x41, di:0x34, bp:0xffd0, sp:0xfffe,
        cs:0x200, ds:0x6000, es:0x7000, ss:0x8000, ip:f.ip,
        flags:Flags::from_bits(flags),
    };
    if let Some(destination) = f.destination { set_word(&mut m.regs,destination,dividend as u16); }
    if f.source<8 {
        if f.bits==8 {
            let index=f.source%4;
            let shift=8*(f.source/4);
            let value=(word(&m.regs,index) & !(255<<shift)) | ((source & 255)<<shift);
            set_word(&mut m.regs,index,value);
        } else { set_word(&mut m.regs,f.source,source); }
    } else {
        let ea=address(&m.regs,f.source);
        // Avoid a pre-execution memory-boundary fault in arithmetic vectors;
        // a dedicated vector below checks that exact path separately.
        if f.bits==16 && ea.offset==0xffff { return; }
        if f.bits==8 { m.write8(ea,source as u8).unwrap(); }
        else { m.write16(ea,source).unwrap(); }
    }
    let before=m.regs;
    let steps=m.steps;
    let source=operand(m,f);
    let expected=oracle(before,f,source);
    let result=generated::step(m);
    match expected {
        Ok(expected)=>{
            assert!(result.is_ok(),"{:02x?} before={before:?}: {result:?}",f.bytes);
            assert_eq!(m.regs,expected,"{:02x?} before={before:?}",f.bytes);
        }
        Err(reason)=>{
            let fault=result.expect_err("divide should fault, not wrap or panic");
            assert!(fault.0.contains(reason),"{:02x?}: {fault}",f.bytes);
            assert!(fault.0.starts_with(&format!("0200:{:04x} ",f.ip)),"{fault}");
            assert_eq!(m.regs,before,"fault changed registers/flags");
        }
    }
    if f.source>=8 {
        let ea=address(&before,f.source);
        let after=if f.bits==8 {u16::from(m.read8(ea).unwrap())} else {m.read16(ea).unwrap()};
        assert_eq!(after,source,"multiply/divide must not write the source memory");
    }
    assert_eq!(m.steps,steps+1);
}
fn main() {
    let mut bytes=vec![0;96];
    for (i,w) in [0x5a4du16,96,1,0,2,0,0xffff,0,64,0,0,0,28,0].into_iter().enumerate() {bytes[i*2..i*2+2].copy_from_slice(&w.to_le_bytes());}
    let mut m=Machine::from_mz(&bytes,0x1010).unwrap();
    for f in FIXTURES { m.memory.write_bytes(SegmentedAddress::new(0x200,f.ip),f.bytes).unwrap(); }
    let dividends=[0,1,7,13,127,128,255,256,32767,32768,0xff7f,0xff80,0xfff3,65535,65536,65539,0x7fff_ffff,0x8000_0000,0xffff_0000,0xffff_7fff,0xffff_8000,0xffff_fff3,0xffff_ffff];
    let sources=[0,1,2,3,5,127,128,255,256,32767,32768,65535];
    let mut random=0xa187_03c2u32;
    for f in FIXTURES {
        for dividend in dividends {
            for source in sources {
                // Opposing initial CF/OF catches implementations that set but
                // never clear overflow, or preserve only one flag correctly.
                for flags in [0xa555,0x5aaa] { check(&mut m,f,dividend,source,flags); }
            }
        }
        for _ in 0..64 {
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);
            let dividend=random;
            random=random.wrapping_mul(1664525).wrapping_add(1013904223);
            check(&mut m,f,dividend,random as u16,(random>>16) as u16);
        }
    }
    // Exhaustive 8-bit input pairs for unsigned/signed MUL, using BL so AL
    // remains independent. The high AX byte and DX must not enter the product.
    for f in FIXTURES.iter().filter(|f| f.bits==8 && !f.division && f.source==3) {
        for a in 0..=255 { for b in 0..=255 { check(&mut m,f,0x5a00|a,b,0xffff); } }
    }
    // Explicit quotient/remainder placements and signed truncation examples.
    for (bits,signed,dividend,source,ax,dx) in [
        (8,false,0x103,2,0x0181,0x5aa5),
        (8,true,0xfff3,5,0xfdfe,0x5aa5),
        (8,true,13,0xfffb,0x03fe,0x5aa5),
        (16,false,0x10003,2,0x8001,1),
        (16,true,0xffff_fff3,5,0xfffe,0xfffd),
        (16,true,13,0xfffb,0xfffe,3),
    ] {
        let f=FIXTURES.iter().find(|f| f.bits==bits && f.signed==signed && f.division && f.source==3).unwrap();
        check(&mut m,f,dividend,source,0xd5a5);
        assert_eq!((m.regs.ax,m.regs.dx),(ax,dx));
    }
    // Full dividend MIN/-1 as well as narrow quotient MIN/-1 must return a
    // Fault. The native executable is built with overflow checks enabled.
    for (bits,dividend) in [(8,0x8000),(8,0xff80),(16,0x8000_0000),(16,0xffff_8000)] {
        let f=FIXTURES.iter().find(|f| f.bits==bits && f.signed && f.division && f.source==3).unwrap();
        check(&mut m,f,dividend,0xffff,0xffff);
        assert_eq!(m.regs.ip,f.ip);
    }
    // Read faults propagate before register or flag writes, for each mnemonic.
    for f in FIXTURES.iter().filter(|f| f.bits==16 && f.source==8 && f.destination.is_none()) {
        m.regs=Registers {cs:0x200,ip:f.ip,ds:0x6000,bx:0xffff,ax:7,dx:3,flags:Flags::from_bits(0xffff),..Registers::default()};
        let before=m.regs;
        assert!(generated::step(&mut m).is_err());
        assert_eq!(m.regs,before);
    }
    println!("MUL/IMUL/DIV/IDIV wide-oracle execution passed: {} encodings, {} steps (no decoder linked)",FIXTURES.len(),m.steps);
}
"#);
        std::fs::write(dir.join("main.rs"), program).unwrap();
        let compiler = runtime_fixture_rustc()
            .args(["--edition=2024", "-Dwarnings", "-C", "overflow-checks=yes"])
            .arg(dir.join("main.rs"))
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
        assert!(output.contains("wide-oracle execution passed"), "{output}");
        print!("{output}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_ss_segment_override_and_effective_address_wrap() {
        let g = recover(
            &[0x8b, 0x42, 0xfe, 0x26, 0x8b, 0x42, 0xfe, 0xc3],
            &[at(0)],
            10,
        )
        .unwrap();
        let text = emit_rust(&g).unwrap();
        assert!(text.contains("SegmentedAddress::new(m.regs.ss"));
        assert!(text.contains("SegmentedAddress::new(m.regs.es"));
        assert!(text.contains("wrapping_add(m.regs.bp)"));
        assert!(text.contains("wrapping_add(m.regs.si)"));
    }
}
