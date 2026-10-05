//! `xiii-tool anim ...`: skeletal mesh / animation coverage, validation and dev-aid exports
//! (wireframe PNG sheets and OBJ). Output files contain decoded game geometry: write them
//! outside the repository and the installation.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xiii_decode::skeletal::math::{Transform, Vec3};
use xiii_decode::skeletal::normalize::evaluate_pose;
use xiii_decode::skeletal::{
    self, AnimSet, Clip, MESH_ANIMATION_CLASS, RawSkeletalMesh, SKELETAL_MESH_CLASS, Skeleton,
    SkinnedMesh, validate,
};
use xiii_package::{Limits, ObjectRef, Package};

pub const USAGE: &str = "\
  xiii-tool anim coverage <install-root>
      Decode every Engine.SkeletalMesh and Engine.MeshAnimation export of every
      package (exact payload consumption required) and print per-class counts and
      the first failure per class. Exits 1 on any failure.

  xiii-tool anim list --package <name|file> [--game-dir <root>]
      List skeletal meshes and animations of one package with decoded counts.

  xiii-tool anim validate --package <name|file> --mesh <name> [--game-dir <root>]
                          [--anim <name>] [--anim-package <name|file>] [--seq <name>]...
      Skeleton, skin-weight and clip checks; per-frame skinned bounds and a foot
      probe for each --seq. The mesh's own animation is used unless --anim is given;
      an imported animation package is resolved through --game-dir.

  xiii-tool anim render ... --seq <name> --frames <f,f,..> --out <file.png>
      Same selection options as validate. Writes one PNG sheet: columns = frames
      (bind pose first), rows = front (X right, Z up) and side (Y right, Z up)
      orthographic wireframes in source coordinates, bones in red.

  xiii-tool anim export ... [--seq <name> --frame <f>] --out <file.obj>
      Writes the (posed) skinned mesh and its bones as a Wavefront OBJ.";

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\nUSAGE:\n{USAGE}");
    ExitCode::from(2)
}

/// Entry point for `xiii-tool anim <sub> ...`.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("coverage") => coverage(&args[1..]),
        Some("list") => with_opts(&args[1..], list),
        Some("validate") => with_opts(&args[1..], validate_cmd),
        Some("render") => with_opts(&args[1..], render),
        Some("export") => with_opts(&args[1..], export),
        Some(other) => usage_error(&format!("unknown anim command '{other}'")),
        None => usage_error("missing anim command"),
    }
}

// ------------------------------------------------------------------ options / loading

#[derive(Default, Debug)]
struct Opts {
    package: Option<String>,
    game_dir: Option<PathBuf>,
    mesh: Option<String>,
    anim: Option<String>,
    anim_package: Option<String>,
    seqs: Vec<String>,
    frames: Vec<f32>,
    frame: Option<f32>,
    out: Option<PathBuf>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value"))
        };
        match a.as_str() {
            "--package" => o.package = Some(val()?),
            "--game-dir" => o.game_dir = Some(PathBuf::from(val()?)),
            "--mesh" => o.mesh = Some(val()?),
            "--anim" => o.anim = Some(val()?),
            "--anim-package" => o.anim_package = Some(val()?),
            "--seq" => o.seqs.push(val()?),
            "--frames" => {
                o.frames = val()?
                    .split(',')
                    .map(|s| {
                        s.trim()
                            .parse::<f32>()
                            .map_err(|e| format!("--frames: {e}"))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--frame" => {
                o.frame = Some(val()?.parse().map_err(|e| format!("--frame: {e}"))?);
            }
            "--out" => o.out = Some(PathBuf::from(val()?)),
            s => return Err(format!("unexpected argument '{s}'")),
        }
    }
    Ok(o)
}

