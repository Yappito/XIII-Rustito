//! CPU-side world import for the diagnostic map viewer. No Bevy types: this module turns a
//! map from an owned installation into converted (Bevy-space) meshes, RGBA textures and
//! object records, and counts everything it could not import. Nothing falls back silently:
//! every skipped actor/material/surface increments a named counter that the overlay shows.
//!
//! Actor placement uses **effective** values: the map's tagged property if present, else the
//! inherited class default resolved read-only through `xiii_script` (`Vm::class_layout`), else
//! the documented `Engine.Actor` default (Location 0, Rotation 0, DrawScale 1, DrawScale3D 1,
//! PrePivot 0). The source of every field is counted as `placement.<field>.<source>` and
//! `actor.player_start.<field>.<source>`. `PrePivot` is applied before scale/rotation (see
//! `xiii_decode::common::actor_to_bevy_pre_pivot`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use xiii_decode::common::{
    BevyTransform, Mat3, actor_to_bevy_pre_pivot, to_bevy_direction, to_bevy_position,
};
use xiii_decode::model::{self, level, poly_flags};
use xiii_decode::static_mesh::decode_static_mesh;
use xiii_decode::terrain;
use xiii_decode::texture::{RgbaImage, Texture, TextureFormat, decode_texture};
use xiii_install::{Installation, OpenOptions};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue};
use xiii_script::{ScriptLimits, ScriptPackage, ScriptSet, Vm, VmLimits};

/// Parsed package bytes.
pub struct Loaded {
    /// Logical name as requested.
    pub name: String,
    /// File bytes.
    pub data: Vec<u8>,
    /// Parsed tables.
    pub package: Package,
}

/// How a texture's alpha should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaKind {
    /// Ignore alpha.
    Opaque,
    /// Alpha test.
    Mask,
    /// Alpha blend.
    Blend,
}

/// Decoded texture ready for upload.
pub struct SceneTexture {
    /// `Package.Object` label.
    pub label: String,
    /// Mip 0 as RGBA8.
    pub image: RgbaImage,
    /// Alpha usage.
    pub alpha: AlphaKind,
    /// Source size in texels (for BSP UV scaling).
    pub size: [u32; 2],
}

/// Material slot of a mesh.
#[derive(Debug, Clone, PartialEq)]
pub enum MaterialSlot {
    /// A decoded texture.
    Texture(usize),
    /// No texture could be resolved; diagnostic colour with the reason.
    Missing(String),
}

/// Mesh in Bevy space (metres, right-handed), counter-clockwise front faces.
pub struct SceneMesh {
    /// Label for diagnostics.
    pub label: String,
    /// Positions.
    pub positions: Vec<[f32; 3]>,
    /// Normals.
    pub normals: Vec<[f32; 3]>,
    /// UVs.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// Material.
    pub material: MaterialSlot,
}

/// Placed object (one per mesh section instance).
pub struct SceneObject {
    /// Index into [`WorldScene::meshes`].
    pub mesh: usize,
    /// Placement in Bevy space.
    pub transform: BevyTransform,
    /// Source object path (map export path, plus the mesh path).
    pub path: String,
    /// Resolved effective placement values (map + class-default + engine-default fallbacks).
    pub placement: Option<ResolvedPlacement>,
}

/// Imported world.
#[derive(Default)]
pub struct WorldScene {
    /// Meshes.
    pub meshes: Vec<SceneMesh>,
    /// Textures.
    pub textures: Vec<SceneTexture>,
    /// Objects.
    pub objects: Vec<SceneObject>,
    /// Player start position (Bevy space) and Unreal rotator.
    pub player_start: Option<([f32; 3], [i32; 3])>,
    /// Named counters ("imported" and "skipped" categories).
    pub counters: BTreeMap<String, usize>,
    /// First examples per failure category.
    pub examples: BTreeMap<String, String>,
    /// Collision triangles in Bevy space with an index into `collision_sources`.
    /// Static meshes contribute their decoded collision set 0 (not render triangles), the
    /// BSP its node polygons without `PF_NotSolid`/portal flags (invisible walls included),
    /// the terrain its visible quads.
    pub collision: Vec<([[f32; 3]; 3], u32)>,
    /// Source object path per collision index.
    pub collision_sources: Vec<String>,
}

impl WorldScene {
    fn add_collision(&mut self, source: String, tris: impl IntoIterator<Item = [[f32; 3]; 3]>) {
        let id = self.collision_sources.len() as u32;
        self.collision_sources.push(source);
        let before = self.collision.len();
        self.collision.extend(tris.into_iter().map(|t| (t, id)));
        let n = self.collision.len() - before;
        self.count("collision.triangles", n);
    }

    /// Nearest collision hit along a ray (Bevy space): distance and source path.
    pub fn ray_collision(&self, origin: [f32; 3], dir: [f32; 3]) -> Option<(f32, &str)> {
        let mut best: Option<(f32, u32)> = None;
        for (t, src) in &self.collision {
            if let Some(d) = ray_triangle(origin, dir, t)
                && best.is_none_or(|(b, _)| d < b)
            {
                best = Some((d, *src));
            }
        }
        best.map(|(d, s)| (d, self.collision_sources[s as usize].as_str()))
    }

    fn count(&mut self, key: &str, n: usize) {
        *self.counters.entry(key.to_owned()).or_default() += n;
    }

    fn fail(&mut self, key: &str, example: String) {
        self.count(key, 1);
        self.examples.entry(key.to_owned()).or_insert(example);
    }

    /// Counters whose name marks a problem (`skip.`/`fail.`), summed.
    pub fn problem_total(&self) -> usize {
        self.counters
            .iter()
            .filter(|(k, _)| k.starts_with("skip.") || k.starts_with("fail."))
            .map(|(_, v)| v)
            .sum()
    }
}

/// Lazily loaded packages of one installation.
pub struct PackageCache {
    install: Installation,
    loaded: HashMap<String, Option<Arc<Loaded>>>,
}

