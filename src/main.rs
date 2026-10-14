// SPDX-License-Identifier: GPL-3.0-or-later
#![forbid(unsafe_code)]

use pal_wayland::mz::MzFile;
use std::{env, ffi::OsString, fs, process::ExitCode};

const USAGE: &str = "Usage:
  pal-wayland-tool inspect <exe>
  pal-wayland-tool load-check <exe> <load-segment>

load-segment is decimal or 0x-prefixed hexadecimal (for example 0x1000).
Static inspection only: no guest instructions or DOS services are executed.
load-check allocates image + minimum extra paragraphs; no PSP or DOS environment.";

fn parse_segment(value: &OsString) -> Result<u16, String> {
    let text = value.to_str().ok_or("load segment must be ASCII")?;
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => text.parse::<u16>(),
    };
    parsed
        .map_err(|_| format!("invalid load segment {text:?}; expected 0..65535 or 0x0000..0xffff"))
}

fn run(args: &[OsString]) -> Result<(), String> {
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let command = args.first().and_then(|arg| arg.to_str());
    let segment = match command {
        Some("inspect") if args.len() == 2 => None,
        Some("load-check") if args.len() == 3 => Some(parse_segment(&args[2])?),
        _ => return Err(USAGE.to_owned()),
    };
    let data = fs::read(&args[1]).map_err(|err| format!("read {:?}: {err}", args[1]))?;
    let mz = MzFile::parse(&data).map_err(|err| format!("MZ: {err}"))?;
    // Complete all fallible checks before emitting a success report on stdout.
    let loaded = segment
        .map(|segment| mz.load(segment))
        .transpose()
        .map_err(|err| format!("load: {err}"))?;
    println!("actual_file_bytes: {}", data.len());
    println!("declared_file_bytes: {}", mz.declared_file_bytes());
    println!(
        "trailing_file_bytes: {}",
        data.len() - mz.declared_file_bytes()
    );
    println!("header_bytes: {}", mz.header_bytes());
    println!("image_bytes: {}", mz.image().len());
    println!("relocations: {}", mz.relocations().len());
    println!(
        "entry_relative: {:04x}:{:04x}",
        mz.header().cs,
        mz.header().ip
    );
    println!("entry_image_offset: {:#x}", mz.entry_image_offset());
    println!("entry_file_offset: {:#x}", mz.entry_file_offset());
    println!(
        "stack_relative: {:04x}:{:04x}",
        mz.header().ss,
        mz.header().sp
    );
    println!("min_extra_paragraphs: {}", mz.header().min_extra_paragraphs);
    println!("max_extra_paragraphs: {}", mz.header().max_extra_paragraphs);
    if let Some(loaded) = loaded {
        println!("load_segment: {:#06x}", loaded.load_segment);
        println!("allocation_bytes: {}", loaded.memory.len());
        println!("entry_loaded: {:04x}:{:04x}", loaded.cs, loaded.ip);
        println!("stack_loaded: {:04x}:{:04x}", loaded.ss, loaded.sp);
        println!("applied_relocations: {}", loaded.relocation_count);
        println!("scope: deterministic minimum-memory image load; no PSP, DOS, or guest execution");
    } else {
        println!("scope: static MZ inspection; no code recovery or guest execution");
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(&env::args_os().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pal-wayland-tool: {error}");
            ExitCode::FAILURE
        }
    }
}