fn with_opts(args: &[String], f: fn(&Opts) -> Result<String, CmdError>) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => return usage_error(&e),
    };
    match f(&opts) {
        Ok(text) => {
            crate::emit(&text);
            ExitCode::SUCCESS
        }
        Err(CmdError::Usage(e)) => usage_error(&e),
        Err(CmdError::Io(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
        Err(CmdError::Decode(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

enum CmdError {
    Usage(String),
    Io(String),
    Decode(String),
}

struct Loaded {
    label: String,
    data: Vec<u8>,
    package: Package,
}

fn read_file(path: &Path) -> Result<Loaded, CmdError> {
    let data = std::fs::read(path)
        .map_err(|e| CmdError::Io(format!("cannot read {}: {e}", path.display())))?;
    let package = Package::parse(&data, &Limits::default())
        .map_err(|e| CmdError::Decode(format!("{}: {e}", path.display())))?;
    Ok(Loaded {
        label: path.display().to_string(),
        data,
        package,
    })
}

/// Loads a package given as an existing file path or a logical name resolved in `game_dir`.
fn load_package(spec: &str, game_dir: Option<&Path>) -> Result<Loaded, CmdError> {
    let as_path = Path::new(spec);
    if as_path.is_file() {
        return read_file(as_path);
    }
    let Some(root) = game_dir else {
        return Err(CmdError::Usage(format!(
            "'{spec}' is not a file; pass --game-dir to resolve logical package names"
        )));
    };
    let inst = xiii_install::Installation::open(root, &xiii_install::OpenOptions::default())
        .map_err(|e| {
            CmdError::Io(format!(
                "cannot open installation {}: {e:?}",
                root.display()
            ))
        })?;
    let stem = as_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(spec)
        .to_owned();
    let resolved = inst
        .resolve_package(&stem)
        .map_err(|e| CmdError::Decode(format!("cannot resolve package '{stem}': {e:?}")))?;
    read_file(resolved.path())
}

fn find_export(p: &Package, name: &str, class: &str) -> Option<usize> {
    (0..p.exports().len()).find(|&i| {
        p.export_class_path(i) == Some(class)
            && p.object_name(ObjectRef::Export(i as u32))
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

struct Character {
    mesh_name: String,
    raw: RawSkeletalMesh,
    skeleton: Skeleton,
    mesh: SkinnedMesh,
    anim_label: Option<String>,
    anims: Option<AnimSet>,
}

fn load_character(o: &Opts) -> Result<Character, CmdError> {
    let spec = o
        .package
        .as_deref()
        .ok_or_else(|| CmdError::Usage("--package is required".into()))?;
    let mesh_name = o
        .mesh
        .as_deref()
        .ok_or_else(|| CmdError::Usage("--mesh is required".into()))?;
    let pkg = load_package(spec, o.game_dir.as_deref())?;
    let mi = find_export(&pkg.package, mesh_name, SKELETAL_MESH_CLASS).ok_or_else(|| {
        CmdError::Decode(format!("no SkeletalMesh '{mesh_name}' in {}", pkg.label))
    })?;
    let raw = skeletal::decode_skeletal_mesh(&pkg.package, &pkg.data, mi)
        .map_err(|e| CmdError::Decode(format!("{}: {e}", pkg.label)))?;
    let skeleton = Skeleton::from_mesh(&raw)
        .map_err(|e| CmdError::Decode(format!("{mesh_name}: skeleton: {e}")))?;
    let mesh = SkinnedMesh::from_raw(&raw, &skeleton, Some(&pkg.package))
        .map_err(|e| CmdError::Decode(format!("{mesh_name}: skin: {e}")))?;

    // Animation: explicit --anim [--anim-package], else the mesh's own reference.
    let (anim_pkg_spec, anim_name) = match (&o.anim, &o.anim_package) {
        (Some(a), ap) => (ap.clone(), Some(a.clone())),
        (None, _) => match raw.animation {
            ObjectRef::Null => (None, None),
            ObjectRef::Export(i) => (
                None,
                pkg.package
                    .object_name(ObjectRef::Export(i))
                    .map(str::to_owned),
            ),
            r @ ObjectRef::Import(_) => {
                let path = pkg.package.object_path(r).unwrap_or("").to_owned();
                let mut parts = path.splitn(2, '.');
                let root = parts.next().map(str::to_owned);
                (root, parts.next().map(str::to_owned))
            }
        },
    };
    let (anim_label, anims) = match anim_name {
        None => (None, None),
        Some(name) => {
            let apkg = match &anim_pkg_spec {
                Some(s) => Some(load_package(s, o.game_dir.as_deref())?),
                None => None,
            };
            let ap = apkg.as_ref().unwrap_or(&pkg);
            let ai = find_export(&ap.package, &name, MESH_ANIMATION_CLASS).ok_or_else(|| {
                CmdError::Decode(format!("no MeshAnimation '{name}' in {}", ap.label))
            })?;
            let raw_anim = skeletal::decode_mesh_animation(&ap.package, &ap.data, ai)
                .map_err(|e| CmdError::Decode(format!("{}: {e}", ap.label)))?;
            let set = AnimSet::from_raw(&raw_anim)
                .map_err(|e| CmdError::Decode(format!("{name}: {e}")))?;
            (Some(format!("{name} ({})", ap.label)), Some(set))
        }
    };
    Ok(Character {
        mesh_name: mesh_name.to_owned(),
        raw,
        skeleton,
        mesh,
        anim_label,
        anims,
    })
}

fn clip<'a>(c: &'a Character, name: &str) -> Result<&'a Clip, CmdError> {
    let set = c
        .anims
        .as_ref()
        .ok_or_else(|| CmdError::Decode(format!("{} has no animation", c.mesh_name)))?;
    set.clip(name)
        .ok_or_else(|| CmdError::Decode(format!("no sequence '{name}'")))
}

fn v(v: Vec3) -> String {
    format!("({:.1}, {:.1}, {:.1})", v.x, v.y, v.z)
}

// ------------------------------------------------------------------ coverage

#[derive(Default)]
struct ClassCov {
    attempted: usize,
    ok: usize,
    failed: usize,
    bytes: u64,
    first_error: Option<String>,
    packages: BTreeMap<String, usize>,
}

fn coverage(args: &[String]) -> ExitCode {
    let [root] = args else {
        return usage_error("anim coverage needs an installation root");
    };
    let root = Path::new(root);
    let outcomes = match xiii_tool::corpus::tagged_files(root) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let mut cov: BTreeMap<&str, ClassCov> = BTreeMap::new();
    let mut others: BTreeMap<String, usize> = BTreeMap::new();
    let mut extra = String::new();
    let (mut seqs, mut keys, mut bones_total, mut invalid_infl) = (0usize, 0usize, 0usize, 0usize);
    for (rel, path) in &outcomes {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(p) = Package::parse(&data, &Limits::default()) else {
            continue;
        };
        for i in 0..p.exports().len() {
            let class = p.export_class_path(i).unwrap_or("");
            let size = u64::from(p.exports()[i].serial_size);
            let name = p.object_name(ObjectRef::Export(i as u32)).unwrap_or("?");
            let entry = match class {
                SKELETAL_MESH_CLASS | MESH_ANIMATION_CLASS => cov
                    .entry(if class == SKELETAL_MESH_CLASS {
                        SKELETAL_MESH_CLASS
                    } else {
                        MESH_ANIMATION_CLASS
                    })
                    .or_default(),
                c if c.ends_with(".VertMesh") => {
                    *others.entry(format!("{c} (not decoded)")).or_default() += 1;
                    continue;
                }
                _ => continue,
            };
            entry.attempted += 1;
            entry.bytes += size;
            *entry.packages.entry(rel.clone()).or_default() += 1;
            let result: Result<(), String> = if class == SKELETAL_MESH_CLASS {
                skeletal::decode_skeletal_mesh(&p, &data, i)
                    .map_err(|e| e.to_string())
                    .and_then(|raw| {
                        let s = Skeleton::from_mesh(&raw).map_err(|e| e.to_string())?;
                        let m =
                            SkinnedMesh::from_raw(&raw, &s, Some(&p)).map_err(|e| e.to_string())?;
                        bones_total += s.bones.len();
                        if m.influence_stats.invalid_bone_influences > 0 {
                            invalid_infl += m.influence_stats.invalid_bone_influences;
                            let _ = writeln!(
                                extra,
                                "  note: {rel} {name}: {} influences on out-of-range bones dropped",
                                m.influence_stats.invalid_bone_influences
                            );
                        }
                        Ok(())
                    })
            } else {
                skeletal::decode_mesh_animation(&p, &data, i)
                    .map_err(|e| e.to_string())
                    .and_then(|raw| {
                        let set = AnimSet::from_raw(&raw).map_err(|e| e.to_string())?;
                        seqs += set.clips.len();
                        keys += set
                            .clips
                            .iter()
                            .flat_map(|c| &c.tracks)
                            .map(|t| t.rotations.len())
                            .sum::<usize>();
                        for c in &set.clips {
                            let r = validate::clip_report(&set, c);
                            if !(r.track_count_ok
                                && r.times_monotone
                                && r.times_in_range
                                && r.position_counts_ok
                                && r.quats_finite)
                            {
                                return Err(format!(
                                    "clip '{}' fails structure checks: {r:?}",
                                    c.name
                                ));
                            }
                            if r.notifies_out_of_range > 0 {
                                let _ = writeln!(
                                    extra,
                                    "  note: {rel} {name}.{}: {} notify time(s) outside 0..1",
                                    c.name, r.notifies_out_of_range
                                );
                            }
                        }
                        Ok(())
                    })
            };
            match result {
                Ok(()) => entry.ok += 1,
                Err(e) => {
                    entry.failed += 1;
                    entry
                        .first_error
                        .get_or_insert_with(|| format!("{rel} {name}: {e}"));
                }
            }
        }
    }
    let mut out = String::new();
    let _ = writeln!(out, "anim coverage: {}", root.display());
    let _ = writeln!(
        out,
        "{:<22} {:>8} {:>8} {:>8} {:>12}  packages",
        "class", "attempt", "ok", "failed", "bytes"
    );
    let mut failed = false;
    for (class, c) in &cov {
        failed |= c.failed > 0;
        let pk: Vec<String> = c.packages.iter().map(|(k, n)| format!("{k}:{n}")).collect();
        let _ = writeln!(
            out,
            "{:<22} {:>8} {:>8} {:>8} {:>12}  {}",
            class,
            c.attempted,
            c.ok,
            c.failed,
            c.bytes,
            pk.join(", ")
        );
        if let Some(e) = &c.first_error {
            let _ = writeln!(out, "  first failure: {e}");
        }
    }
    for (c, n) in &others {
        let _ = writeln!(out, "{c}: {n}");
    }
    let _ = writeln!(
        out,
        "sequences {seqs}, rotation keys {keys}, mesh bones {bones_total}, dropped out-of-range influences {invalid_infl}"
    );
    out.push_str(&extra);
    let _ = writeln!(out, "result: {}", if failed { "FAIL" } else { "OK" });
    crate::emit(&out);
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ------------------------------------------------------------------ list

fn list(o: &Opts) -> Result<String, CmdError> {
    let spec = o
        .package
        .as_deref()
        .ok_or_else(|| CmdError::Usage("--package is required".into()))?;
    let pkg = load_package(spec, o.game_dir.as_deref())?;
    let p = &pkg.package;
    let mut out = String::new();
    let _ = writeln!(out, "package {}", pkg.label);
    for i in 0..p.exports().len() {
        let class = p.export_class_path(i).unwrap_or("");
        let name = p.object_name(ObjectRef::Export(i as u32)).unwrap_or("?");
        if class == SKELETAL_MESH_CLASS {
            match skeletal::decode_skeletal_mesh(p, &pkg.data, i) {
                Ok(m) => {
                    let anim = p.object_path(m.animation).unwrap_or("None");
                    let _ = writeln!(
                        out,
                        "[{i}] SkeletalMesh {name}: points {} wedges {} faces {} materials {} bones {} influences {} boxes {} anim {anim}",
                        m.points.len(),
                        m.wedges.len(),
                        m.faces.len(),
                        m.materials.len(),
                        m.ref_skeleton.len(),
                        m.bone_influences.len(),
                        m.bone_boxes.len()
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "[{i}] SkeletalMesh {name}: FAILED {e}");
                }
            }
        } else if class == MESH_ANIMATION_CLASS {
            match skeletal::decode_mesh_animation(p, &pkg.data, i) {
                Ok(a) => {
                    let names: Vec<&str> = a
                        .sequences
                        .iter()
                        .take(8)
                        .map(|s| s.name.as_str())
                        .collect();
                    let _ = writeln!(
                        out,
                        "[{i}] MeshAnimation {name}: bones {} sequences {} ({}{})",
                        a.ref_bones.len(),
                        a.sequences.len(),
                        names.join(", "),
                        if a.sequences.len() > 8 { ", ..." } else { "" }
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "[{i}] MeshAnimation {name}: FAILED {e}");
                }
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ validate

fn validate_cmd(o: &Opts) -> Result<String, CmdError> {
    let c = load_character(o)?;
    let mut out = String::new();
    let stored: Vec<[f32; 4]> = c.raw.ref_skeleton.iter().map(|b| b.orientation).collect();
    let sr = validate::skeleton_report(&c.skeleton, &stored);
    let _ = writeln!(out, "mesh {}", c.mesh_name);
    let _ = writeln!(
        out,
        "skeleton: bones {} roots {} max depth {} bind finite {} max |q|-1 {:.2e}; joint bounds {} .. {} (height {:.1})",
        sr.bones,
        sr.roots,
        sr.max_depth,
        sr.bind_finite,
        sr.max_quat_norm_error,
        v(sr.joint_min),
        v(sr.joint_max),
        sr.height()
    );
    for (i, b) in c.skeleton.bones.iter().enumerate() {
        let _ = writeln!(
            out,
            "  [{i:2}] {:<16} parent {:>3} global {}",
            b.name,
            b.parent.map_or("-".to_owned(), |p| p.to_string()),
            v(b.bind_global.translation)
        );
    }
    let mr = validate::mesh_report(&c.mesh, &c.skeleton);
    let st = c.mesh.influence_stats;
    let _ = writeln!(
        out,
        "mesh: vertices {} triangles {} sections {} degenerate {}; bind bounds {} .. {}; weights sum {:.4}..{:.4} (raw {:.4}..{:.4}), max influences {}, unweighted points {}, truncated {}, out-of-range influences dropped {}; outward normals {:.0}%; bind skin error {:.2e}",
        mr.vertices,
        mr.triangles,
        mr.sections,
        mr.degenerate_triangles,
        v(mr.bind_min),
        v(mr.bind_max),
        mr.weight_sum_min,
        mr.weight_sum_max,
        st.raw_sum_min,
        st.raw_sum_max,
        st.max_influences,
        st.points_without_influences,
        st.points_truncated,
        st.invalid_bone_influences,
        mr.outward_normal_fraction * 100.0,
        mr.bind_skin_max_error
    );
    let _ = writeln!(
        out,
        "mesh transform: scale {} origin {} rot origin {:?}; materials {:?}",
        v(c.mesh.mesh_scale),
        v(c.mesh.mesh_origin),
        c.mesh.rot_origin,
        c.mesh
            .materials
            .iter()
            .map(|m| m.texture_path.clone().unwrap_or_else(|| "None".into()))
            .collect::<Vec<_>>()
    );
    let Some(set) = &c.anims else {
        let _ = writeln!(out, "no animation");
        return Ok(out);
    };
    let map = set.bone_map(&c.skeleton);
    let missing: Vec<&str> = c
        .skeleton
        .bones
        .iter()
        .zip(&map)
        .filter(|(_, m)| m.is_none())
        .map(|(b, _)| b.name.as_str())
        .collect();
    let _ = writeln!(
        out,
        "animation {}: bones {} clips {}; mesh bones without a track: {:?}",
        c.anim_label.as_deref().unwrap_or("?"),
        set.bone_names.len(),
        set.clips.len(),
        missing
    );
    let mut bad = 0;
    for cl in &set.clips {
        let r = validate::clip_report(set, cl);
        if !(r.track_count_ok && r.times_monotone && r.times_in_range && r.position_counts_ok) {
            bad += 1;
            let _ = writeln!(out, "  BAD clip {r:?}");
        }
    }
    let _ = writeln!(out, "clips failing structure checks: {bad}");
    let (bmin, bmax) = (mr.bind_min, mr.bind_max);
    let probe = c
        .skeleton
        .find("X L Foot")
        .or_else(|| c.skeleton.find("X L Hand"))
        .unwrap_or(0);
    for name in &o.seqs {
        let cl = clip(&c, name)?;
        let r = validate::clip_report(set, cl);
        let _ = writeln!(
            out,
            "sequence {} groups {:?}: frames {} rate {} duration {:.3}s keys {} root speed {} notifies {:?}",
            cl.name,
            cl.groups,
            cl.num_frames,
            cl.rate,
            cl.duration,
            r.rotation_keys,
            v(cl.root_speed),
            cl.notifies
                .iter()
                .map(|n| format!("{:.3}:{}", n.time, n.function))
                .collect::<Vec<_>>()
        );
        let fb = validate::skinned_bounds_over_time(&c.mesh, &c.skeleton, cl, &map, 1.0, probe);
        let ext = |a: Vec3, b: Vec3| b - a;
        let bind_ext = ext(bmin, bmax);
        let mut max_ratio = 0.0f32;
        let mut finite = true;
        for f in &fb {
            let e = ext(f.min, f.max);
            finite &= f.min.is_finite() && f.max.is_finite();
            max_ratio = max_ratio.max(e.length() / bind_ext.length().max(1e-6));
            let _ = writeln!(
                out,
                "  frame {:5.1}: bounds {} .. {}  probe {} {}",
                f.frame,
                v(f.min),
                v(f.max),
                c.skeleton.bones[probe].name,
                v(f.probe)
            );
        }
        // Loop closure: last key frame vs. wrap target (frame 0).
        let p0 = evaluate_pose(&c.skeleton, cl, &map, 0.0, true);
        let pl = evaluate_pose(&c.skeleton, cl, &map, cl.track_time - 1.0, true);
        let jump = p0
            .iter()
            .zip(&pl)
            .map(|(a, b)| (a.translation - b.translation).length())
            .fold(0.0f32, f32::max);
        let _ = writeln!(
            out,
            "  summary: all finite {finite}; max bounds diagonal / bind diagonal {max_ratio:.2}; max joint distance frame 0 vs frame {} {:.2}",
            cl.track_time - 1.0,
            jump
        );
    }
    Ok(out)
}

// ------------------------------------------------------------------ render

/// RGB canvas with Bresenham lines.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![255; w * h * 3],
        }
    }

    fn put(&mut self, x: i64, y: i64, c: [u8; 3]) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            let i = (y as usize * self.w + x as usize) * 3;
            self.px[i..i + 3].copy_from_slice(&c);
        }
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), c: [u8; 3]) {
        if !(a.0.is_finite() && a.1.is_finite() && b.0.is_finite() && b.1.is_finite()) {
            return;
        }
        let (mut x0, mut y0) = (a.0.round() as i64, a.1.round() as i64);
        let (x1, y1) = (b.0.round() as i64, b.1.round() as i64);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx + dy;
        for _ in 0..20_000 {
            self.put(x0, y0, c);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    fn dot(&mut self, p: (f32, f32), r: i64, c: [u8; 3]) {
        let (x, y) = (p.0.round() as i64, p.1.round() as i64);
        for dy in -r..=r {
            for dx in -r..=r {
                self.put(x + dx, y + dy, c);
            }
        }
    }
}

const PANEL: usize = 300;

/// One orthographic panel: horizontal axis `hx` (0 = X, 1 = Y), vertical Z up.
#[allow(clippy::too_many_arguments)]
fn draw_panel(
    cv: &mut Canvas,
    ox: usize,
    oy: usize,
    hx: usize,
    center: (f32, f32),
    scale: f32,
    pts: &[Vec3],
    indices: &[u32],
    pose: &[Transform],
    parents: &[Option<usize>],
) {
    let proj = |p: Vec3| {
        let h = if hx == 0 { p.x } else { p.y };
        (
            ox as f32 + PANEL as f32 / 2.0 + (h - center.0) * scale,
            oy as f32 + PANEL as f32 / 2.0 - (p.z - center.1) * scale,
        )
    };
    // frame and ground line z = 0
    for x in ox..ox + PANEL {
        cv.put(x as i64, oy as i64, [200, 200, 200]);
        cv.put(x as i64, (oy + PANEL - 1) as i64, [200, 200, 200]);
    }
    for y in oy..oy + PANEL {
        cv.put(ox as i64, y as i64, [200, 200, 200]);
    }
    let g = proj(Vec3::ZERO).1;
    cv.line(
        (ox as f32, g),
        ((ox + PANEL - 1) as f32, g),
        [120, 200, 120],
    );
    for t in indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| proj(pts[i as usize]));
        cv.line(a, b, [150, 150, 170]);
        cv.line(b, c, [150, 150, 170]);
        cv.line(c, a, [150, 150, 170]);
    }
    for (i, t) in pose.iter().enumerate() {
        if let Some(p) = parents[i] {
            cv.line(proj(pose[p].translation), proj(t.translation), [220, 0, 0]);
        }
    }
    for t in pose {
        cv.dot(proj(t.translation), 1, [140, 0, 0]);
    }
}

fn render(o: &Opts) -> Result<String, CmdError> {
    let outp = o
        .out
        .clone()
        .ok_or_else(|| CmdError::Usage("--out is required".into()))?;
    let c = load_character(o)?;
    let parents: Vec<Option<usize>> = c.skeleton.bones.iter().map(|b| b.parent).collect();
    let mut columns: Vec<(String, Vec<Vec3>, Vec<Transform>)> = vec![(
        "bind".into(),
        c.mesh.positions.clone(),
        c.skeleton.bind_globals(),
    )];
    if let Some(seq) = o.seqs.first() {
        let cl = clip(&c, seq)?;
        let map = c
            .anims
            .as_ref()
            .map(|s| s.bone_map(&c.skeleton))
            .unwrap_or_default();
        let frames = if o.frames.is_empty() {
            vec![0.0]
        } else {
            o.frames.clone()
        };
        for f in frames {
            let pose = evaluate_pose(&c.skeleton, cl, &map, f, true);
            let pts = c.mesh.skin(&c.skeleton, &pose);
            columns.push((format!("{}@{f}", cl.name), pts, pose));
        }
    }
    // Common scale from the union of all columns.
    let mut min = Vec3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut max = -min;
    for (_, pts, _) in &columns {
        let (a, b) = validate::bounds(pts);
        min = min.min(a);
        max = max.max(b);
    }
    let span = (max - min).x.max((max - min).y).max((max - min).z).max(1.0);
    let scale = (PANEL as f32 * 0.9) / span;
    let mid = (min + max) * 0.5;
    let mut cv = Canvas::new(PANEL * columns.len(), PANEL * 2);
    for (k, (_, pts, pose)) in columns.iter().enumerate() {
        draw_panel(
            &mut cv,
            k * PANEL,
            0,
            0,
            (mid.x, mid.z),
            scale,
            pts,
            &c.mesh.indices,
            pose,
            &parents,
        );
        draw_panel(
            &mut cv,
            k * PANEL,
            PANEL,
            1,
            (mid.y, mid.z),
            scale,
            pts,
            &c.mesh.indices,
            pose,
            &parents,
        );
    }
    let png = encode_png(cv.w as u32, cv.h as u32, &cv.px);
    std::fs::write(&outp, png)
        .map_err(|e| CmdError::Io(format!("cannot write {}: {e}", outp.display())))?;
    let labels: Vec<&str> = columns.iter().map(|c| c.0.as_str()).collect();
    Ok(format!(
        "wrote {} ({}x{}): columns {:?}; rows front (X,Z) / side (Y,Z); scale {:.3} px/unit\n",
        outp.display(),
        cv.w,
        cv.h,
        labels,
        scale
    ))
}

// ------------------------------------------------------------------ OBJ export

fn export(o: &Opts) -> Result<String, CmdError> {
    let outp = o
        .out
        .clone()
        .ok_or_else(|| CmdError::Usage("--out is required".into()))?;
    let c = load_character(o)?;
    let (label, pts, pose) = match o.seqs.first() {
        Some(seq) => {
            let cl = clip(&c, seq)?;
            let map = c
                .anims
                .as_ref()
                .map(|s| s.bone_map(&c.skeleton))
                .unwrap_or_default();
            let f = o.frame.unwrap_or(0.0);
            let pose = evaluate_pose(&c.skeleton, cl, &map, f, true);
            (
                format!("{}@{f}", cl.name),
                c.mesh.skin(&c.skeleton, &pose),
                pose,
            )
        }
        None => (
            "bind".to_owned(),
            c.mesh.positions.clone(),
            c.skeleton.bind_globals(),
        ),
    };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# xiii-tool anim export: {} {label} (source coordinates, Z up)",
        c.mesh_name
    );
    let _ = writeln!(s, "o {}", c.mesh_name);
    for p in &pts {
        let _ = writeln!(s, "v {} {} {}", p.x, p.y, p.z);
    }
    for uv in &c.mesh.uvs {
        let _ = writeln!(s, "vt {} {}", uv[0], 1.0 - uv[1]);
    }
    for t in c.mesh.indices.as_chunks::<3>().0 {
        let _ = writeln!(s, "f {0}/{0} {1}/{1} {2}/{2}", t[0] + 1, t[1] + 1, t[2] + 1);
    }
    let base = pts.len();
    let _ = writeln!(s, "o {}_bones", c.mesh_name);
    for t in &pose {
        let p = t.translation;
        let _ = writeln!(s, "v {} {} {}", p.x, p.y, p.z);
    }
    for (i, b) in c.skeleton.bones.iter().enumerate() {
        if let Some(p) = b.parent {
            let _ = writeln!(s, "l {} {}", base + p + 1, base + i + 1);
        }
    }
    std::fs::write(&outp, s)
        .map_err(|e| CmdError::Io(format!("cannot write {}: {e}", outp.display())))?;
    Ok(format!("wrote {} ({label})\n", outp.display()))
}

// ------------------------------------------------------------------ tiny PNG encoder

fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, e) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *e = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// RGB8 PNG with uncompressed (stored) deflate blocks.
fn encode_png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for row in rgb.chunks_exact(w as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        z.extend([1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        z.extend(len.to_le_bytes());
        z.extend((!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend(adler32(&raw).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend(crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_structure() {
        let png = encode_png(2, 1, &[255, 0, 0, 0, 255, 0]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // IDAT: zlib header + one final stored block of 7 bytes (filter + 6) + adler
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap());
        assert_eq!(idat_len, 2 + 5 + 7 + 4);
        assert_eq!(png[43], 1);
    }

    #[test]
    fn options_parse() {
        let args: Vec<String> = ["--package", "x", "--seq", "Walk", "--frames", "0, 5,10"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_opts(&args).unwrap();
        assert_eq!(o.frames, vec![0.0, 5.0, 10.0]);
        assert_eq!(o.seqs, vec!["Walk".to_owned()]);
        assert!(parse_opts(&["--frame".to_owned()]).is_err());
    }
}