impl PackageCache {
    /// Opens an installation read-only.
    pub fn open(game_dir: &std::path::Path) -> Result<Self, String> {
        let install =
            Installation::open(game_dir, &OpenOptions::default()).map_err(|e| e.to_string())?;
        Ok(Self {
            install,
            loaded: HashMap::new(),
        })
    }

    /// Loads a package by logical name (cached; failures cached as `None`).
    pub fn get(&mut self, name: &str) -> Result<Arc<Loaded>, String> {
        let key = name.to_ascii_lowercase();
        if let Some(v) = self.loaded.get(&key) {
            return v
                .clone()
                .ok_or_else(|| format!("package {name} failed earlier"));
        }
        let result = (|| -> Result<Arc<Loaded>, String> {
            let resolved = self
                .install
                .resolve_package(name)
                .map_err(|e| e.to_string())?;
            let data = std::fs::read(resolved.path())
                .map_err(|e| format!("{}: {e}", resolved.path().display()))?;
            let package =
                Package::parse(&data, &Limits::default()).map_err(|e| format!("{name}: {e}"))?;
            Ok(Arc::new(Loaded {
                name: name.to_owned(),
                data,
                package,
            }))
        })();
        self.loaded.insert(key, result.as_ref().ok().cloned());
        result
    }

    /// Loads a map package.
    pub fn map(&mut self, name: &str) -> Result<Arc<Loaded>, String> {
        let resolved = self.install.resolve_map(name).map_err(|e| e.to_string())?;
        let stem = resolved.entry.name.clone();
        self.get(&format!("{stem}.unr"))
    }

    /// Installation root the cache was opened from (for loading the `.u` class defaults).
    pub fn root(&self) -> &std::path::Path {
        self.install.root()
    }

    /// Resolves an object reference of `from` to (package, export index). Imports are
    /// followed into their root package and matched by path and class name.
    pub fn resolve(
        &mut self,
        from: &Arc<Loaded>,
        r: ObjectRef,
    ) -> Result<(Arc<Loaded>, usize), String> {
        match r {
            ObjectRef::Null => Err("null reference".into()),
            ObjectRef::Export(i) => Ok((from.clone(), i as usize)),
            ObjectRef::Import(i) => {
                let p = &from.package;
                let path = p.object_path(r).ok_or("bad import path")?.to_owned();
                let class = p.name(p.imports()[i as usize].class_name).to_owned();
                let (root, rest) = path
                    .split_once('.')
                    .ok_or_else(|| format!("import {path} has no package"))?;
                let target = self.get(root)?;
                let tp = &target.package;
                let idx = (0..tp.exports().len())
                    .find(|&e| {
                        tp.object_path(ObjectRef::Export(e as u32))
                            .is_some_and(|q| q.eq_ignore_ascii_case(rest))
                            && tp
                                .export_class_path(e)
                                .and_then(|c| c.rsplit('.').next())
                                .is_some_and(|c| c.eq_ignore_ascii_case(&class))
                    })
                    .ok_or_else(|| format!("{path} ({class}) not found in {root}"))?;
                Ok((target, idx))
            }
        }
    }
}

/// Material classes whose texture is reached through an object property (first present
/// candidate wins). Classes from the GOG corpus coverage: Shader 751, TexOscillator 94,
/// TexPanner 83, SinusModifier 40 (XIII-specific), FinalBlend 23, TexEnvMap 16, TexRotator 13.
const MATERIAL_LINKS: &[(&str, &[&str])] = &[
    ("Shader", &["Diffuse", "SelfIllumination", "Specular"]),
    ("FinalBlend", &["Material"]),
    ("TexPanner", &["Material"]),
    ("TexRotator", &["Material"]),
    ("TexScaler", &["Material"]),
    ("TexOscillator", &["Material"]),
    ("TexEnvMap", &["Material"]),
    ("TexCoordSource", &["Material"]),
    ("TexModifier", &["Material"]),
    ("SinusModifier", &["Material"]),
    ("Combiner", &["Material1", "Material2"]),
    ("ColorModifier", &["Material"]),
];

/// (lower-case package name, export index).
type ObjectKey = (String, usize);
/// Converted static mesh: (scene mesh index, label) per section and local collision
/// triangles (Bevy space, unscaled).
#[derive(Clone)]
struct MeshSections {
    sections: Vec<(usize, String)>,
    collision: Arc<Vec<[[f32; 3]; 3]>>,
    /// Collision triangles of set 0 whose material slot has `EnableCollision` = false. UE2
    /// would not block the player with these; they are counted (not silently kept or dropped).
    collision_slot_disabled: usize,
}

struct Importer<'a> {
    cache: &'a mut PackageCache,
    scene: WorldScene,
    textures: HashMap<ObjectKey, Result<usize, String>>,
    meshes: HashMap<ObjectKey, Result<MeshSections, String>>,
}

