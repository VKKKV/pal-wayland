// SPDX-License-Identifier: GPL-3.0-or-later
//! Build-time only: emits an inspectable Rust project, never compiles/runs it.
use pal_wayland::{
    lift::{
        Address, Graph, emit_rust, emit_rust_variants, recover_borland_in_ranges, recover_in_ranges,
    },
    runtime::Machine,
};
use std::{
    collections::BTreeSet,
    env, fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
};

// Views are caller-confirmed physical memory, not runtime snapshots or patches.
// The caller must retain input hashes/source provenance and verify header/code bounds.
const USAGE: &str = "usage: pal-wayland-lift <MZ-exe> <NEW-output-directory> [--entry CS:IP] [--seed CS:IP] [--max-instructions N] [--near-table CS:OFFSET:decimal-count] [--mutable-immediate CS:IP] [--fpu-macros borland|none] [--static-view PATH --view-seed CS:IP --view-range HEXSTART:HEXEND ...]\nEach static view is exactly 1 MiB physical memory and requires its own seeds and nonempty, disjoint, end-exclusive hexadecimal ranges. --view-seed/--view-range bind to the most recent --static-view; --seed/--near-table remain original-MZ-only. --entry selects the original runtime entry, not a view root. Global mutable-immediate opt-ins apply to every recovered occurrence; conflicts reject exemptions. Views are analysis-only: never executed, embedded, or applied to runtime memory. Caller must verify input hashes/source provenance and header/code bounds.";

#[derive(Debug)]
struct StaticView {
    path: PathBuf,
    seeds: Vec<Address>,
    ranges: Vec<Range<usize>>,
}
fn view_range(value: &str) -> Result<Range<usize>, String> {
    let (start, end) = value
        .split_once(':')
        .ok_or("expected hexadecimal view range PHYSICALSTART:PHYSICALEND")?;
    let hex = |s: &str| {
        usize::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
            .map_err(|_| "invalid hexadecimal view range".to_string())
    };
    let range = hex(start)?..hex(end)?;
    if range.start >= range.end || range.end > 0x100000 {
        return Err("view range must be nonempty and inside 1 MiB physical memory".into());
    }
    Ok(range)
}
fn map_graph(mapping: &mut String, graph: &Graph) {
    for (at, node) in &graph.nodes {
        mapping.push_str(&format!("{:04x}:{:04x} {node:?}\n", at.cs, at.ip));
    }
    for unresolved in &graph.unresolved {
        mapping.push_str(&format!("UNRESOLVED {unresolved}\n"));
    }
}

