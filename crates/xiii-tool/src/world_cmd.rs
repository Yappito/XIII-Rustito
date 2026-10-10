//! World/asset decoding commands (M2a): texture, static-mesh, BSP and terrain decoders from
//! `xiii-decode`, applied to single exports or the whole installation.
//!
//! Output written by these commands (PNG/OBJ dumps) contains decoded game content: write it
//! outside the repository and the installation (the commands refuse the installation).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use xiii_decode::common::DecodeError;
use xiii_decode::static_mesh::{STATIC_MESH_CLASS, decode_static_mesh};
use xiii_decode::texture::{PALETTE_CLASS, TEXTURE_CLASS, decode_palette, decode_texture};
use xiii_decode::{model, terrain};
use xiii_package::{Limits, ObjectRef, Package, Property, PropertyValue};

use crate::corpus::tagged_files;
use crate::props::find_export;

/// Subcommands handled here.
pub const COMMANDS: &[&str] = &[
    "world-coverage",
    "texture",
    "mesh",
    "bsp",
    "zones",
    "terrain",
    "material-survey",
    "nav",
    "box-probe",
];

/// Usage text appended to the main help.
pub const USAGE: &str = "\
  xiii-tool world-coverage <install-root> [--classes texture,staticmesh,model,polys,terrain]
      Decode every Texture/Palette/StaticMesh/Model/Polys/TerrainInfo/TerrainSector export
      of every package (texture pixels of every mip included) and print per-class and
      per-format counts with the first failure of each kind (path, offset).

  xiii-tool texture <package-file> --export <index|path> [--png <out.png>] [--mip N]
      Decode one texture; optionally write a mip as PNG (decoded game content: keep local).

  xiii-tool mesh <package-file> --export <index|path> [--obj <out.obj>]
      Decode one static mesh and print its layout summary; optionally write OBJ (source
      coordinates, source winding).

  xiii-tool bsp <map-file> [--export <index|path>] [--region x0,y0,z0,x1,y1,z1] [--point x,y,z]
      Decode the level BSP model (or the given Model) and its surfaces. --region lists every
      node polygon whose bounds touch the source-space box (node, node flags, surface,
      PolyFlags, material, plane, vertices). --point prints the leaf/zone reached by the BSP
      walk and the CSG side sequence of the nodes on the path (diagnostic). --grid
      x0,y0,x1,y1,z,step prints a top-down map of the zone reached at height z (rows +Y
      downward... see the legend line; '#' = solid, '.' = no zone).

  xiii-tool zones <map-file> [--export <index|path>]
      Decode the level BSP model (or the given Model) and list its zones: index, ZoneActor
      export path and class, whether the zone is a sky zone, per-zone leaf count (derived
      from the nodes' iLeaf/iZone pairs) and the connectivity/visibility masks.

  xiii-tool nav <map-name> --game-dir <install-root>
      Decode the map's NavigationPoints and ReachSpec edges and list them (index, class,
      name, location; per edge: start/end index, CollisionRadius/Height, reach flags,
      distance), followed by the connected components over all edges.

  xiii-tool box-probe <map-name> --game-dir <install-root> --from x,y,z --to x,y,z
                     [--half radius,halfheight] [--samples N]
      Import the map's box collision soup (the static set the player sim starts from) and
      move an axis-aligned box (default 34,75: the player) from --from to --to (Unreal
      units). Prints the first swept hit, then every distinct triangle the box overlaps at N
      evenly spaced centres (default 64) with its source path, Unreal-space vertices and
      normal.

  xiii-tool terrain <map-file> --game-dir <install-root>
      Decode TerrainInfo/TerrainSector exports and their heightmap.

  xiii-tool material-survey <install-root> [--maps Plage00,Plage01,Banque01]
      Count UE2 material classes referenced by placed geometry (BSP surfaces, static mesh
      sections, terrain layers) per map and corpus-wide, and tabulate the tagged properties
      in use per class with their value ranges. Also prints the reflected blend enums.";

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn emit(text: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

/// Dispatches one of [`COMMANDS`].
pub fn run(cmd: &str, args: &[String]) -> ExitCode {
    match cmd {
        "world-coverage" => coverage_cmd(args),
        "texture" => texture_cmd(args),
        "mesh" => mesh_cmd(args),
        "bsp" => bsp_cmd(args),
        "zones" => zones_cmd(args),
        "terrain" => terrain_cmd(args),
        "material-survey" => material_survey_cmd(args),
        "nav" => nav_cmd(args),
        "box-probe" => box_probe_cmd(args),
        _ => usage_error(&format!("unknown command '{cmd}'")),
    }
}

struct Args {
    positional: Vec<String>,
    options: BTreeMap<String, String>,
}

fn parse_args(args: &[String], valued: &[&str]) -> Result<Args, String> {
    let mut out = Args {
        positional: Vec::new(),
        options: BTreeMap::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(name) = a.strip_prefix("--") {
            if valued.contains(&name) {
                let v = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
                out.options.insert(name.to_owned(), v.clone());
            } else {
                return Err(format!("unknown option '{a}'"));
            }
        } else {
            out.positional.push(a.clone());
        }
    }
    Ok(out)
}

/// Reads and parses a package file.
pub fn load_package(path: &Path) -> Result<(Vec<u8>, Package), String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let package = Package::parse(&data, &Limits::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((data, package))
}

fn export_path(p: &Package, i: usize) -> String {
    p.object_path(ObjectRef::Export(i as u32))
        .unwrap_or("?")
        .to_owned()
}

// ---------------------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------------------

/// Per-class decode statistics.
#[derive(Debug, Clone, Default)]
pub struct ClassCoverage {
    /// Exports with a payload.
    pub attempted: u64,
    /// Decoded to the exact payload end.
    pub ok: u64,
    /// Failures by category.
    pub failures: BTreeMap<String, u64>,
    /// First failure text per category.
    pub examples: BTreeMap<String, String>,
    /// Payload bytes of decoded exports.
    pub bytes: u64,
    /// Bytes inside consumed-but-unexplained ranges.
    pub unknown_bytes: u64,
    /// Decoded exports that end with an explicit unsupported tail (counted in `ok`).
    pub partial: u64,
    /// Bytes in unsupported tails.
    pub unsupported_tail_bytes: u64,
}

impl ClassCoverage {
    fn fail(&mut self, package: &str, e: &DecodeError) {
        let cat = e.category();
        *self.failures.entry(cat.clone()).or_default() += 1;
        self.examples
            .entry(cat)
            .or_insert_with(|| format!("{package}: {e}"));
    }

    /// Total failures.
    pub fn failed(&self) -> u64 {
        self.failures.values().sum()
    }
}

/// Whole-installation coverage of the world decoders.
#[derive(Debug, Clone, Default)]
pub struct WorldCoverage {
    /// Packages parsed.
    pub packages: u64,
    /// Package parse errors.
    pub package_errors: Vec<String>,
    /// Per class path.
    pub classes: BTreeMap<String, ClassCoverage>,
    /// Texture format -> (header decoded, all mips decoded to RGBA, pixel decode failures).
    pub texture_formats: BTreeMap<String, [u64; 3]>,
    /// Texture pixel failure examples by format.
    pub texture_pixel_examples: BTreeMap<String, String>,
    /// Texture trailing-byte triples (licensee >= 55): distinct values seen.
    pub texture_trailing_distinct: u64,
    /// Static mesh totals: vertices, triangles, collision triangles (set 0), meshes with a
    /// simplified collision set, raw triangles.
    pub mesh_totals: [u64; 5],
    /// Static mesh winding: triangles against / along stored normals / degenerate.
    pub mesh_winding: [u64; 3],
    /// Extra counters (free-form, from model/terrain decoders).
    pub counters: BTreeMap<String, u64>,
}

/// Which classes to decode.
#[derive(Debug, Clone, Copy)]
pub struct ClassSelection {
    /// Texture + Palette.
    pub texture: bool,
    /// StaticMesh.
    pub static_mesh: bool,
    /// Model.
    pub model: bool,
    /// Polys.
    pub polys: bool,
    /// TerrainInfo / TerrainSector.
    pub terrain: bool,
}

impl ClassSelection {
    /// Everything.
    pub fn all() -> Self {
        Self {
            texture: true,
            static_mesh: true,
            model: true,
            polys: true,
            terrain: true,
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        let mut out = Self {
            texture: false,
            static_mesh: false,
            model: false,
            polys: false,
            terrain: false,
        };
        for part in s.split(',') {
            match part.trim().to_ascii_lowercase().as_str() {
                "texture" => out.texture = true,
                "staticmesh" => out.static_mesh = true,
                "model" => out.model = true,
                "polys" => out.polys = true,
                "terrain" => out.terrain = true,
                "all" => return Ok(Self::all()),
                other => return Err(format!("unknown class group '{other}'")),
            }
        }
        Ok(out)
    }
}

/// Decodes the selected classes in every package below `root`.
pub fn scan_world(root: &Path, sel: ClassSelection) -> std::io::Result<WorldCoverage> {
    let mut cov = WorldCoverage::default();
    let mut trailing = std::collections::BTreeSet::new();
    for (rel, path) in tagged_files(root)? {
        let (data, package) = match load_package(&path) {
            Ok(v) => v,
            Err(e) => {
                cov.package_errors.push(e);
                continue;
            }
        };
        cov.packages += 1;
        scan_package(&rel, &data, &package, sel, &mut cov, &mut trailing);
    }
    cov.texture_trailing_distinct = trailing.len() as u64;
    Ok(cov)
}

fn scan_package(
    rel: &str,
    data: &[u8],
    package: &Package,
    sel: ClassSelection,
    cov: &mut WorldCoverage,
    trailing: &mut std::collections::BTreeSet<[u8; 3]>,
) {
    for i in 0..package.exports().len() {
        if package.exports()[i].serial_size == 0 {
            continue;
        }
        let class = package.export_class_path(i).unwrap_or("?");
        let bytes = u64::from(package.exports()[i].serial_size);
        macro_rules! tally {
            ($result:expr, $ok:expr) => {{
                let entry = cov.classes.entry(class.to_owned()).or_default();
                entry.attempted += 1;
                match $result {
                    Ok(v) => {
                        let entry = cov.classes.get_mut(class).expect("inserted");
                        entry.ok += 1;
                        entry.bytes += bytes;
                        #[allow(clippy::redundant_closure_call)]
                        ($ok)(v, &mut *cov)
                    }
                    Err(e) => entry.fail(rel, &e),
                }
            }};
        }
        if sel.texture && class.eq_ignore_ascii_case(TEXTURE_CLASS) {
            tally!(
                decode_texture(package, data, i),
                |t: xiii_decode::texture::Texture, cov: &mut WorldCoverage| {
                    cov.classes.get_mut(class).expect("x").unknown_bytes +=
                        t.report.unknown_bytes() as u64;
                    if let Some(tr) = t.trailing {
                        trailing.insert(tr);
                    }
                    *cov.counters.entry("texture.empty_mips".into()).or_default() +=
                        t.empty_mips() as u64;
                    if t.mips.first().is_none_or(|m| m.data.is_empty()) {
                        *cov.counters
                            .entry("texture.missing_mip0".into())
                            .or_default() += 1;
                    }
                    let f = t.format.name();
                    let slot = cov.texture_formats.entry(f.clone()).or_default();
                    slot[0] += 1;
                    match t.decode_all() {
                        Ok(_) => slot[1] += 1,
                        Err(e) => {
                            slot[2] += 1;
                            cov.texture_pixel_examples.entry(f).or_insert_with(|| {
                                format!("{rel}: {} {}", export_path(package, i), e)
                            });
                        }
                    }
                }
            );
        } else if sel.texture && class.eq_ignore_ascii_case(PALETTE_CLASS) {
            tally!(
                decode_palette(package, data, i),
                |_p, _c: &mut WorldCoverage| {}
            );
        } else if sel.static_mesh && class.eq_ignore_ascii_case(STATIC_MESH_CLASS) {
            tally!(
                decode_static_mesh(package, data, i),
                |m: xiii_decode::static_mesh::StaticMesh, cov: &mut WorldCoverage| {
                    cov.classes.get_mut(class).expect("x").unknown_bytes +=
                        m.report.unknown_bytes() as u64;
                    cov.mesh_totals[0] += m.vertices.len() as u64;
                    cov.mesh_totals[1] += (m.indices.len() / 3) as u64;
                    cov.mesh_totals[2] += m.collision[0].triangles.len() as u64;
                    cov.mesh_totals[3] += u64::from(!m.collision[1].triangles.is_empty());
                    cov.mesh_totals[4] += m.raw_triangles.len() as u64;
                    let (a, b, c) = m.winding_statistics();
                    cov.mesh_winding[0] += a as u64;
                    cov.mesh_winding[1] += b as u64;
                    cov.mesh_winding[2] += c as u64;
                }
            );
        } else if sel.model && class.eq_ignore_ascii_case(model::MODEL_CLASS) {
            tally!(
                model::decode_model(package, data, i),
                |m: model::Model, cov: &mut WorldCoverage| {
                    let c = cov.classes.get_mut(class).expect("x");
                    c.unknown_bytes += m.report.unknown_bytes() as u64;
                    if let Some((_, span)) = m.report.unsupported_tail {
                        c.partial += 1;
                        c.unsupported_tail_bytes += span.len() as u64;
                    }
                    for (k, v) in m.counters() {
                        *cov.counters.entry(format!("model.{k}")).or_default() += v;
                    }
                }
            );
        } else if sel.polys && class.eq_ignore_ascii_case(model::POLYS_CLASS) {
            tally!(
                model::decode_polys(package, data, i),
                |p: model::Polys, cov: &mut WorldCoverage| {
                    *cov.counters.entry("polys.polygons".into()).or_default() +=
                        p.polys.len() as u64;
                }
            );
        } else if sel.terrain && class.eq_ignore_ascii_case(terrain::TERRAIN_SECTOR_CLASS) {
            tally!(
                terrain::decode_sector(package, data, i),
                |s: terrain::TerrainSector, cov: &mut WorldCoverage| {
                    cov.classes.get_mut(class).expect("x").unknown_bytes +=
                        s.report.unknown_bytes() as u64;
                }
            );
        } else if sel.terrain && class.eq_ignore_ascii_case(terrain::TERRAIN_INFO_CLASS) {
            tally!(
                terrain::decode_terrain_info(package, data, i),
                |t: terrain::TerrainInfo, cov: &mut WorldCoverage| {
                    let c = cov.classes.get_mut(class).expect("x");
                    c.unknown_bytes += t.report.unknown_bytes() as u64;
                    if let Some((_, span)) = t.report.unsupported_tail {
                        c.partial += 1;
                        c.unsupported_tail_bytes += span.len() as u64;
                    }
                }
            );
        }
    }
}

/// Plain-text report.
pub fn coverage_text(cov: &WorldCoverage) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "packages {} (parse errors {})",
        cov.packages,
        cov.package_errors.len()
    );
    for e in &cov.package_errors {
        let _ = writeln!(out, "  PACKAGE ERROR {e}");
    }
    let _ = writeln!(
        out,
        "{:<26} {:>8} {:>8} {:>8} {:>12} {:>10} {:>8} {:>10}",
        "class", "attempt", "ok", "failed", "bytes(ok)", "unknownB", "partial", "tailB"
    );
    for (name, c) in &cov.classes {
        let _ = writeln!(
            out,
            "{:<26} {:>8} {:>8} {:>8} {:>12} {:>10} {:>8} {:>10}",
            name,
            c.attempted,
            c.ok,
            c.failed(),
            c.bytes,
            c.unknown_bytes,
            c.partial,
            c.unsupported_tail_bytes
        );
        for (cat, n) in &c.failures {
            let _ = writeln!(out, "    {n:>6} {cat}");
            if let Some(ex) = c.examples.get(cat) {
                let _ = writeln!(out, "           e.g. {ex}");
            }
        }
    }
    if !cov.texture_formats.is_empty() {
        let _ = writeln!(
            out,
            "texture formats (headers decoded / all mips to RGBA8 / pixel failures):"
        );
        for (f, [a, b, c]) in &cov.texture_formats {
            let _ = writeln!(out, "  {f:<16} {a:>6} {b:>6} {c:>6}");
            if let Some(ex) = cov.texture_pixel_examples.get(f) {
                let _ = writeln!(out, "      e.g. {ex}");
            }
        }
        let _ = writeln!(
            out,
            "texture trailing 3-byte values: {} distinct",
            cov.texture_trailing_distinct
        );
    }
    if cov.mesh_totals.iter().any(|&v| v != 0) {
        let t = cov.mesh_totals;
        let _ = writeln!(
            out,
            "static meshes: vertices {}, render triangles {}, collision triangles {}, meshes with simplified collision {}, raw triangles {}",
            t[0], t[1], t[2], t[3], t[4]
        );
        let w = cov.mesh_winding;
        let _ = writeln!(
            out,
            "static mesh winding (source coords, (b-a)x(c-a) vs stored normals): against {}, along {}, degenerate {}",
            w[0], w[1], w[2]
        );
    }
    for (k, v) in &cov.counters {
        let _ = writeln!(out, "  {k}: {v}");
    }
    out
}