impl Importer<'_> {
    fn texture_from(
        &mut self,
        from: &Arc<Loaded>,
        r: ObjectRef,
        depth: u32,
    ) -> Result<usize, String> {
        if depth > 8 {
            return Err("material chain too deep".into());
        }
        let (pkg, idx) = self.cache.resolve(from, r)?;
        let key = (pkg.name.to_ascii_lowercase(), idx);
        if let Some(v) = self.textures.get(&key) {
            return v.clone();
        }
        let class = pkg.package.export_class_path(idx).unwrap_or("?").to_owned();
        let short = class.rsplit('.').next().unwrap_or("").to_owned();
        let result = if short.eq_ignore_ascii_case("Texture") {
            decode_texture(&pkg.package, &pkg.data, idx)
                .map_err(|e| format!("texture decode: {e}"))
                .and_then(|t| {
                    let img = t
                        .decode_mip(0)
                        .map_err(|e| format!("texture pixels: {e}"))?;
                    let alpha = alpha_kind(&t, &img);
                    let label = format!(
                        "{}.{}",
                        pkg.name,
                        pkg.package
                            .object_path(ObjectRef::Export(idx as u32))
                            .unwrap_or("?")
                    );
                    self.scene.textures.push(SceneTexture {
                        label,
                        image: img,
                        alpha,
                        size: t.size,
                    });
                    Ok(self.scene.textures.len() - 1)
                })
        } else if let Some((_, names)) = MATERIAL_LINKS
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(&short))
        {
            let next = pkg
                .package
                .read_object_properties(&pkg.data, idx, &Limits::default())
                .ok()
                .and_then(|props| {
                    names.iter().find_map(|name| {
                        props.block.properties.iter().find_map(|p| {
                            match (
                                pkg.package.property_name(p).eq_ignore_ascii_case(name),
                                &p.value,
                            ) {
                                (true, PropertyValue::Object(o)) if !o.is_null() => Some(*o),
                                _ => None,
                            }
                        })
                    })
                });
            match next {
                Some(o) => self.texture_from(&pkg, o, depth + 1),
                None => Err(format!("{class} without {}", names.join("/"))),
            }
        } else {
            Err(format!("unsupported material class {class}"))
        };
        self.textures.insert(key, result.clone());
        result
    }

    fn material(&mut self, from: &Arc<Loaded>, r: ObjectRef, what: &str) -> MaterialSlot {
        if r.is_null() {
            self.scene.count(&format!("skip.{what}.material_none"), 1);
            return MaterialSlot::Missing("None".into());
        }
        match self.texture_from(from, r, 0) {
            Ok(t) => MaterialSlot::Texture(t),
            Err(e) => {
                let cat = e.split(':').next().unwrap_or("").to_owned();
                let label = from.package.object_path(r).unwrap_or("?").to_owned();
                self.scene.fail(
                    &format!("skip.{what}.material ({cat})"),
                    format!("{label}: {e}"),
                );
                MaterialSlot::Missing(e)
            }
        }
    }

    /// Converted per-section meshes of a static mesh, cached.
    fn static_mesh(&mut self, from: &Arc<Loaded>, r: ObjectRef) -> Result<MeshSections, String> {
        let (pkg, idx) = self.cache.resolve(from, r)?;
        let key = (pkg.name.to_ascii_lowercase(), idx);
        if let Some(v) = self.meshes.get(&key) {
            return v.clone();
        }
        let label = format!(
            "{}.{}",
            pkg.name,
            pkg.package
                .object_path(ObjectRef::Export(idx as u32))
                .unwrap_or("?")
        );
        let result = match decode_static_mesh(&pkg.package, &pkg.data, idx) {
            Err(e) => Err(format!("static mesh decode: {e}")),
            Ok(m) => {
                let positions: Vec<[f32; 3]> = m
                    .vertices
                    .iter()
                    .map(|v| to_bevy_position(v.position))
                    .collect();
                let normals: Vec<[f32; 3]> = m
                    .vertices
                    .iter()
                    .map(|v| to_bevy_direction(v.normal))
                    .collect();
                let uvs: Vec<[f32; 2]> = m
                    .uv_streams
                    .first()
                    .map(|s| s.uvs.clone())
                    .unwrap_or_else(|| vec![[0.0; 2]; m.vertices.len()]);
                let cs = &m.collision[0];
                let collision: Vec<[[f32; 3]; 3]> = cs
                    .triangles
                    .iter()
                    .map(|t| {
                        t.vertices
                            .map(|v| to_bevy_position(cs.vertices[v as usize]))
                    })
                    .collect();
                // Collision triangles whose material slot has EnableCollision = false: under
                // UE2 these do not block the player. Counted, not filtered (evidence only).
                let collision_slot_disabled = cs
                    .triangles
                    .iter()
                    .filter(|t| {
                        usize::try_from(t.material)
                            .ok()
                            .and_then(|si| m.materials.get(si))
                            .is_some_and(|mm| !mm.enable_collision)
                    })
                    .count();
                let mut out = Vec::new();
                for (si, s) in m.sections.iter().enumerate() {
                    if s.num_faces == 0 {
                        continue;
                    }
                    let material = match m.materials.get(si) {
                        Some(mm) => self.material(&pkg, mm.material, "mesh"),
                        None => {
                            self.scene.count("skip.mesh.section_without_material", 1);
                            MaterialSlot::Missing("no slot".into())
                        }
                    };
                    // Source winding is kept: the det -1 axis change makes it CCW.
                    let indices: Vec<u32> = m
                        .section_indices(si)
                        .iter()
                        .map(|&i| u32::from(i))
                        .collect();
                    self.scene.meshes.push(SceneMesh {
                        label: format!("{label}#{si}"),
                        positions: positions.clone(),
                        normals: normals.clone(),
                        uvs: uvs.clone(),
                        indices,
                        material,
                    });
                    out.push((self.scene.meshes.len() - 1, label.clone()));
                }
                Ok(MeshSections {
                    sections: out,
                    collision: Arc::new(collision),
                    collision_slot_disabled,
                })
            }
        };
        self.meshes.insert(key, result.clone());
        result
    }
}

/// Effective values of one actor: map property, else inherited class default, else the
/// documented `Engine.Actor` default. Sources are counted per field.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolvedPlacement {
    pub location: [f32; 3],
    pub rotation: [i32; 3],
    pub draw_scale: f32,
    pub draw_scale_3d: [f32; 3],
    pub pre_pivot: [f32; 3],
}

/// Loaded `.u` packages for resolving inherited class defaults.
pub struct ClassDefaults {
    set: ScriptSet,
    layouts: HashMap<String, std::rc::Rc<xiii_script::vm::ClassLayout>>,
}

type SharedLayout = std::rc::Rc<xiii_script::vm::ClassLayout>;