fn address(value: &str) -> Result<Address, String> {
    let (cs, ip) = value.split_once(':').ok_or("expected hexadecimal CS:IP")?;
    Ok(Address {
        cs: u16::from_str_radix(cs, 16).map_err(|e| e.to_string())?,
        ip: u16::from_str_radix(ip, 16).map_err(|e| e.to_string())?,
    })
}
fn create(path: &Path, content: &str) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(content.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}
fn run() -> Result<(), String> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() < 2 {
        return Err(USAGE.into());
    }
    let input = fs::canonicalize(&args[0]).map_err(|e| e.to_string())?;
    let data = fs::read(&input).map_err(|e| e.to_string())?;
    let m = Machine::from_mz(&data, 0x1010).map_err(|e| e.to_string())?;
    let mut entry = Address {
        cs: m.regs.cs,
        ip: m.regs.ip,
    };
    let mut seeds = Vec::new();
    let mut max = 100_000;
    let mut borland = false;
    let mut near_tables = Vec::new();
    let mut mutable = Vec::new();
    let mut views = Vec::<StaticView>::new();
    let mut opts = args[2..].chunks_exact(2);
    for pair in opts.by_ref() {
        match pair[0].as_str() {
            "--entry" => entry = address(&pair[1])?,
            "--seed" => seeds.push(address(&pair[1])?),
            "--mutable-immediate" => mutable.push(address(&pair[1])?),
            "--static-view" => views.push(StaticView {
                path: fs::canonicalize(&pair[1])
                    .map_err(|e| format!("static view {}: {e}", pair[1]))?,
                seeds: Vec::new(),
                ranges: Vec::new(),
            }),
            "--view-seed" => views
                .last_mut()
                .ok_or("--view-seed requires a preceding --static-view")?
                .seeds
                .push(address(&pair[1])?),
            "--view-range" => views
                .last_mut()
                .ok_or("--view-range requires a preceding --static-view")?
                .ranges
                .push(view_range(&pair[1])?),
            "--near-table" => {
                let (base, count) = pair[1]
                    .rsplit_once(':')
                    .ok_or("expected CS:OFFSET:decimal-count")?;
                let base = address(base)?;
                let count: usize = count.parse().map_err(|_| "invalid near-table count")?;
                if count == 0 || count > 32768 || usize::from(base.ip) + count * 2 > 65536 {
                    return Err("near-table outside segment or count range".into());
                }
                near_tables.push((base, count));
            }
            "--fpu-macros" => {
                borland = match pair[1].as_str() {
                    "borland" => true,
                    "none" => false,
                    _ => return Err("fpu-macros must be borland or none".into()),
                }
            }
            "--max-instructions" => {
                max = pair[1].parse().map_err(|_| "invalid max-instructions")?
            }
            _ => return Err(format!("unknown option {}", pair[0])),
        }
    }
    if !opts.remainder().is_empty() {
        return Err("option missing value".into());
    }
    seeds.insert(0, entry);
    let mz = pal_wayland::mz::MzFile::parse(&data).map_err(|e| e.to_string())?;
    let image_start = usize::from(m.load_segment) * 16;
    let range = image_start..image_start + mz.image().len();
    let recover = if borland {
        recover_borland_in_ranges
    } else {
        recover_in_ranges
    };
    for (base, count) in &near_tables {
        if base.physical() < range.start || base.physical() + count * 2 > range.end {
            return Err("near-table outside MZ image".into());
        }
        for index in 0..*count {
            let offset = m
                .read16(pal_wayland::semantics::SegmentedAddress::new(
                    base.cs,
                    base.ip + (index as u16) * 2,
                ))
                .map_err(|e| e.to_string())?;
            let target = Address {
                cs: base.cs,
                ip: offset,
            };
            if !range.contains(&target.physical()) {
                return Err(format!("near-table target {target} outside MZ image"));
            }
            seeds.push(target);
        }
    }
    let mut graphs = vec![recover(
        m.memory.as_bytes(),
        &seeds,
        max,
        std::slice::from_ref(&range),
    )?];
    for view in &mut views {
        if view.seeds.is_empty() {
            return Err(format!(
                "static view {} requires at least one --view-seed",
                view.path.display()
            ));
        }
        view.ranges.sort_by_key(|r| r.start);
        if view.ranges.is_empty() || view.ranges.windows(2).any(|w| w[0].end > w[1].start) {
            return Err(format!(
                "static view {} requires nonempty, disjoint --view-range bounds",
                view.path.display()
            ));
        }
        for seed in &view.seeds {
            if !view.ranges.iter().any(|r| r.contains(&seed.physical())) {
                return Err(format!(
                    "static view {} seed {seed} outside confirmed view ranges",
                    view.path.display()
                ));
            }
        }
        let bytes = fs::read(&view.path)
            .map_err(|e| format!("static view {}: {e}", view.path.display()))?;
        if bytes.len() != m.memory.as_bytes().len() {
            return Err(format!(
                "static view {} must be exactly 1 MiB physical memory (got {} bytes)",
                view.path.display(),
                bytes.len()
            ));
        }
        graphs.push(
            recover(&bytes, &view.seeds, max, &view.ranges)
                .map_err(|e| format!("static view {}: {e}", view.path.display()))?,
        );
    }
    // Apply global opt-ins consistently across occurrences. Never silently drop
    // an exemption to bypass the multi-view emitter's strict conflict checks.
    for target in mutable {
        let mut found = false;
        for graph in &mut graphs {
            if let Some(node) = graph.nodes.get_mut(&target) {
                found = true;
                if node.borland.is_some() {
                    return Err("FPU macro is not a writable MOV immediate".into());
                }
                node.mutable_immediate = Some(pal_wayland::mutable_immediate::analyze(
                    &node.instruction,
                    &node.bytes,
                )?);
            }
        }
        if !found {
            return Err(format!("mutable target {target} not in recovered code"));
        }
    }
    // Preserve original-only behavior; the strict emitter owns multi-view merge.
    let generated = if views.is_empty() {
        emit_rust(&graphs[0])?
    } else {
        emit_rust_variants(&graphs)?
    };
    let addresses: BTreeSet<_> = graphs
        .iter()
        .flat_map(|g| g.nodes.keys().copied())
        .collect();
    let variants: BTreeSet<_> = graphs
        .iter()
        .flat_map(|g| g.nodes.iter().map(|(at, n)| (*at, n.bytes.clone())))
        .collect();
    let unresolved: BTreeSet<_> = graphs.iter().flat_map(|g| g.unresolved.iter()).collect();
    let mut mapping =
        format!("Analysis arguments: {args:?}\nConfirmed seed/table roots: {seeds:?}\n");
    mapping.push_str(&format!(
        "VIEW 0 original path={input:?} ranges={:?} roots={seeds:?}\n",
        std::slice::from_ref(&range)
    ));
    map_graph(&mut mapping, &graphs[0]);
    for (index, view) in views.iter().enumerate() {
        mapping.push_str(&format!(
            "VIEW {} static path={:?} ranges={:?} roots={:?}\n",
            index + 1,
            view.path,
            view.ranges,
            view.seeds
        ));
        map_graph(&mut mapping, &graphs[index + 1]);
    }
    mapping.push_str(&format!(
        "TOTAL static_instructions={} static_variants={} unresolved_edges={} static_views={}\n",
        addresses.len(),
        variants.len(),
        unresolved.len(),
        graphs.len()
    ));
    // Creation is deliberate and exclusive: never replace source/assets or an
    // older generated project. All analysis finishes before filesystem writes.
    let out = Path::new(&args[1]);
    fs::create_dir(out).map_err(|e| format!("new output directory {}: {e}", out.display()))?;
    fs::create_dir(out.join("src")).map_err(|e| e.to_string())?;
    let manifest = format!(
        "[package]\nname = \"pal-wayland\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[features]\nwindow=[\"pal-wayland/window\"]\nprofiling=[\"pal-wayland/profiling\"]\n\n[dependencies]\npal-wayland = {{ path = {:?}, default-features = false }}\n",
        env!("CARGO_MANIFEST_DIR")
    );
    create(&out.join("Cargo.toml"), &manifest)?;
    create(&out.join("src/translated.rs"), &generated)?;
    let main = include_str!("../../templates/native_main.rs.in")
        .replace("@ENTRY_CS@", &entry.cs.to_string())
        .replace("@ENTRY_IP@", &entry.ip.to_string());
    create(&out.join("src/main.rs"), &main)?;
    create(&out.join("source-map.txt"), &mapping)?;
    create(
        &out.join("SCOPE.txt"),
        &format!(
            "Input: {}\nLoad segment: 1010\nEntry: {:04x}:{:04x}\nStatic instructions: {}\nUnresolved edges: {}\nStatic variants: {}\nStatic views: {}\nCounts deduplicate architectural CS:IP, exact-byte cases and unresolved diagnostics across views.\nStatic view paths/ranges/roots are in source-map.txt. Views are exactly 1 MiB physical memory; ranges are end-exclusive physical bounds and roots are explicit architectural CS:IP.\nCaller must verify input hashes/source provenance and image/header code bounds; paths are not content identities. Views are build-time analysis only, never runtime snapshots, guest patches, executed code, or embedded assets.\nNo runtime decoder. Unknown instructions, targets, services and modified code fail.\nLocal derivative only: do not redistribute without rights. Not certified playable.\n",
            input.display(),
            entry.cs,
            entry.ip,
            addresses.len(),
            unresolved.len(),
            variants.len(),
            graphs.len()
        ),
    )?;
    println!(
        "generated_directory: {}",
        fs::canonicalize(out).map_err(|e| e.to_string())?.display()
    );
    println!(
        "static_instructions: {}\nunresolved_edges: {}\nstatic_variants: {}\nstatic_views: {}",
        addresses.len(),
        unresolved.len(),
        variants.len(),
        graphs.len()
    );
    println!(
        "Build separately: cargo build --offline --manifest-path {:?}",
        out.join("Cargo.toml")
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("pal-wayland-lift: {e}");
        std::process::exit(1);
    }
}