fn coverage_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["classes"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(root) = a.positional.first().map(PathBuf::from) else {
        return usage_error("world-coverage needs an installation root");
    };
    let sel = match a
        .options
        .get("classes")
        .map(|s| ClassSelection::parse(s))
        .transpose()
    {
        Ok(s) => s.unwrap_or_else(ClassSelection::all),
        Err(e) => return usage_error(&e),
    };
    let started = Instant::now();
    let cov = match scan_world(&root, sel) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: scanning {} failed: {e}", root.display());
            return ExitCode::from(2);
        }
    };
    let mut out = format!(
        "world-coverage: {} ({:.1} s)\n",
        root.display(),
        started.elapsed().as_secs_f64()
    );
    out.push_str(&coverage_text(&cov));
    emit(&out);
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------------------
// Single-export commands
// ---------------------------------------------------------------------------------------

fn select(args: &Args, package: &Package, file: &str) -> Result<usize, ExitCode> {
    let Some(sel) = args.options.get("export") else {
        return Err(usage_error("--export is required"));
    };
    find_export(package, sel).ok_or_else(|| {
        eprintln!("error: no export '{sel}' in {file}");
        ExitCode::from(1)
    })
}

fn texture_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["export", "png", "mip"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = a.positional.first() else {
        return usage_error("texture needs a package file");
    };
    let (data, package) = match load_package(Path::new(file)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let i = match select(&a, &package, file) {
        Ok(i) => i,
        Err(c) => return c,
    };
    let t = match decode_texture(&package, &data, i) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} format {}{} size {}x{} masked {} alpha {} clamp {:?} palette {} trailing {:?} unknown {:?}",
        export_path(&package, i),
        t.format.name(),
        if t.format_defaulted { " (default)" } else { "" },
        t.size[0],
        t.size[1],
        t.masked,
        t.alpha_texture,
        t.clamp,
        t.palette.as_ref().map_or(0, Vec::len),
        t.trailing,
        t.report.unknown
    );
    for (k, m) in t.mips.iter().enumerate() {
        let _ = writeln!(
            out,
            "  mip {k}: {}x{} bits {}/{} {} bytes at 0x{:x}",
            m.width,
            m.height,
            m.ubits,
            m.vbits,
            m.data.len(),
            m.data_offset
        );
    }
    emit(&out);
    if let Some(png) = a.options.get("png") {
        let mip: usize = a
            .options
            .get("mip")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        match t.decode_mip(mip) {
            Ok(img) => {
                if let Err(e) = write_png(Path::new(png), img.width, img.height, &img.pixels) {
                    eprintln!("error: {e}");
                    return ExitCode::from(2);
                }
                println!("wrote {png}");
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

fn mesh_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["export", "obj"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = a.positional.first() else {
        return usage_error("mesh needs a package file");
    };
    let (data, package) = match load_package(Path::new(file)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let i = match select(&a, &package, file) {
        Ok(i) => i,
        Err(c) => return c,
    };
    let m = match decode_static_mesh(&package, &data, i) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} ({} payload bytes)",
        export_path(&package, i),
        m.report.payload.len()
    );
    let _ = writeln!(
        out,
        "  bounds {:?}..{:?} sphere {:?} xiii {:?}/{}",
        m.primitive.bounding_box.min,
        m.primitive.bounding_box.max,
        m.primitive.bounding_sphere,
        m.primitive.xiii_bytes,
        m.primitive.xiii_float
    );
    let _ = writeln!(
        out,
        "  vertices {} uv streams {} indices {} wire {} sections {}",
        m.vertices.len(),
        m.uv_streams.len(),
        m.indices.len(),
        m.wireframe_indices.len(),
        m.sections.len()
    );
    for (k, s) in m.sections.iter().enumerate() {
        let mat = m
            .materials
            .get(k)
            .map(|mm| {
                format!(
                    "{} coll={} u={}",
                    crate::props::ref_text(&package, mm.material),
                    mm.enable_collision,
                    mm.unknown
                )
            })
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(out, "    section {k}: {s:?} material {mat}");
    }
    for (k, c) in m.collision.iter().enumerate() {
        let _ = writeln!(
            out,
            "  collision[{k}]: vertices {} triangles {} nodes {} unknown {}",
            c.vertices.len(),
            c.triangles.len(),
            c.nodes.len(),
            c.unknown
        );
    }
    let _ = writeln!(
        out,
        "  simplified: raw tris {} wire {} material {} (property {:?})",
        m.simple_triangles.len(),
        m.simple_wireframe.len(),
        crate::props::ref_text(&package, m.simple_material),
        m.simplified_col_material_prop
            .map(|r| crate::props::ref_text(&package, r))
    );
    let _ = writeln!(
        out,
        "  unknown block {:?}; raw triangles {}; internal version {}; unknown ranges {:?}",
        m.unknown_block,
        m.raw_triangles.len(),
        m.internal_version,
        m.report.unknown
    );
    let w = m.winding_statistics();
    let _ = writeln!(out, "  winding against/along/degenerate {w:?}");
    emit(&out);
    if let Some(obj) = a.options.get("obj") {
        let mut s = String::new();
        for v in &m.vertices {
            let _ = writeln!(s, "v {} {} {}", v.position[0], v.position[1], v.position[2]);
        }
        if let Some(uv) = m.uv_streams.first() {
            for t in &uv.uvs {
                let _ = writeln!(s, "vt {} {}", t[0], 1.0 - t[1]);
            }
        }
        for t in m.indices.as_chunks::<3>().0 {
            let _ = writeln!(s, "f {0}/{0} {1}/{1} {2}/{2}", t[0] + 1, t[1] + 1, t[2] + 1);
        }
        if let Err(e) = std::fs::write(obj, s) {
            eprintln!("error: cannot write {obj}: {e}");
            return ExitCode::from(2);
        }
        println!("wrote {obj}");
    }
    ExitCode::SUCCESS
}

fn bsp_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["export", "region", "point", "grid"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = a.positional.first() else {
        return usage_error("bsp needs a map file");
    };
    let (data, package) = match load_package(Path::new(file)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let i = if a.options.contains_key("export") {
        match select(&a, &package, file) {
            Ok(i) => i,
            Err(c) => return c,
        }
    } else {
        match model::find_level_model(&package, &data) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    };
    let region = match a
        .options
        .get("region")
        .map(|v| parse_floats::<6>(v, "region"))
    {
        Some(Err(e)) => return usage_error(&e),
        Some(Ok(v)) => Some(v),
        None => None,
    };
    let grid = match a.options.get("grid").map(|v| parse_floats::<6>(v, "grid")) {
        Some(Err(e)) => return usage_error(&e),
        Some(Ok(v)) if v[5] > 0.0 => Some(v),
        Some(Ok(_)) => return usage_error("--grid step must be > 0"),
        None => None,
    };
    let point = match a
        .options
        .get("point")
        .map(|v| parse_floats::<3>(v, "point"))
    {
        Some(Err(e)) => return usage_error(&e),
        Some(Ok(v)) => Some(v),
        None => None,
    };
    match model::decode_model(&package, &data, i) {
        Ok(m) => {
            if let Some(g) = grid {
                emit(&bsp_grid_text(&m, g));
            }
            if region.is_none() && point.is_none() && grid.is_none() {
                emit(&model::summary_text(&package, i, &m));
            }
            if let Some(r) = region {
                emit(&bsp_region_text(&package, &m, r));
            }
            if let Some(p) = point {
                emit(&bsp_point_text(&m, p));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_floats<const N: usize>(v: &str, name: &str) -> Result<[f32; N], String> {
    let parts: Vec<f32> = v
        .split(',')
        .map(|x| x.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("invalid --{name} {v:?}"))?;
    parts
        .try_into()
        .map_err(|_| format!("--{name} needs {N} comma-separated numbers, got {v:?}"))
}

/// Node polygons whose axis-aligned bounds touch the source-space box `r` (x0,y0,z0,x1,y1,z1).
fn bsp_region_text(package: &Package, m: &model::Model, r: [f32; 6]) -> String {
    let mut s = String::new();
    let lo = [r[0].min(r[3]), r[1].min(r[4]), r[2].min(r[5])];
    let hi = [r[0].max(r[3]), r[1].max(r[4]), r[2].max(r[5])];
    let mut count = 0usize;
    for poly in m.polygons() {
        let mut pmin = [f32::INFINITY; 3];
        let mut pmax = [f32::NEG_INFINITY; 3];
        for v in &poly.vertices {
            for k in 0..3 {
                pmin[k] = pmin[k].min(v[k]);
                pmax[k] = pmax[k].max(v[k]);
            }
        }
        if (0..3).any(|k| pmax[k] < lo[k] || pmin[k] > hi[k]) {
            continue;
        }
        count += 1;
        let n = &m.nodes[poly.node];
        let (flags, material) = match m.surfs.get(poly.surf) {
            Some(surf) => (
                surf.poly_flags,
                package
                    .object_path(surf.material)
                    .unwrap_or("None")
                    .to_owned(),
            ),
            None => (0, "<bad surf>".to_owned()),
        };
        let _ = writeln!(
            s,
            "node {} nodeflags 0x{:02x} surf {} polyflags 0x{flags:08x} material {material}              plane ({:.3},{:.3},{:.3},{:.1}) zone {:?} leaf {:?} front {} back {} coplanar {}",
            poly.node,
            n.flags,
            poly.surf,
            n.plane[0],
            n.plane[1],
            n.plane[2],
            n.plane[3],
            n.zone,
            n.leaf,
            n.front,
            n.back,
            n.coplanar
        );
        let verts: Vec<String> = poly
            .vertices
            .iter()
            .map(|v| format!("({:.0},{:.0},{:.0})", v[0], v[1], v[2]))
            .collect();
        let _ = writeln!(s, "    verts {}", verts.join(" "));
    }
    let _ = writeln!(s, "region polygons: {count}");
    s
}

/// BSP walk of a source-space point: each visited node's side and flags, the reached
/// leaf/zone, and the UE-style CSG `Outside` value (front of a CSG node is outside, back is
/// inside, a node with `NF_NotCsg` (0x01) or `NF_IsNew` (0x20) keeps the parent's value; the
/// root starts at `RootOutside`). The rule is a labelled diagnostic, not used by the importer.
fn bsp_grid_text(m: &model::Model, g: [f32; 6]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let map = xiii_world::zones::ZoneMap::new(m);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "grid z {} step {}: rows from y {} (top) to y {}, columns from x {} (left) to x {};          zone digit 0-9a-zA-Z (zone index base 62), '#' CSG inside (solid), '.' no zone",
        g[4], g[5], g[1], g[3], g[0], g[2]
    );
    let mut y = g[1];
    while y <= g[3] {
        let mut row = String::new();
        let mut x = g[0];
        while x <= g[2] {
            let p = [x, y, g[4]];
            let c = if !csg_outside(m, map.root(), p) {
                '#'
            } else {
                match map.zone_of_point(&m.nodes, p) {
                    Some(z) => DIGITS.get(usize::from(z)).map_or('?', |&b| b as char),
                    None => '.',
                }
            };
            row.push(c);
            x += g[5];
        }
        let _ = writeln!(s, "{y:>7.0} {row}");
        y += g[5];
    }
    s
}

/// UE-style CSG side of a point (see [`bsp_point_text`]); diagnostic only.
fn csg_outside(m: &model::Model, root: usize, p: [f32; 3]) -> bool {
    let mut i = root;
    let mut outside = m.root_outside != 0;
    for _ in 0..=m.nodes.len() {
        let Some(n) = m.nodes.get(i) else {
            break;
        };
        let d = n.plane[0] * p[0] + n.plane[1] * p[1] + n.plane[2] * p[2] - n.plane[3];
        let front = d >= 0.0;
        if n.num_vertices > 0 && n.flags & (0x01 | 0x20) == 0 {
            outside = front;
        }
        let child = if front { n.front } else { n.back };
        if child < 0 {
            break;
        }
        i = child as usize;
    }
    outside
}

fn bsp_point_text(m: &model::Model, p: [f32; 3]) -> String {
    let mut s = String::new();
    let map = xiii_world::zones::ZoneMap::new(m);
    let mut i = map.root();
    let mut outside = m.root_outside != 0;
    let mut steps = 0usize;
    let _ = writeln!(
        s,
        "point ({},{},{}) root_outside {}",
        p[0], p[1], p[2], m.root_outside
    );
    loop {
        let Some(n) = m.nodes.get(i) else {
            let _ = writeln!(s, "  bad node index {i}");
            break;
        };
        let d = n.plane[0] * p[0] + n.plane[1] * p[1] + n.plane[2] * p[2] - n.plane[3];
        let front = d >= 0.0;
        let csg = n.num_vertices > 0 && n.flags & (0x01 | 0x20) == 0;
        if csg {
            outside = front;
        }
        let child = if front { n.front } else { n.back };
        steps += 1;
        if child < 0 || steps > m.nodes.len() {
            let leaf = if front { n.leaf[1] } else { n.leaf[0] };
            let zone = if front { n.zone[1] } else { n.zone[0] };
            let _ = writeln!(
                s,
                "  end node {i} side {} dist {d:.2} leaf {leaf} zone {zone} csg_outside {outside}                  (steps {steps})",
                if front { "front" } else { "back" }
            );
            break;
        }
        i = child as usize;
    }
    s
}

fn box_probe_cmd(args: &[String]) -> ExitCode {
    use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_position};
    let a = match parse_args(args, &["game-dir", "from", "to", "half", "samples", "soup"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let (Some(map), Some(root)) = (a.positional.first(), a.options.get("game-dir")) else {
        return usage_error("box-probe needs a map name and --game-dir");
    };
    let parse3 = |k: &str| a.options.get(k).map(|v| parse_floats::<3>(v, k));
    let (Some(Ok(from)), Some(Ok(to))) = (parse3("from"), parse3("to")) else {
        return usage_error("box-probe needs --from x,y,z and --to x,y,z");
    };
    let half = match a.options.get("half").map(|v| parse_floats::<2>(v, "half")) {
        None => [34.0, 75.0],
        Some(Ok(h)) => h,
        Some(Err(e)) => return usage_error(&e),
    };
    let samples: usize = match a.options.get("samples").map(|v| v.parse::<usize>()) {
        None => 64,
        Some(Ok(n)) if n >= 2 => n,
        _ => return usage_error("--samples must be an integer >= 2"),
    };
    let scene = match xiii_world::PackageCache::open(Path::new(root))
        .and_then(|mut cache| xiii_world::import_map(&mut cache, map))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let soup = a.options.get("soup").map(String::as_str) == Some("line");
    let entries: Vec<([[f32; 3]; 3], u32)> = if soup {
        scene.line_collision().collect()
    } else {
        scene.box_collision().collect()
    };
    let world = xiii_collision::CollisionWorld::new(entries);
    let s_m = 1.0 / UNREAL_UNITS_PER_METER;
    // Unreal (r, r, h) -> Bevy (y, z, x) axes.
    let half_b = [half[0] * s_m, half[1] * s_m, half[0] * s_m];
    let to_unreal = |b: [f32; 3]| {
        [
            -b[2] * UNREAL_UNITS_PER_METER,
            b[0] * UNREAL_UNITS_PER_METER,
            b[1] * UNREAL_UNITS_PER_METER,
        ]
    };
    let fmt3 = |v: [f32; 3]| format!("({:.1},{:.1},{:.1})", v[0], v[1], v[2]);
    let source_path = |id: u32| {
        scene
            .collision_sources
            .get(id as usize)
            .map_or("<unknown source>", String::as_str)
            .to_owned()
    };
    let mut s = String::new();
    let (bf, bt) = (to_bevy_position(from), to_bevy_position(to));
    match world.sweep(bf, bt, half_b) {
        Some(h) => {
            let t = world.triangle(h.triangle);
            let n = h.normal;
            let _ = writeln!(
                s,
                "sweep hit t {:.4} at {} source {} start_penetrating {} normal(unreal) ({:.3},{:.3},{:.3}) tri {} {} {}",
                h.t,
                fmt3(std::array::from_fn(|k| from[k] + (to[k] - from[k]) * h.t)),
                source_path(h.source),
                h.start_penetrating,
                -n[2],
                n[0],
                n[1],
                fmt3(to_unreal(t[0])),
                fmt3(to_unreal(t[1])),
                fmt3(to_unreal(t[2]))
            );
        }
        None => {
            let _ = writeln!(s, "sweep: no hit");
        }
    }
    let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
    for i in 0..samples {
        let f = i as f32 / (samples - 1) as f32;
        let c: [f32; 3] = std::array::from_fn(|k| from[k] + (to[k] - from[k]) * f);
        for hit in world.overlap_aabb(to_bevy_position(c), half_b) {
            seen.entry(hit.triangle).or_insert(i);
        }
    }
    for (tri, first) in &seen {
        let t = world.triangle(*tri);
        let e1: [f32; 3] = std::array::from_fn(|k| t[1][k] - t[0][k]);
        let e2: [f32; 3] = std::array::from_fn(|k| t[2][k] - t[0][k]);
        let n = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
        let _ = writeln!(
            s,
            "overlap tri {tri} first-sample {first} source {} verts {} {} {} normal(unreal) ({:.3},{:.3},{:.3})",
            source_path(world.source(*tri)),
            fmt3(to_unreal(t[0])),
            fmt3(to_unreal(t[1])),
            fmt3(to_unreal(t[2])),
            -n[2] / len,
            n[0] / len,
            n[1] / len
        );
    }
    let _ = writeln!(s, "overlapping triangles: {}", seen.len());
    emit(&s);
    ExitCode::SUCCESS
}

fn nav_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["game-dir"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let (Some(map), Some(root)) = (a.positional.first(), a.options.get("game-dir")) else {
        return usage_error("nav needs a map name and --game-dir");
    };
    let root = Path::new(root);
    let nav = xiii_world::PackageCache::open(root).and_then(|mut cache| {
        let mut defaults = xiii_world::ClassDefaults::open(root)?;
        xiii_world::navigation::decode_navigation(&mut cache, &mut defaults, map)
    });
    let nav = match nav {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut s = String::new();
    for (i, p) in nav.points.iter().enumerate() {
        let l = p.location;
        let _ = writeln!(
            s,
            "point {i} {} {} ({:.0},{:.0},{:.0}) r {} h {}",
            p.class, p.path, l[0], l[1], l[2], p.collision_radius, p.collision_height
        );
    }
    // Union-find over every edge with both ends resolved.
    let mut parent: Vec<usize> = (0..nav.points.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for e in &nav.edges {
        let _ = writeln!(
            s,
            "edge {:?} -> {:?} r {} h {} flags 0x{:x} {:?} dist {}",
            e.start_point,
            e.end_point,
            e.collision_radius,
            e.collision_height,
            e.reach_flags,
            xiii_world::navigation::reach_flags::names(e.reach_flags),
            e.distance
        );
        if let (Some(x), Some(y)) = (e.start_point, e.end_point) {
            let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
            parent[rx] = ry;
        }
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..nav.points.len() {
        let r = find(&mut parent, i);
        comps.entry(r).or_default().push(i);
    }
    let _ = writeln!(s, "components {}", comps.len());
    for members in comps.values() {
        let list: Vec<String> = members.iter().map(|m| m.to_string()).collect();
        let _ = writeln!(s, "component size {}: {}", members.len(), list.join(" "));
    }
    emit(&s);
    ExitCode::SUCCESS
}

fn zones_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["export"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = a.positional.first() else {
        return usage_error("zones needs a map file");
    };
    let (data, package) = match load_package(Path::new(file)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let i = if a.options.contains_key("export") {
        match select(&a, &package, file) {
            Ok(i) => i,
            Err(c) => return c,
        }
    } else {
        match model::find_level_model(&package, &data) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    };
    let m = match model::decode_model(&package, &data, i) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    emit(&model::zones_text(&package, i, &m));
    ExitCode::SUCCESS
}

fn terrain_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["game-dir"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = a.positional.first() else {
        return usage_error("terrain needs a map file");
    };
    let (data, package) = match load_package(Path::new(file)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut out = String::new();
    let mut failed = false;
    for i in 0..package.exports().len() {
        let class = package.export_class_path(i).unwrap_or("");
        if class.eq_ignore_ascii_case(terrain::TERRAIN_INFO_CLASS) {
            match terrain::decode_terrain_info(&package, &data, i) {
                Ok(t) => out.push_str(&terrain::info_summary(&package, i, &t)),
                Err(e) => {
                    failed = true;
                    let _ = writeln!(out, "FAIL {e}");
                }
            }
        } else if class.eq_ignore_ascii_case(terrain::TERRAIN_SECTOR_CLASS) {
            match terrain::decode_sector(&package, &data, i) {
                Ok(s) => out.push_str(&terrain::sector_summary(&package, i, &s)),
                Err(e) => {
                    failed = true;
                    let _ = writeln!(out, "FAIL {e}");
                }
            }
        }
    }
    emit(&out);
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ---------------------------------------------------------------------------------------
// Material survey
// ---------------------------------------------------------------------------------------

/// UE2 material classes the survey recognises (the spec's list plus the base modifiers).
const MATERIAL_CLASSES: &[&str] = &[
    "Texture",
    "Shader",
    "Combiner",
    "FinalBlend",
    "TexPanner",
    "TexRotator",
    "TexScaler",
    "TexOscillator",
    "TexEnvMap",
    "TexCoordSource",
    "TexModifier",
    "SinusModifier",
    "ConstantColor",
    "Cubemap",
    "ColorModifier",
];

fn is_material_class(short: &str) -> bool {
    MATERIAL_CLASSES
        .iter()
        .any(|c| c.eq_ignore_ascii_case(short))
}

/// Distinct-value cap so a property like `Diffuse` (hundreds of textures) stays readable.
const VALUE_CAP: usize = 12;

/// Observed values of one tagged property across the corpus.
#[derive(Default, Clone)]
struct ValueStats {
    count: usize,
    min: f32,
    max: f32,
    numeric: bool,
    values: BTreeMap<String, usize>,
    truncated: bool,
}

impl ValueStats {
    fn observe(&mut self, v: f32) {
        if !self.numeric {
            self.min = v;
            self.max = v;
            self.numeric = true;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
    }

    fn add(&mut self, package: &Package, prop: &Property) {
        self.count += 1;
        let key = match &prop.value {
            PropertyValue::Float(f) => {
                self.observe(*f);
                return;
            }
            PropertyValue::Int(i) => {
                self.observe(*i as f32);
                return;
            }
            PropertyValue::Byte(b) => b.to_string(),
            PropertyValue::Bool(b) => b.to_string(),
            PropertyValue::Name(n) => package.name(*n).to_owned(),
            PropertyValue::Object(r) | PropertyValue::Class(r) => {
                crate::props::ref_text(package, *r)
            }
            // Struct/rotator/vector/array/raw values: use the normal property renderer so the
            // value ranges are visible instead of a discriminant placeholder.
            _ => crate::props::value_text(package, prop),
        };
        if self.values.contains_key(&key) {
            *self.values.get_mut(&key).expect("present") += 1;
        } else if self.values.len() < VALUE_CAP {
            self.values.insert(key, 1);
        } else {
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        if self.numeric {
            format!("{} values, range {}..{}", self.count, self.min, self.max)
        } else {
            let mut s = format!("{} values {{", self.count);
            for (i, (k, n)) in self.values.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                let _ = write!(s, "{k}: {n}");
            }
            if self.truncated {
                s.push_str(", ...");
            }
            s.push('}');
            s
        }
    }
}

/// One map material row: player footstep wrapper, AI footstep wrapper, noise loudness.
type FootstepRow = (Option<String>, Option<String>, Option<f32>);

fn material_survey_cmd(args: &[String]) -> ExitCode {
    let a = match parse_args(args, &["maps"]) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let Some(root) = a.positional.first().map(PathBuf::from) else {
        return usage_error("material-survey needs an installation root");
    };
    let maps: Vec<String> = a
        .options
        .get("maps")
        .map(|s| s.split(',').map(|x| x.trim().to_owned()).collect())
        .unwrap_or_else(|| vec!["Plage00".into(), "Plage01".into(), "Banque01".into()]);
    let mut out = String::new();

    // ---- corpus-wide material export histogram and property values ----
    let mut class_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut prop_stats: BTreeMap<(String, String), ValueStats> = BTreeMap::new();
    let (mut files, mut exports, mut prop_failures) = (0usize, 0usize, 0usize);
    let mut engine_path: Option<PathBuf> = None;
    let tagged = match tagged_files(&root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: cannot scan {}: {e}", root.display());
            return ExitCode::from(2);
        }
    };
    for (_rel, path) in &tagged {
        if path
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case("engine"))
        {
            engine_path = Some(path.clone());
        }
        let (data, package) = match load_package(path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        files += 1;
        for i in 0..package.exports().len() {
            if package.exports()[i].serial_size == 0 {
                continue;
            }
            let class = package.export_class_path(i).unwrap_or("?");
            let short = class.rsplit('.').next().unwrap_or("");
            if !is_material_class(short) {
                continue;
            }
            exports += 1;
            *class_counts.entry(short.to_owned()).or_default() += 1;
            match package.read_object_properties(&data, i, &Limits::default()) {
                Ok(props) => {
                    for prop in &props.block.properties {
                        let name = package.property_name(prop);
                        prop_stats
                            .entry((short.to_owned(), name.to_owned()))
                            .or_default()
                            .add(&package, prop);
                    }
                }
                Err(_) => prop_failures += 1,
            }
        }
    }
    let _ = writeln!(
        out,
        "material-survey: {} ({} packages, {} material exports, {} property-block failures)",
        root.display(),
        files,
        exports,
        prop_failures
    );
    let _ = writeln!(out, "== corpus material exports ==");
    for (c, n) in &class_counts {
        let _ = writeln!(out, "  {n:>6} {c}");
    }
    let _ = writeln!(out, "== property values per class ==");
    for ((class, prop), stats) in &prop_stats {
        let _ = writeln!(out, "  {class}.{prop}: {}", stats.render());
    }

    // ---- classes referenced by placed geometry, per map ----
    let _ = writeln!(out, "== geometry material classes per map ==");
    let mut cache = match xiii_world::PackageCache::open(&root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot open installation: {e}");
            return ExitCode::from(2);
        }
    };
    for map in &maps {
        match xiii_world::import_map(&mut cache, map) {
            Ok(scene) => {
                let mut classes: BTreeMap<String, usize> = BTreeMap::new();
                let mut blends: BTreeMap<String, usize> = BTreeMap::new();
                let mut unsupported: BTreeMap<String, usize> = BTreeMap::new();
                for m in &scene.materials {
                    for c in &m.class_chain {
                        *classes.entry(c.clone()).or_default() += 1;
                    }
                    *blends.entry(m.blend.name().to_owned()).or_default() += 1;
                    for u in &m.unsupported {
                        *unsupported.entry(u.clone()).or_default() += 1;
                    }
                }
                let _ = writeln!(
                    out,
                    "  {map}: {} materials | classes {classes:?} | blends {blends:?} | unsupported {unsupported:?}",
                    scene.materials.len()
                );
            }
            Err(e) => {
                let _ = writeln!(out, "  {map}: IMPORT FAILED: {e}");
            }
        }
    }

    // ---- footstep surfaces per map (item6e): which material plays which wrapper ----
    let _ = writeln!(out, "== footstep surfaces per map ==");
    for map in &maps {
        match xiii_world::import_map(&mut cache, map) {
            Ok(scene) => {
                let mut rows: BTreeMap<String, FootstepRow> = BTreeMap::new();
                for m in &scene.materials {
                    let path = m
                        .material_path
                        .clone()
                        .unwrap_or_else(|| "<no path>".to_owned());
                    if m.footstep_sound.is_none() && m.footstep_sound_ai.is_none() {
                        continue;
                    }
                    rows.entry(path).or_insert((
                        m.footstep_sound.clone(),
                        m.footstep_sound_ai.clone(),
                        m.noise_loudness,
                    ));
                }
                let total = scene.materials.len();
                let box_steps = scene
                    .collision_box
                    .iter()
                    .filter(|&&i| scene.footstep_sound(i).is_some())
                    .count();
                let box_material = scene
                    .collision_box
                    .iter()
                    .filter(|&&i| scene.collision_material(i).is_some())
                    .count();
                let _ = writeln!(
                    out,
                    "  {map}: {} distinct material path(s) with a footstep wrapper of {total} resolved; \
                     box collision {box_steps}/{} triangles have a surface footstep ({box_material} have a material)",
                    rows.len(),
                    scene.collision_box.len()
                );
                for (path, (player, ai, noise)) in &rows {
                    let _ = writeln!(
                        out,
                        "    {path} -> player {} | ai {} | noise {}",
                        player.as_deref().unwrap_or("-"),
                        ai.as_deref().unwrap_or("-"),
                        noise.map_or_else(|| "-".to_owned(), |n| n.to_string())
                    );
                }
            }
            Err(e) => {
                let _ = writeln!(out, "  {map}: IMPORT FAILED: {e}");
            }
        }
    }

    // ---- reflected blend enums (the mapping source) ----
    if let Some(path) = engine_path
        && let Ok(data) = std::fs::read(&path)
        && let Ok(pkg) = xiii_script::ScriptPackage::load(
            "engine",
            data,
            &xiii_script::ScriptLimits::default(),
            &Limits::default(),
        )
    {
        let _ = writeln!(out, "== reflected enums ==");
        for enum_path in [
            "Shader.EOutputBlending",
            "FinalBlend.EFrameBufferBlending",
            "TexOscillator.ETexOscillationType",
            "TexModifier.ETexCoordSrc",
            "TexEnvMap.ETexEnvMapType",
            "SinusModifier.ESinusAmplitudeFade",
            "Combiner.EColorOperation",
            "Combiner.EAlphaOperation",
        ] {
            let Some(e) = pkg.export_by_path(enum_path) else {
                continue;
            };
            if let Some(xiii_script::ScriptObject::Enum(en)) = pkg.objects.get(&e) {
                let names: Vec<&str> = en.names.iter().map(|&n| pkg.name_text(n)).collect();
                let _ = writeln!(out, "  {enum_path} = {names:?}");
            }
        }
    }
    emit(&out);
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------------------
// Minimal PNG writer (stored deflate blocks; no compression) for decoded-texture dumps.
// ---------------------------------------------------------------------------------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Encodes RGBA8 pixels as a PNG byte stream.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks_exact(width as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    if chunks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8; 4], body: &[u8]| {
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(body);
        png.extend_from_slice(&c);
        png.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    png
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    std::fs::write(path, encode_png(width, height, rgba))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_signature_and_crc() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        let png = encode_png(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn class_selection_parsing() {
        assert!(
            ClassSelection::parse("texture,staticmesh")
                .unwrap()
                .static_mesh
        );
        assert!(ClassSelection::parse("bogus").is_err());
    }
}