impl ClassDefaults {
    /// Loads every code package of the installation (read-only; ~10 packages).
    pub fn open(game_dir: &std::path::Path) -> Result<Self, String> {
        let install =
            Installation::open(game_dir, &OpenOptions::default()).map_err(|e| e.to_string())?;
        let mut set = ScriptSet::new();
        for entry in install.packages() {
            if entry.kind != xiii_install::PackageKind::Code {
                continue;
            }
            let data =
                std::fs::read(&entry.path).map_err(|e| format!("{}: {e}", entry.path.display()))?;
            let p = ScriptPackage::load(
                &entry.name,
                data,
                &ScriptLimits::default(),
                &Limits::default(),
            )
            .map_err(|e| format!("{}: {e}", entry.path.display()))?;
            set.add(p);
        }
        Ok(Self {
            set,
            layouts: HashMap::new(),
        })
    }

    /// Resolved layout of a class path (`Package.Class` as written in the map), cached.
    fn layout(&mut self, class_path: &str) -> Result<SharedLayout, String> {
        let key = class_path.to_ascii_lowercase();
        if let Some(l) = self.layouts.get(&key) {
            return Ok(l.clone());
        }
        let (pkg, class) = class_path
            .split_once('.')
            .ok_or_else(|| format!("class {class_path:?} is not Package.Class"))?;
        let pi = self
            .set
            .package_index(pkg)
            .ok_or_else(|| format!("package {pkg} not loaded"))?;
        let e = self.set.packages[pi]
            .export_by_path(class)
            .ok_or_else(|| format!("class {class_path} not found in {pkg}"))?;
        let class_ref = xiii_script::GlobalRef {
            package: pi,
            export: e,
        };
        let mut vm = Vm::new(&self.set, VmLimits::default());
        let l = vm
            .class_layout(class_ref)
            .map_err(|err| format!("class layout of {class_path}: {err}"))?;
        self.layouts.insert(key, l.clone());
        Ok(l)
    }

    /// Resolves placement values of one actor: map property, else class default, else
    /// documented `Engine.Actor` default. Returns the values and which fields came from
    /// map / class-default / engine-default.
    pub fn resolve(
        &mut self,
        class_path: &str,
        a: &level::ActorPlacement,
    ) -> Result<(ResolvedPlacement, [level::PlacementSource; 5]), String> {
        let l = self.layout(class_path)?;
        let get_f = |name: &str| -> Option<f32> {
            let s = l.slot_by_name(name)?;
            match l.defaults.get(s.base) {
                Some(xiii_script::Value::Float(v)) => Some(*v),
                _ => None,
            }
        };
        let get_v = |name: &str| -> Option<[f32; 3]> {
            let s = l.slot_by_name(name)?;
            match l.defaults.get(s.base) {
                Some(xiii_script::Value::Vector(v)) => Some(*v),
                _ => None,
            }
        };
        let get_r = |name: &str| -> Option<[i32; 3]> {
            let s = l.slot_by_name(name)?;
            match l.defaults.get(s.base) {
                Some(xiii_script::Value::Rotator(v)) => Some(*v),
                _ => None,
            }
        };
        let pick_v = |map: Option<[f32; 3]>, class: Option<[f32; 3]>, engine: [f32; 3]| {
            let src = |a: bool, b: bool| {
                if a {
                    level::PlacementSource::MapProperty
                } else if b {
                    level::PlacementSource::ClassDefault
                } else {
                    level::PlacementSource::EngineDefault
                }
            };
            (
                map.or(class).unwrap_or(engine),
                src(map.is_some(), class.is_some()),
            )
        };
        let pick_e = |map: Option<f32>, class: Option<f32>, engine: f32| {
            let src = |a: bool, b: bool| {
                if a {
                    level::PlacementSource::MapProperty
                } else if b {
                    level::PlacementSource::ClassDefault
                } else {
                    level::PlacementSource::EngineDefault
                }
            };
            (
                map.or(class).unwrap_or(engine),
                src(map.is_some(), class.is_some()),
            )
        };
        let (location, s_location) = pick_v(a.location, get_v("Location"), [0.0; 3]);
        let (rotation, s_rotation) = {
            let class = get_r("Rotation");
            let v = match a.rotation {
                Some(v) => Some(v),
                None => class,
            };
            let src = if a.rotation.is_some() {
                level::PlacementSource::MapProperty
            } else if class.is_some() {
                level::PlacementSource::ClassDefault
            } else {
                level::PlacementSource::EngineDefault
            };
            (v.unwrap_or([0; 3]), src)
        };
        let (draw_scale, s_draw_scale) = pick_e(a.draw_scale, get_f("DrawScale"), 1.0);
        let (draw_scale_3d, s_scale_3d) = pick_v(a.draw_scale_3d, get_v("DrawScale3D"), [1.0; 3]);
        let (pre_pivot, s_pre_pivot) = pick_v(a.pre_pivot, get_v("PrePivot"), [0.0; 3]);
        Ok((
            ResolvedPlacement {
                location,
                rotation,
                draw_scale,
                draw_scale_3d,
                pre_pivot,
            },
            [
                s_location,
                s_rotation,
                s_draw_scale,
                s_scale_3d,
                s_pre_pivot,
            ],
        ))
    }
}

/// Bakes a resolved placement into a Bevy transform with PrePivot applied before
/// scale/rotation.
pub fn placement_transform(a: &ResolvedPlacement) -> BevyTransform {
    let src_scale = [
        a.draw_scale_3d[0] * a.draw_scale,
        a.draw_scale_3d[1] * a.draw_scale,
        a.draw_scale_3d[2] * a.draw_scale,
    ];
    actor_to_bevy_pre_pivot(a.location, a.rotation, src_scale, a.pre_pivot)
}

fn alpha_kind(t: &Texture, img: &RgbaImage) -> AlphaKind {
    let has_alpha = img.pixels.as_chunks::<4>().0.iter().any(|p| p[3] < 250);
    if !has_alpha {
        AlphaKind::Opaque
    } else if t.alpha_texture && !t.masked && t.format != TextureFormat::Dxt1 {
        AlphaKind::Blend
    } else {
        AlphaKind::Mask
    }
}

/// Applies a [`BevyTransform`] to a Bevy-space point: per-axis scale, rotation, translation.
pub fn apply_transform_pub(t: &BevyTransform, v: [f32; 3]) -> [f32; 3] {
    apply_transform(t, v)
}

fn apply_transform(t: &BevyTransform, v: [f32; 3]) -> [f32; 3] {
    let s = [v[0] * t.scale[0], v[1] * t.scale[1], v[2] * t.scale[2]];
    let r = &t.rotation;
    std::array::from_fn(|i| r[i][0] * s[0] + r[i][1] * s[1] + r[i][2] * s[2] + t.translation[i])
}

/// Moller-Trumbore ray/triangle distance (both sides).
pub fn ray_triangle(o: [f32; 3], d: [f32; 3], t: &[[f32; 3]; 3]) -> Option<f32> {
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let e1 = sub(t[1], t[0]);
    let e2 = sub(t[2], t[0]);
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(o, t[0]);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = dot(e2, q) * inv;
    (dist > 1e-5).then_some(dist)
}

fn identity() -> BevyTransform {
    BevyTransform {
        translation: [0.0; 3],
        rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        scale: [1.0; 3],
    }
}

/// Imports a map: static-mesh actors, the level BSP and terrain.
///
/// Actor placement resolves Location/Rotation/DrawScale/DrawScale3D/PrePivot from the map's
/// tagged properties, falling back to the inherited class defaults (`Vm::class_layout`) and
/// then to the documented `Engine.Actor` defaults; every fallback is counted
/// (`placement.<field>.<source>`), never applied silently.
pub fn import_map(cache: &mut PackageCache, map: &str) -> Result<WorldScene, String> {
    let root = cache.root().to_path_buf();
    let map_pkg = cache.map(map)?;
    let mut im = Importer {
        cache,
        scene: WorldScene::default(),
        textures: HashMap::new(),
        meshes: HashMap::new(),
    };
    let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
    im.scene
        .count("actor.property_failures", actors.failures.len());
    let mut defaults =
        ClassDefaults::open(&root).map_err(|e| format!("loading class defaults: {e}"))?;
    if let Some(ps) = actors.player_starts.first() {
        let (eff, src) = defaults
            .resolve(&ps.class, ps)
            .map_err(|e| format!("PlayerStart {}: {e}", ps.path))?;
        count_placement_sources(&mut im.scene, "actor.player_start", &src);
        im.scene.player_start = Some((to_bevy_position(eff.location), eff.rotation));
    }
    im.scene
        .count("actor.player_starts", actors.player_starts.len());

    for a in &actors.static_mesh_actors {
        let class_short = a.class.rsplit('.').next().unwrap_or("").to_owned();
        if class_short.ends_with("Emitter") {
            // Particle-emitter objects carry the particle's StaticMesh, not a placement.
            im.scene
                .count(&format!("skip.actor.particle_mesh ({class_short})"), 1);
            continue;
        }
        if a.hidden {
            im.scene
                .count(&format!("skip.actor.hidden ({class_short})"), 1);
            continue;
        }
        if let Some(dt) = a.draw_type
            && dt != 8
        {
            im.scene
                .count(&format!("skip.actor.draw_type_{dt} ({class_short})"), 1);
            continue;
        }
        let Some(r) = a.static_mesh else { continue };
        let (eff, src) = match defaults.resolve(&a.class, a) {
            Ok(v) => v,
            Err(e) => {
                im.scene
                    .fail(&format!("fail.actor.class_defaults ({class_short})"), e);
                continue;
            }
        };
        for (field, s) in PLACEMENT_FIELDS.iter().zip(src) {
            let name = source_name(source_index(s));
            im.scene.count(&format!("placement.{field}.{name}"), 1);
        }
        match im.static_mesh(&map_pkg, r) {
            Ok(converted) => {
                im.scene
                    .count(&format!("actor.static_mesh ({class_short})"), 1);
                let transform = placement_transform(&eff);
                let non_default_pivot = eff.pre_pivot.iter().any(|v| v.abs() > 1e-6);
                if non_default_pivot {
                    im.scene
                        .count(&format!("actor.pre_pivot_applied ({class_short})"), 1);
                }
                if converted.collision_slot_disabled > 0 {
                    // Under UE2 these collision triangles do not block the player; reported,
                    // not filtered, because the import has no verified per-slot rule yet.
                    im.scene.count(
                        &format!("note.collision.mesh_slot_disabled ({class_short})"),
                        converted.collision_slot_disabled,
                    );
                    im.scene
                        .examples
                        .entry(format!("note.collision.mesh_slot_disabled ({class_short})"))
                        .or_insert_with(|| {
                            format!(
                                "{0}: {1} collision triangles",
                                a.path, converted.collision_slot_disabled
                            )
                        });
                }
                let mut label0 = String::new();
                for (mesh, label) in converted.sections {
                    label0.clone_from(&label);
                    im.scene.objects.push(SceneObject {
                        mesh,
                        transform,
                        path: format!("{} -> {label}", a.path),
                        placement: Some(eff),
                    });
                }
                // Collision: explicit bCollideActors/bBlockPlayers = false excludes the actor.
                let [collide, _, block_players] = a.collision_flags;
                if collide == Some(false) || block_players == Some(false) {
                    im.scene.count(
                        &format!("note.collision.actor_non_blocking ({class_short})"),
                        1,
                    );
                    im.scene
                        .examples
                        .entry(format!("note.collision.actor_non_blocking ({class_short})"))
                        .or_insert_with(|| {
                            format!(
                                "{} [bCollideActors={:?} bBlockActors={:?} bBlockPlayers={:?}] -> {label0}",
                                a.path, collide, a.collision_flags[1], block_players
                            )
                        });
                } else {
                    let tris: Vec<[[f32; 3]; 3]> = converted
                        .collision
                        .iter()
                        .map(|t| t.map(|v| apply_transform(&transform, v)))
                        .collect();
                    im.scene
                        .add_collision(format!("{} -> {label0}", a.path), tris);
                }
            }
            Err(e) => im.scene.fail(
                &format!("fail.actor.static_mesh ({class_short})"),
                format!("{}: {e}", a.path),
            ),
        }
    }

    import_bsp(&mut im, &map_pkg);
    import_terrain(&mut im, &map_pkg);
    Ok(im.scene)
}

fn source_index(s: level::PlacementSource) -> usize {
    match s {
        level::PlacementSource::MapProperty => 0,
        level::PlacementSource::ClassDefault => 1,
        level::PlacementSource::EngineDefault => 2,
    }
}

fn source_name(i: usize) -> &'static str {
    ["map", "class_default", "engine_default"][i]
}

/// Field names of a resolved placement, in the `[PlacementSource; 5]` order.
const PLACEMENT_FIELDS: [&str; 5] = [
    "location",
    "rotation",
    "drawscale",
    "drawscale3d",
    "prepivot",
];

/// Counts the source of each placement field (map / class default / engine default).
fn count_placement_sources(
    scene: &mut WorldScene,
    prefix: &str,
    sources: &[level::PlacementSource; 5],
) {
    for (field, s) in PLACEMENT_FIELDS.iter().zip(sources) {
        let name = source_name(source_index(*s));
        scene.count(&format!("{prefix}.{field}.{name}"), 1);
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn import_bsp(im: &mut Importer<'_>, map_pkg: &Arc<Loaded>) {
    let p = &map_pkg.package;
    let idx = match model::find_level_model(p, &map_pkg.data) {
        Ok(i) => i,
        Err(e) => return im.scene.fail("fail.bsp.level_model", e.to_string()),
    };
    let m = match model::decode_model(p, &map_pkg.data, idx) {
        Ok(m) => m,
        Err(e) => return im.scene.fail("fail.bsp.decode", e.to_string()),
    };
    if m.report.unsupported_tail.is_some() {
        im.scene.count(
            "note.bsp.model_tail_not_decoded (zones/lightmaps/leaves)",
            1,
        );
    }
    // Group triangles per surface material.
    let mut groups: BTreeMap<i64, (MaterialSlot, SceneMesh)> = BTreeMap::new();
    let mut bsp_collision = Vec::new();
    for poly in m.polygons() {
        let surf = m.surfs[poly.surf];
        if surf.poly_flags & (poly_flags::NOT_SOLID | poly_flags::PORTAL) == 0 {
            let v: Vec<[f32; 3]> = poly.vertices.iter().map(|&p| to_bevy_position(p)).collect();
            for k in 1..v.len() - 1 {
                bsp_collision.push([v[0], v[k], v[k + 1]]);
            }
        } else {
            im.scene.count("note.collision.bsp_non_solid_polygons", 1);
        }
        if surf.poly_flags & poly_flags::INVISIBLE != 0 {
            im.scene
                .count("note.bsp.invisible_polygons (not drawn, by flag)", 1);
            continue;
        }
        if surf.poly_flags & poly_flags::PORTAL != 0 {
            im.scene
                .count("note.bsp.portal_polygons (not drawn, by flag)", 1);
            continue;
        }
        if surf.poly_flags & poly_flags::FAKE_BACKDROP != 0 {
            im.scene
                .count("skip.bsp.sky_backdrop_polygons (skybox not rendered)", 1);
            continue;
        }
        let key = i64::from(surf.material.raw());
        if let std::collections::btree_map::Entry::Vacant(slot) = groups.entry(key) {
            let material = im.material(map_pkg, surf.material, "bsp");
            slot.insert((
                material.clone(),
                SceneMesh {
                    label: format!("BSP {}", p.object_path(surf.material).unwrap_or("None")),
                    positions: Vec::new(),
                    normals: Vec::new(),
                    uvs: Vec::new(),
                    indices: Vec::new(),
                    material,
                },
            ));
        }
        let (material, mesh) = groups.get_mut(&key).expect("inserted");
        let tex_size = match material {
            MaterialSlot::Texture(t) => im.scene.textures[*t].size,
            MaterialSlot::Missing(_) => [64, 64],
        };
        let base = m.points[surf.base as usize];
        let tu = m.vectors[surf.texture_u as usize];
        let tv = m.vectors[surf.texture_v as usize];
        let n = to_bevy_direction(m.vectors[surf.normal as usize]);
        let first = mesh.positions.len() as u32;
        for v in &poly.vertices {
            let d = sub(*v, base);
            mesh.positions.push(to_bevy_position(*v));
            mesh.normals.push(n);
            mesh.uvs.push([
                dot(d, tu) / tex_size[0] as f32,
                dot(d, tv) / tex_size[1] as f32,
            ]);
        }
        // BSP polygons wind opposite to static meshes in source space (measured: numeric
        // normal along the node plane), so each fan triangle is reversed.
        for k in 1..poly.vertices.len() as u32 - 1 {
            mesh.indices
                .extend_from_slice(&[first, first + k + 1, first + k]);
        }
        im.scene.count("bsp.polygons", 1);
    }
    im.scene.add_collision(
        format!(
            "{} (BSP)",
            p.object_path(ObjectRef::Export(idx as u32)).unwrap_or("?")
        ),
        bsp_collision,
    );
    for (_, (_, mesh)) in groups {
        let label = mesh.label.clone();
        im.scene.meshes.push(mesh);
        im.scene.objects.push(SceneObject {
            mesh: im.scene.meshes.len() - 1,
            transform: identity(),
            path: format!(
                "{} {label}",
                p.object_path(ObjectRef::Export(idx as u32)).unwrap_or("?")
            ),
            placement: None,
        });
    }
}

fn import_terrain(im: &mut Importer<'_>, map_pkg: &Arc<Loaded>) {
    let p = &map_pkg.package;
    for i in 0..p.exports().len() {
        if !p
            .export_class_path(i)
            .is_some_and(|c| c.eq_ignore_ascii_case(terrain::TERRAIN_INFO_CLASS))
        {
            continue;
        }
        let path = p
            .object_path(ObjectRef::Export(i as u32))
            .unwrap_or("?")
            .to_owned();
        let t = match terrain::decode_terrain_info(p, &map_pkg.data, i) {
            Ok(t) => t,
            Err(e) => {
                im.scene.fail("fail.terrain.decode", e.to_string());
                continue;
            }
        };
        let Some(map_ref) = t.terrain_map else {
            im.scene.fail("fail.terrain.no_heightmap", path);
            continue;
        };
        let (w, h) = match im.cache.resolve(map_pkg, map_ref).and_then(|(pk, ix)| {
            decode_texture(&pk.package, &pk.data, ix).map_err(|e| e.to_string())
        }) {
            Ok(tex) => (tex.size[0] as usize, tex.size[1] as usize),
            Err(e) => {
                im.scene.fail("fail.terrain.heightmap", e);
                continue;
            }
        };
        let mesh = match t.mesh(w, h) {
            Ok(m) => m,
            Err(e) => {
                im.scene.fail("fail.terrain.mesh", e.to_string());
                continue;
            }
        };
        im.scene.count("terrain.hidden_quads", mesh.hidden_quads);
        let composite = composite_terrain_texture(im, map_pkg, &t, &mesh);
        let material = match composite {
            Some(img) => {
                im.scene.textures.push(SceneTexture {
                    label: format!("{path} layer composite"),
                    size: [img.width, img.height],
                    image: img,
                    alpha: AlphaKind::Opaque,
                });
                MaterialSlot::Texture(im.scene.textures.len() - 1)
            }
            None => MaterialSlot::Missing("terrain layers".into()),
        };
        let terrain_tris: Vec<[[f32; 3]; 3]> = mesh
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| t.map(|i| to_bevy_position(mesh.positions[i as usize])))
            .collect();
        im.scene
            .add_collision(format!("{path} (terrain)"), terrain_tris);
        let positions: Vec<[f32; 3]> = mesh
            .positions
            .iter()
            .map(|&v| to_bevy_position(v))
            .collect();
        let normals = grid_normals(&mesh.positions, w, h);
        im.scene.meshes.push(SceneMesh {
            label: format!("{path} heightfield"),
            positions,
            normals,
            uvs: mesh.grid_uv.clone(),
            indices: mesh.indices.clone(),
            material,
        });
        im.scene.objects.push(SceneObject {
            mesh: im.scene.meshes.len() - 1,
            transform: identity(),
            path,
            placement: None,
        });
        im.scene.count("terrain.infos", 1);
    }
}

fn grid_normals(pos: &[[f32; 3]], w: usize, h: usize) -> Vec<[f32; 3]> {
    let mut out = Vec::with_capacity(pos.len());
    for y in 0..h {
        for x in 0..w {
            let at = |xx: usize, yy: usize| pos[yy * w + xx];
            let dx = sub(at((x + 1).min(w - 1), y), at(x.saturating_sub(1), y));
            let dy = sub(at(x, (y + 1).min(h - 1)), at(x, y.saturating_sub(1)));
            // Source normal: up-facing cross product of the +X and +Y grid directions.
            let n = [
                dx[1] * dy[2] - dx[2] * dy[1],
                dx[2] * dy[0] - dx[0] * dy[2],
                dx[0] * dy[1] - dx[1] * dy[0],
            ];
            let n = if n[2] < 0.0 { [-n[0], -n[1], -n[2]] } else { n };
            let l = dot(n, n).sqrt().max(1e-6);
            out.push(to_bevy_direction([n[0] / l, n[1] / l, n[2] / l]));
        }
    }
    out
}

fn sample(img: &RgbaImage, u: f32, v: f32, wrap: bool) -> [f32; 4] {
    let (w, h) = (img.width as f32, img.height as f32);
    let (mut x, mut y) = (u * w - 0.5, v * h - 0.5);
    if !wrap {
        x = x.clamp(0.0, w - 1.0);
        y = y.clamp(0.0, h - 1.0);
    }
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let px = |xi: f32, yi: f32| -> [f32; 4] {
        let (wi, hi) = (img.width as i64, img.height as i64);
        let (mut xi, mut yi) = (xi as i64, yi as i64);
        if wrap {
            xi = xi.rem_euclid(wi);
            yi = yi.rem_euclid(hi);
        } else {
            xi = xi.clamp(0, wi - 1);
            yi = yi.clamp(0, hi - 1);
        }
        let o = ((yi * wi + xi) * 4) as usize;
        std::array::from_fn(|k| img.pixels[o + k] as f32)
    };
    let (a, b, c, d) = (
        px(x0, y0),
        px(x0 + 1.0, y0),
        px(x0, y0 + 1.0),
        px(x0 + 1.0, y0 + 1.0),
    );
    std::array::from_fn(|k| {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bot = c[k] + (d[k] - c[k]) * fx;
        top + (bot - top) * fy
    })
}

/// Bakes the terrain layers into one texture over the grid: each layer texture is tiled
/// in world space (`world / (UScale * USize)`, an assumption) and blended over the previous
/// result with the alpha channel of its alpha map. Diagnostic approximation of the
/// original multi-pass terrain shading, not a parity claim.
fn composite_terrain_texture(
    im: &mut Importer<'_>,
    map_pkg: &Arc<Loaded>,
    t: &terrain::TerrainInfo,
    mesh: &terrain::TerrainMesh,
) -> Option<RgbaImage> {
    let (w, h) = (mesh.width, mesh.height);
    let v0 = mesh.positions[0];
    let step_x = sub(mesh.positions[1], v0)[0];
    let step_y = sub(mesh.positions[w], v0)[1];
    let mut layers = Vec::new();
    for l in &t.layers {
        let tex = match im.texture_from(map_pkg, l.texture, 0) {
            Ok(i) => i,
            Err(e) => {
                im.scene.fail("skip.terrain.layer_texture", e);
                continue;
            }
        };
        let alpha = match im
            .cache
            .resolve(map_pkg, l.alpha_map)
            .and_then(|(pk, ix)| {
                decode_texture(&pk.package, &pk.data, ix).map_err(|e| e.to_string())
            })
            .and_then(|tx| tx.decode_mip(0).map_err(|e| e.to_string()))
        {
            Ok(a) => a,
            Err(e) => {
                im.scene.fail("skip.terrain.layer_alpha", e);
                continue;
            }
        };
        layers.push((tex, alpha, l.u_scale, l.v_scale));
    }
    if layers.is_empty() {
        return None;
    }
    let out_w = ((w - 1) * 64).min(2048) as u32;
    let out_h = ((h - 1) * 64).min(2048) as u32;
    let mut px = vec![0u8; (out_w * out_h * 4) as usize];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let gu = (ox as f32 + 0.5) / out_w as f32;
            let gv = (oy as f32 + 0.5) / out_h as f32;
            let wx = v0[0] + gu * (w - 1) as f32 * step_x;
            let wy = v0[1] + gv * (h - 1) as f32 * step_y;
            let mut c = [128.0f32, 128.0, 128.0];
            for (tex, alpha, us, vs) in &layers {
                let st = &im.scene.textures[*tex];
                let a = sample(
                    alpha,
                    gu * (w - 1) as f32 / w as f32 + 0.5 / w as f32,
                    gv * (h - 1) as f32 / h as f32 + 0.5 / h as f32,
                    false,
                )[3] / 255.0;
                if a <= 0.0 {
                    continue;
                }
                let tu = wx / (us.max(1e-3) * st.size[0] as f32);
                let tv = wy / (vs.max(1e-3) * st.size[1] as f32);
                let s = sample(&st.image, tu, tv, true);
                for k in 0..3 {
                    c[k] += (s[k] - c[k]) * a;
                }
            }
            let o = ((oy * out_w + ox) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
        }
    }
    Some(RgbaImage {
        width: out_w,
        height: out_h,
        pixels: px,
    })
}

/// Row-major 3x3 to column-major array (for `bevy::math::Mat3::from_cols_array`).
pub fn to_cols(m: &Mat3) -> [f32; 9] {
    [
        m[0][0], m[1][0], m[2][0], m[0][1], m[1][1], m[2][1], m[0][2], m[1][2], m[2][2],
    ]
}

/// Bevy-space camera yaw/pitch (radians, `EulerRot::YXZ`) for an Unreal rotator.
pub fn rotator_to_yaw_pitch(rot: [i32; 3]) -> (f32, f32) {
    let k = std::f32::consts::TAU / 65536.0;
    let yaw_u = rot[1] as f32 * k;
    let pitch_u = rot[0] as f32 * k;
    // Unreal forward (cos p cos y, cos p sin y, sin p) -> Bevy (cos p sin y, sin p, -cos p cos y).
    // Bevy camera forward for yaw Y about +Y is (-sin Y, 0, -cos Y): Y = -yaw_u.
    (-yaw_u, pitch_u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_yaw_matches_axis_mapping() {
        // Unreal yaw 90 deg looks along +Y (source) = +X in Bevy.
        let (yaw, pitch) = rotator_to_yaw_pitch([0, 16384, 0]);
        let fwd = [
            -(yaw.sin()) * pitch.cos(),
            pitch.sin(),
            -(yaw.cos()) * pitch.cos(),
        ];
        assert!(
            (fwd[0] - 1.0).abs() < 1e-5 && fwd[2].abs() < 1e-5,
            "{fwd:?}"
        );
        let (yaw, _) = rotator_to_yaw_pitch([0, 0, 0]);
        assert!(yaw.abs() < 1e-6);
    }

    #[test]
    fn bilinear_sample_wraps() {
        let img = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 0, 255, 200, 200, 200, 255],
        };
        let s = sample(&img, 0.25, 0.5, true);
        assert!(s[0].abs() < 1e-3);
        let s = sample(&img, 0.0, 0.5, true);
        assert!((s[0] - 100.0).abs() < 1e-3, "{s:?}");
    }
}

/// Opt-in import of the opening maps (`XIII_GOG_DIR`); prints SKIPPED otherwise.
#[cfg(test)]
mod local_tests {
    use super::*;

    #[test]
    fn gog_opening_maps_import_without_failures() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        // A relative value is resolved against the workspace root, so the acceptance command
        // `XIII_GOG_DIR=XIII_Game cargo test` works from anywhere (test CWD is the crate dir).
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        for (map, actors, bsp_polys) in [("Plage00", 156, 344), ("Plage01", 133, 338)] {
            let scene = import_map(&mut cache, map).expect("import");
            let get = |k: &str| scene.counters.get(k).copied().unwrap_or(0);
            assert_eq!(get("actor.static_mesh (StaticMeshActor)"), actors, "{map}");
            assert_eq!(get("bsp.polygons"), bsp_polys, "{map}");
            assert_eq!(get("terrain.infos"), 1, "{map}");
            assert!(scene.player_start.is_some(), "{map}");
            let fails: Vec<_> = scene
                .counters
                .keys()
                .filter(|k| k.starts_with("fail."))
                .collect();
            assert!(fails.is_empty(), "{map}: {fails:?} {:?}", scene.examples);
            let missing = scene
                .meshes
                .iter()
                .filter(|m| matches!(m.material, MaterialSlot::Missing(_)))
                .count();
            assert_eq!(
                missing, 0,
                "{map}: unresolved materials {:?}",
                scene.examples
            );
        }
    }
}
