//! Bevy-free world import and the Unreal-space physics adapter.
//!
//! This crate turns a map from an owned installation into converted (Bevy-space) meshes, RGBA
//! textures, object records and a collision triangle soup, and counts everything it could not
//! import. Nothing falls back silently: every skipped actor/material/surface increments a
//! named counter.
//!
//! Actor placement uses **effective** values: the map's tagged property if present, else the
//! inherited class default resolved read-only through `xiii_script` (`Vm::class_layout`), else
//! the documented `Engine.Actor` default (Location 0, Rotation 0, DrawScale 1, DrawScale3D 1,
//! PrePivot 0). The source of every field is counted as `placement.<field>.<source>` and
//! `actor.player_start.<field>.<source>`. `PrePivot` is applied before scale/rotation (see
//! `xiii_decode::common::actor_to_bevy_pre_pivot`).
//!
//! [`physics::WorldPhysicsAdapter`] implements `xiii_script::physics::WorldPhysics` on top of
//! the imported collision, converting between Unreal axes/units (Z up) and the Bevy collision
//! space (Y up, metres) with the single coordinate policy in `xiii_decode::common`.

#![warn(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub mod animation;
pub mod materials;
pub mod nav_provider;
pub mod navigation;
pub mod physics;
pub mod runtime;
pub mod zones;

use materials::{BlendMode, MaterialNode, NodeKey, ResolvedMaterial, UvOp};
use xiii_decode::common::{
    BevyTransform, Mat3, Props, actor_to_bevy_pre_pivot, to_bevy_direction, to_bevy_position,
};
use xiii_decode::model::{self, level, poly_flags};
use xiii_decode::static_mesh::decode_static_mesh;
use xiii_decode::static_mesh_instance::decode_static_mesh_instance;
use xiii_decode::terrain;
use xiii_decode::texture::{RgbaImage, Texture, TextureFormat, decode_texture};
use xiii_install::{Installation, OpenOptions};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue, StructValue};
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
    /// Material (base texture or the reason it is missing) for compatibility/diagnostics.
    pub material: MaterialSlot,
    /// Index into [`WorldScene::materials`] with the full resolved description.
    pub material_index: usize,
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
    /// BSP zone this object belongs to (classified from its location / polygon centroid).
    /// `None` for geometry with no zone assignment (e.g. terrain); the main view draws it.
    pub zone: Option<u32>,
    /// Baked per-vertex colours (RGBA, alpha 255) for this placed object, when the source
    /// provides a stream matching the mesh's vertex count. `None` renders unlit (texture only).
    pub colors: Option<Vec<[u8; 4]>>,
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
    /// One [`materials::ResolvedMaterial`] per distinct surface material (indexed by
    /// [`SceneMesh::material_index`]); blend, two-sidedness and animated UV transforms.
    pub materials: Vec<ResolvedMaterial>,
    /// Player start position (Bevy space) and Unreal rotator.
    pub player_start: Option<([f32; 3], [i32; 3])>,
    /// Named counters ("imported" and "skipped" categories).
    pub counters: BTreeMap<String, usize>,
    /// First examples per failure category.
    pub examples: BTreeMap<String, String>,
    /// Collision triangles in Bevy space (metres) shared by the two query soups, each with its
    /// source id (an index into [`WorldScene::collision_sources`]). A triangle selected by both
    /// an extent query and a zero-extent query is stored once and referenced by both index
    /// lists. Static meshes contribute the set chosen by their collision flags (not render
    /// triangles), the BSP its node polygons without `PF_NotSolid`/portal flags (invisible walls
    /// included), the terrain its visible quads.
    pub collision_triangles: Vec<([[f32; 3]; 3], u32)>,
    /// Indices into [`WorldScene::collision_triangles`] selected for extent (box) queries.
    pub collision_box: Vec<u32>,
    /// Indices into [`WorldScene::collision_triangles`] selected for zero-extent (line/ray)
    /// queries.
    pub collision_line: Vec<u32>,
    /// Source object path per collision source id (shared by both soups).
    pub collision_sources: Vec<String>,
    /// BSP zones of the level: actor metadata, sky flag and per-zone geometry counts.
    pub zones: Vec<zones::SceneZone>,
    /// Indices into [`WorldScene::zones`] of the sky zones (`is_sky`), in increasing order.
    pub sky_zones: Vec<u32>,
}

impl WorldScene {
    /// Reserves a source path id (shared by both soups).
    fn new_collision_source(&mut self, source: String) -> u32 {
        let id = self.collision_sources.len() as u32;
        self.collision_sources.push(source);
        id
    }

    /// Adds `tris` under an existing source id to the selected soups. Triangles selected by both
    /// kinds are stored once in the shared pool. Counts `collision.triangles` per unique entry.
    fn add_collision_tris(
        &mut self,
        id: u32,
        tris: impl IntoIterator<Item = [[f32; 3]; 3]>,
        to_box: bool,
        to_line: bool,
    ) {
        for t in tris {
            let index = self.collision_triangles.len() as u32;
            self.collision_triangles.push((t, id));
            if to_box {
                self.collision_box.push(index);
            }
            if to_line {
                self.collision_line.push(index);
            }
            self.count("collision.triangles", 1);
        }
    }

    /// Adds one source's triangles to both soups (BSP, terrain: one geometry for every query).
    fn add_collision(&mut self, source: String, tris: impl IntoIterator<Item = [[f32; 3]; 3]>) {
        let id = self.new_collision_source(source);
        self.add_collision_tris(id, tris, true, true);
    }

    /// Extent (box) query `(triangle, source id)` entries.
    pub fn box_collision(&self) -> impl Iterator<Item = ([[f32; 3]; 3], u32)> + '_ {
        self.collision_box
            .iter()
            .map(|&i| self.collision_triangles[i as usize])
    }

    /// Zero-extent (line/ray) query `(triangle, source id)` entries.
    pub fn line_collision(&self) -> impl Iterator<Item = ([[f32; 3]; 3], u32)> + '_ {
        self.collision_line
            .iter()
            .map(|&i| self.collision_triangles[i as usize])
    }

    /// Nearest zero-extent (line) collision hit along a ray (Bevy space): distance and source
    /// path. The viewer's crosshair / `--dump` probes use this.
    pub fn ray_collision(&self, origin: [f32; 3], dir: [f32; 3]) -> Option<(f32, &str)> {
        let mut best: Option<(f32, u32)> = None;
        for &i in &self.collision_line {
            let (t, src) = &self.collision_triangles[i as usize];
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
/// Converted static mesh: (scene mesh index, label) per section and the local collision
/// triangles (Bevy space, unscaled) chosen for each query kind.
#[derive(Clone)]
struct MeshSections {
    sections: Vec<(usize, String)>,
    /// Render vertex count shared by every section (== colour stream length).
    vertices: usize,
    /// Collision set chosen for extent (box) queries (0 = per-triangle, 1 = simplified).
    box_set: u8,
    /// Collision set chosen for zero-extent (line/ray) queries.
    line_set: u8,
    /// True when the box flag requested the simplified set but the mesh has none (fell back).
    box_fallback: bool,
    /// True when the line flag requested the simplified set but the mesh has none (fell back).
    line_fallback: bool,
    /// Triangles of `box_set` (Bevy space, unscaled). Shared with `collision_line` when both
    /// queries select the same set.
    collision_box: Arc<Vec<[[f32; 3]; 3]>>,
    /// Triangles of `line_set` (Bevy space, unscaled).
    collision_line: Arc<Vec<[[f32; 3]; 3]>>,
    /// Collision triangles of the box set whose material slot has `EnableCollision` = false. UE2
    /// would not block the player with these; they are counted (not silently kept or dropped).
    collision_slot_disabled: usize,
}

struct Importer<'a> {
    cache: &'a mut PackageCache,
    scene: WorldScene,
    textures: HashMap<ObjectKey, Result<usize, String>>,
    meshes: HashMap<ObjectKey, Result<MeshSections, String>>,
    /// Decoded material nodes by object key (the graph walker asks for one at a time).
    nodes: HashMap<NodeKey, MaterialNode>,
    /// Resolved materials by root object key (a root reused by many surfaces is walked once).
    resolved: HashMap<NodeKey, usize>,
}

impl Importer<'_> {
    /// Export key of an object reference, resolving imports into their root package.
    fn key_of(&mut self, pkg: &Arc<Loaded>, r: ObjectRef) -> Option<NodeKey> {
        if r.is_null() {
            return None;
        }
        let (target, idx) = self.cache.resolve(pkg, r).ok()?;
        Some((target.name.to_ascii_lowercase(), idx))
    }

    /// Decodes one material object into a [`MaterialNode`] (cached). A `Texture` decodes and
    /// registers its image; every other class is read from its tagged properties.
    fn node_for(&mut self, pkg: &Arc<Loaded>, idx: usize) -> Result<MaterialNode, String> {
        let key = (pkg.name.to_ascii_lowercase(), idx);
        if let Some(n) = self.nodes.get(&key) {
            return Ok(n.clone());
        }
        let class_full = pkg.package.export_class_path(idx).unwrap_or("?").to_owned();
        let class = class_full.rsplit('.').next().unwrap_or("").to_owned();
        let result = if class.eq_ignore_ascii_case("Texture") {
            let t = self.texture_from(pkg, ObjectRef::Export(idx as u32), 0)?;
            let texture_alpha = match self.scene.textures[t].alpha {
                AlphaKind::Opaque => None,
                AlphaKind::Mask => Some(BlendMode::Masked(0.5)),
                AlphaKind::Blend => Some(BlendMode::Alpha),
            };
            Ok(MaterialNode {
                class,
                texture: Some(t),
                texture_alpha,
                ..Default::default()
            })
        } else {
            let props = pkg
                .package
                .read_object_properties(&pkg.data, idx, &Limits::default())
                .map_err(|e| format!("{class_full} properties: {e}"))?;
            let p = Props::new(&pkg.package, &props);
            Ok(self.build_node(pkg, &class, &p))
        };
        if let Ok(n) = &result {
            self.nodes.insert(key, n.clone());
        }
        result
    }

    /// Node lookup for the graph walker: the key names a loaded package and export.
    fn node_for_key(&mut self, key: &NodeKey) -> Option<MaterialNode> {
        let pkg = self.cache.get(&key.0).ok()?;
        self.node_for(&pkg, key.1).ok()
    }

    /// Builds a [`MaterialNode`] from a decoded property block. Object links are resolved
    /// against `pkg` so the graph walker only ever sees [`NodeKey`]s.
    fn build_node(&mut self, pkg: &Arc<Loaded>, class: &str, p: &Props) -> MaterialNode {
        let mut n = MaterialNode {
            class: class.to_owned(),
            ..Default::default()
        };
        match class.to_ascii_lowercase().as_str() {
            "shader" => {
                let diffuse = self.key_of(pkg, p.object("Diffuse").unwrap_or(ObjectRef::Null));
                let self_illum =
                    self.key_of(pkg, p.object("SelfIllumination").unwrap_or(ObjectRef::Null));
                // Unlit/bright shaders often carry the image in SelfIllumination; fall back to
                // it when Diffuse is empty (the previous resolver did the same).
                let have_diffuse = diffuse.is_some();
                let have_self = self_illum.is_some();
                n.diffuse = diffuse.or(self_illum);
                if !have_diffuse && have_self {
                    n.ignored
                        .push("shader.self_illumination_as_base".to_owned());
                } else if have_self {
                    n.ignored.push("shader.self_illumination".to_owned());
                }
                n.output_blending = p.byte("OutputBlending");
                n.two_sided = p.bool("TwoSided");
                for (name, note) in [
                    ("Opacity", "shader.opacity"),
                    ("Specular", "shader.specular"),
                    ("SpecularityMask", "shader.specularity_mask"),
                    ("FallbackMaterial", "shader.fallback_material"),
                ] {
                    if p.object(name).is_some_and(|o| !o.is_null()) {
                        n.ignored.push(note.to_owned());
                    }
                }
            }
            "finalblend" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                n.frame_buffer_blending = p.byte("FrameBufferBlending");
                n.two_sided = p.bool("TwoSided");
                n.alpha_test = p.bool("AlphaTest");
                n.alpha_ref = p.byte("AlphaRef");
                if p.bool("ZWrite") == Some(false) || p.bool("ZTest") == Some(false) {
                    n.ignored.push("finalblend.no_depth".to_owned());
                }
            }
            "texpanner" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                let rate = p.float("PanRate").unwrap_or(0.1);
                let dir = p.rotator("PanDirection").unwrap_or([0, 0, 0]);
                let yaw = rotator_radians(dir[1]);
                if rate != 0.0 {
                    n.uv_ops.push(UvOp::Pan {
                        speed_u: rate * yaw.cos(),
                        speed_v: rate * yaw.sin(),
                    });
                }
                if dir[0] != 0 || dir[2] != 0 {
                    n.ignored.push("texpanner.pan_pitch_roll".to_owned());
                }
            }
            "texrotator" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                let rot = p.rotator("Rotation").unwrap_or([0, 0, 0]);
                let constant = p.bool("ConstantRotation").unwrap_or(false);
                let cu = p.float("URotCenter").unwrap_or(0.0);
                let cv = p.float("VRotCenter").unwrap_or(0.0);
                let yaw = rotator_radians(rot[1]);
                if constant {
                    if yaw != 0.0 {
                        n.uv_ops.push(UvOp::Rotate {
                            base: 0.0,
                            rate: yaw,
                            center_u: cu,
                            center_v: cv,
                        });
                    }
                } else if yaw != 0.0 {
                    n.uv_ops.push(UvOp::Rotate {
                        base: yaw,
                        rate: 0.0,
                        center_u: cu,
                        center_v: cv,
                    });
                }
                if rot[0] != 0 || rot[2] != 0 {
                    n.ignored.push("texrotator.rotation_pitch_roll".to_owned());
                }
            }
            "texoscillator" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                let rate_u = p.float("UOscillationRate").unwrap_or(1.0);
                let rate_v = p.float("VOscillationRate").unwrap_or(1.0);
                let amp_u = p.float("UOscillationAmplitude").unwrap_or(0.1);
                let amp_v = p.float("VOscillationAmplitude").unwrap_or(0.1);
                let phase_u = p.float("UOscillationPhase").unwrap_or(0.0);
                let phase_v = p.float("VOscillationPhase").unwrap_or(0.0);
                let type_u = p.byte("UOscillationType").unwrap_or(0);
                let type_v = p.byte("VOscillationType").unwrap_or(0);
                if type_u != type_v {
                    n.ignored.push("texoscillator.mixed_types".to_owned());
                }
                // OT_Pan = 0, OT_Stretch = 1 (measured enum order).
                if type_u == 0 {
                    n.uv_ops.push(UvOp::OscillatePan {
                        amplitude_u: amp_u,
                        amplitude_v: amp_v,
                        rate_u,
                        rate_v,
                        phase_u,
                        phase_v,
                    });
                } else {
                    n.uv_ops.push(UvOp::OscillateScale {
                        amplitude_u: amp_u,
                        amplitude_v: amp_v,
                        rate_u,
                        rate_v,
                        phase_u,
                        phase_v,
                    });
                }
                if p.float("UCenter").unwrap_or(0.0) != 0.0
                    || p.float("VCenter").unwrap_or(0.0) != 0.0
                {
                    n.ignored.push("texoscillator.center".to_owned());
                }
            }
            "texscaler" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                let su = p.float("UScale").unwrap_or(1.0);
                let sv = p.float("VScale").unwrap_or(1.0);
                if su != 1.0 || sv != 1.0 {
                    n.uv_ops.push(UvOp::Scale {
                        scale_u: su,
                        scale_v: sv,
                    });
                }
            }
            "colormodifier" => {
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                if let Some(prop) = p.get("Color")
                    && let PropertyValue::Struct(StructValue::Color(c)) = &prop.value
                {
                    n.color = Some([
                        f32::from(c[0]) / 255.0,
                        f32::from(c[1]) / 255.0,
                        f32::from(c[2]) / 255.0,
                        f32::from(c[3]) / 255.0,
                    ]);
                }
            }
            "combiner" => {
                n.material1 = self.key_of(pkg, p.object("Material1").unwrap_or(ObjectRef::Null));
                n.material2 = self.key_of(pkg, p.object("Material2").unwrap_or(ObjectRef::Null));
            }
            _ => {
                // Every other material class (TexEnvMap, SinusModifier, TexCoordSource,
                // TexModifier and unknown subclasses) follows its `Material`/`Diffuse` input if
                // it has one; the class itself is listed as unsupported by the walker.
                n.material = self.key_of(pkg, p.object("Material").unwrap_or(ObjectRef::Null));
                n.diffuse = self.key_of(pkg, p.object("Diffuse").unwrap_or(ObjectRef::Null));
            }
        }
        n
    }

    /// Resolves the surface material `r` to a description and registers it, returning the
    /// compatibility [`MaterialSlot`] and the index into [`WorldScene::materials`].
    fn material(&mut self, from: &Arc<Loaded>, r: ObjectRef, what: &str) -> (MaterialSlot, usize) {
        if r.is_null() {
            self.scene.count(&format!("skip.{what}.material_none"), 1);
            let idx = self.push_material(ResolvedMaterial::default());
            return (MaterialSlot::Missing("None".into()), idx);
        }
        let start = match self.cache.resolve(from, r) {
            Ok((pkg, idx)) => (pkg.name.to_ascii_lowercase(), idx),
            Err(e) => {
                let label = from.package.object_path(r).unwrap_or("?").to_owned();
                self.scene.fail(
                    &format!("skip.{what}.material (unresolved)"),
                    format!("{label}: {e}"),
                );
                let idx = self.push_material(ResolvedMaterial::default());
                return (MaterialSlot::Missing(e), idx);
            }
        };
        if let Some(&idx) = self.resolved.get(&start) {
            return (self.slot_for(idx), idx);
        }
        let resolved = {
            let mut lookup = |k: &NodeKey| self.node_for_key(k);
            materials::resolve(Some(start.clone()), &mut lookup)
        };
        let idx = self.push_material(resolved);
        self.resolved.insert(start, idx);
        self.count_material(idx, what);
        (self.slot_for(idx), idx)
    }

    /// Compatibility slot derived from the resolved base texture.
    fn slot_for(&self, idx: usize) -> MaterialSlot {
        match self.scene.materials[idx].base {
            Some(t) => MaterialSlot::Texture(t),
            None => {
                let m = &self.scene.materials[idx];
                let reason = if m.unsupported.is_empty() {
                    "no texture".to_owned()
                } else {
                    m.unsupported.join(",")
                };
                MaterialSlot::Missing(reason)
            }
        }
    }

    fn push_material(&mut self, m: ResolvedMaterial) -> usize {
        self.scene.materials.push(m);
        self.scene.materials.len() - 1
    }

    /// Counts blend/two-sided/animated-UV/unsupported features for the overlay.
    fn count_material(&mut self, idx: usize, what: &str) {
        let m = self.scene.materials[idx].clone();
        let base_missing = m.base.is_none();
        self.scene.count(&format!("material.resolved.{what}"), 1);
        self.scene
            .count(&format!("material.blend.{}", m.blend.name()), 1);
        if m.two_sided {
            self.scene.count(&format!("material.two_sided.{what}"), 1);
        }
        if !m.uv_transform.is_empty() {
            self.scene.count(&format!("material.uv_animated.{what}"), 1);
            self.scene.count("material.uv_ops", m.uv_transform.len());
        }
        for u in &m.unsupported {
            self.scene.count(&format!("material.unsupported ({u})"), 1);
        }
        if base_missing {
            self.scene.count(&format!("skip.{what}.no_base_texture"), 1);
            self.scene
                .examples
                .entry(format!("skip.{what}.no_base_texture"))
                .or_insert_with(|| m.class_chain.join(" -> "));
        }
    }

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
                // UE2 picks a collision representation per query kind (Epic UDN
                // `Two/StaticMeshCollisionReference`: "Non-Zero Extent Traces (ie Pawn Movement)"
                // use the collision model when the Box flag is set, "Zero Extent Traces (ie
                // Weapon Fire)" when the Line flag is set). The class default row for a Type 1
                // (kDOP/triangle) collision model is `Karma=true, Box=true, Line=false`, and a
                // mesh only serializes a flag that differs from that default. The simplified
                // model is used only when it exists; otherwise the per-triangle set (0) is used
                // ("Material Collision" in the reference). No fallback to render geometry.
                let (use_line, use_box, karma_tagged) = mesh_collision_flags(&pkg, idx);
                if karma_tagged {
                    self.scene
                        .count("note.collision.static_mesh.karma_collision", 1);
                    self.scene
                        .examples
                        .entry("note.collision.static_mesh.karma_collision".to_owned())
                        .or_insert_with(|| format!("{label}: UseSimpleKarmaCollision set"));
                }
                let simplified_present = !m.collision[1].triangles.is_empty();
                // A flag that requests the simplified model but has none falls back to set 0.
                let box_fallback = use_box && !simplified_present;
                let line_fallback = use_line && !simplified_present;
                let box_set = usize::from(use_box && simplified_present);
                let line_set = usize::from(use_line && simplified_present);
                let convert = |cs: &xiii_decode::static_mesh::CollisionSet| -> Vec<[[f32; 3]; 3]> {
                    cs.triangles
                        .iter()
                        .map(|t| {
                            t.vertices
                                .map(|v| to_bevy_position(cs.vertices[v as usize]))
                        })
                        .collect()
                };
                // Share the converted triangles when both query kinds select the same set.
                let (collision_box, collision_line) = if box_set == line_set {
                    let shared = Arc::new(convert(&m.collision[box_set]));
                    (shared.clone(), shared)
                } else {
                    (
                        Arc::new(convert(&m.collision[box_set])),
                        Arc::new(convert(&m.collision[line_set])),
                    )
                };
                // Collision triangles of the box (movement) set whose material slot has
                // EnableCollision = false: under UE2 these do not block the player. Counted,
                // not filtered (evidence only).
                let collision_slot_disabled = m.collision[box_set]
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
                    let (material, material_index) = match m.materials.get(si) {
                        Some(mm) => self.material(&pkg, mm.material, "mesh"),
                        None => {
                            self.scene.count("skip.mesh.section_without_material", 1);
                            let idx = self.push_material(ResolvedMaterial::default());
                            (MaterialSlot::Missing("no slot".into()), idx)
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
                        material_index,
                    });
                    out.push((self.scene.meshes.len() - 1, label.clone()));
                }
                Ok(MeshSections {
                    sections: out,
                    vertices: positions.len(),
                    box_set: box_set as u8,
                    line_set: line_set as u8,
                    box_fallback,
                    line_fallback,
                    collision_box,
                    collision_line,
                    collision_slot_disabled,
                })
            }
        };
        self.meshes.insert(key, result.clone());
        result
    }

    /// Decoded baked vertex colours of a placed `StaticMeshInstance`, converted to RGBA.
    /// Counts lit / unlit / mismatched objects; a colour count that does not match the mesh's
    /// vertex count is a failure, never applied silently.
    fn instance_colors(
        &mut self,
        from: &Arc<Loaded>,
        r: ObjectRef,
        vertex_count: usize,
        path: &str,
    ) -> Option<Vec<[u8; 4]>> {
        let (pkg, idx) = match self.cache.resolve(from, r) {
            Ok(v) => v,
            Err(e) => {
                self.scene
                    .fail("fail.lighting.instance_ref", format!("{path}: {e}"));
                return None;
            }
        };
        match decode_static_mesh_instance(&pkg.package, &pkg.data, idx) {
            Ok(inst) => {
                self.scene.count("lighting.instances.decoded", 1);
                self.scene
                    .count("lighting.instances.lights", inst.lights.len());
                if inst.colors.len() != vertex_count {
                    self.scene.count("lighting.objects.mismatch", 1);
                    self.scene
                        .examples
                        .entry("lighting.objects.mismatch".to_owned())
                        .or_insert_with(|| {
                            format!(
                                "{}: {} colours for {vertex_count} mesh vertices",
                                path,
                                inst.colors.len()
                            )
                        });
                    return None;
                }
                if inst.is_unlit() {
                    self.scene.count("lighting.objects.unlit", 1);
                    return None;
                }
                self.scene.count("lighting.objects.lit", 1);
                self.scene.count("lighting.colors.rgba", inst.colors.len());
                Some(
                    (0..inst.colors.len())
                        .map(|i| {
                            let c = inst.rgba(i);
                            [c[0], c[1], c[2], 255]
                        })
                        .collect(),
                )
            }
            Err(e) => {
                self.scene
                    .fail("fail.lighting.instance_decode", format!("{path}: {e}"));
                None
            }
        }
    }
}

/// Effective values of one actor: map property, else inherited class default, else the
/// documented `Engine.Actor` default. Sources are counted per field.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolvedPlacement {
    /// Effective location in Unreal units.
    pub location: [f32; 3],
    /// Effective rotation rotator `(pitch, yaw, roll)`.
    pub rotation: [i32; 3],
    /// Effective uniform `DrawScale`.
    pub draw_scale: f32,
    /// Effective per-axis `DrawScale3D`.
    pub draw_scale_3d: [f32; 3],
    /// Effective `PrePivot` (Unreal units, mesh space).
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

    /// True when `class_path`'s inheritance chain contains `NavigationPoint` (so the actor is
    /// a navigation point: `PathNode`, `PlayerStart`, `Ladder`, game `*Point` subclasses, ...).
    pub fn is_navigation_point(&mut self, class_path: &str) -> Result<bool, String> {
        let l = self.layout(class_path)?;
        Ok(l.chain_names.iter().any(|n| n == "navigationpoint"))
    }

    /// Resolved inherited float default of a property, or `None` when the property is absent
    /// from the class chain (or is not a float). Map properties are handled by the caller.
    pub fn float_default(&mut self, class_path: &str, name: &str) -> Result<Option<f32>, String> {
        let l = self.layout(class_path)?;
        let Some(s) = l.slot_by_name(name) else {
            return Ok(None);
        };
        Ok(match l.defaults.get(s.base) {
            Some(xiii_script::Value::Float(v)) => Some(*v),
            _ => None,
        })
    }

    /// Resolved inherited vector default of a property, or `None` when absent/not a vector.
    pub fn vector_default(
        &mut self,
        class_path: &str,
        name: &str,
    ) -> Result<Option<[f32; 3]>, String> {
        let l = self.layout(class_path)?;
        let Some(s) = l.slot_by_name(name) else {
            return Ok(None);
        };
        Ok(match l.defaults.get(s.base) {
            Some(xiii_script::Value::Vector(v)) => Some(*v),
            _ => None,
        })
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

/// Unreal rotator component (65536 per turn) to radians.
fn rotator_radians(units: i32) -> f32 {
    units as f32 * (std::f32::consts::TAU / 65536.0)
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

/// The mesh's effective simplified-collision flags `(UseSimpleLineCollision, UseSimpleBoxCollision,
/// UseSimpleKarmaCollision)`.
///
/// `Engine.StaticMesh` is native-only, so an absent tagged property takes the C++ constructor
/// value. Epic's UE2 UDN reference `Two/StaticMeshCollisionReference`
/// (`udn.epicgames.com/Two/StaticMeshCollisionReference.html`, archived 2007-04-30) marks the
/// class default row as `Karma=true, Box=true, Line=false`: non-zero-extent traces (pawn
/// movement) use a Type 1 collision model by default, zero-extent traces (weapon fire) do not.
/// This is corroborated in the corpus: 167 meshes tag `UseSimpleBoxCollision=false` and none tag
/// it `true`; many tag `UseSimpleLineCollision=true` and none tag it `false` — Unreal only
/// serializes a property that differs from the class default (measured, `xiii-tool props`).
///
/// The third value is whether `UseSimpleKarmaCollision` is explicitly tagged; this runtime does
/// not model Karma, so the documented `true` default is not applied.
fn mesh_collision_flags(pkg: &Loaded, idx: usize) -> (bool, bool, bool) {
    let Ok(props) = pkg
        .package
        .read_object_properties(&pkg.data, idx, &Limits::default())
    else {
        return (false, true, false);
    };
    let p = xiii_decode::common::Props::new(&pkg.package, &props);
    (
        p.bool("UseSimpleLineCollision").unwrap_or(false),
        p.bool("UseSimpleBoxCollision").unwrap_or(true),
        p.bool("UseSimpleKarmaCollision").unwrap_or(false),
    )
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
        nodes: HashMap::new(),
        resolved: HashMap::new(),
    };
    let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
    im.scene
        .count("actor.property_failures", actors.failures.len());
    // Decode the level BSP once, before placing actors: its node/leaf zone tables are needed
    // for both the static-mesh actor zones and the BSP polygon zones.
    let level = match model::find_level_model(&map_pkg.package, &map_pkg.data) {
        Ok(idx) => match model::decode_model(&map_pkg.package, &map_pkg.data, idx) {
            Ok(m) => Some(m),
            Err(e) => {
                im.scene.fail("fail.bsp.decode", e.to_string());
                None
            }
        },
        Err(e) => {
            im.scene.fail("fail.bsp.level_model", e.to_string());
            None
        }
    };
    if let Some(m) = &level {
        if m.report.unsupported_tail.is_some() {
            im.scene.count(
                "note.bsp.model_tail_not_decoded (zones/lightmaps/leaves)",
                1,
            );
        }
        im.scene.zones = zones::scene_zones(&map_pkg.package, &map_pkg.data, m);
        im.scene.sky_zones = im
            .scene
            .zones
            .iter()
            .filter(|z| z.is_sky)
            .map(|z| z.index)
            .collect();
        im.scene.count("zones.total", im.scene.zones.len());
        im.scene.count("zones.sky", im.scene.sky_zones.len());
        // A sky zone without a readable Location cannot be rendered by the sky camera; count
        // it (never drop it silently). Null-actor non-sky zones are expected to have none.
        let missing_sky_location = im
            .scene
            .zones
            .iter()
            .filter(|z| z.is_sky && z.location.is_none())
            .count();
        if missing_sky_location > 0 {
            im.scene
                .count("note.zones.sky_zone_without_location", missing_sky_location);
        }
    }
    let level_model = level.as_ref();
    let zone_map = level_model.map(zones::ZoneMap::new);
    if let Some(zm) = &zone_map {
        im.scene
            .count("zones.leaf_conflicts", zm.leaf_conflicts() as usize);
    }
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
        // Primary method: BSP point classification. Fallback for actors whose location is not
        // covered by the leaf/zone table: the engine-computed `Region.ZoneNumber` (the game's
        // own leaf->zone mapping), counted so the fallback is never silent.
        let zone = zone_for_location(zone_map.as_ref(), level_model, eff.location)
            .filter(|z| (*z as usize) < im.scene.zones.len());
        let zone = match zone {
            Some(z) => Some(z),
            None => {
                let z = region_zone(&map_pkg.package, &map_pkg.data, a.export)
                    .filter(|z| (*z as usize) < im.scene.zones.len());
                if z.is_some() {
                    im.scene.count("zones.actor_region_fallback", 1);
                }
                z
            }
        };
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
                let object_colors = match a.static_mesh_instance {
                    Some(r) => im.instance_colors(&map_pkg, r, converted.vertices, &a.path),
                    None => {
                        im.scene.count("lighting.objects.unlit", 1);
                        None
                    }
                };
                for (mesh, label) in converted.sections {
                    label0.clone_from(&label);
                    im.scene.objects.push(SceneObject {
                        mesh,
                        transform,
                        path: format!("{} -> {label}", a.path),
                        placement: Some(eff),
                        zone,
                        colors: object_colors.clone(),
                    });
                }
                // Per-query-kind evidence: which collision set the rule chose, whether the flag
                // requested the simplified model but the mesh had none (fallback to set 0), and
                // whether the chosen set is empty (contributes nothing, listed).
                for (kind, set, fallback, tris) in [
                    (
                        "box",
                        converted.box_set,
                        converted.box_fallback,
                        &converted.collision_box,
                    ),
                    (
                        "line",
                        converted.line_set,
                        converted.line_fallback,
                        &converted.collision_line,
                    ),
                ] {
                    im.scene.count(&format!("collision.{kind}.set{set}"), 1);
                    if fallback {
                        im.scene
                            .count(&format!("collision.{kind}.simple_missing_fallback"), 1);
                    }
                    if tris.is_empty() {
                        let key = format!("collision.{kind}.empty");
                        im.scene.count(&key, 1);
                        im.scene.examples.entry(key).or_insert_with(|| {
                            format!("{}: chosen set {set} has 0 triangles", a.path)
                        });
                    }
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
                    let id = im
                        .scene
                        .new_collision_source(format!("{} -> {label0}", a.path));
                    if converted.box_set == converted.line_set {
                        // One geometry for both query kinds: stored once, referenced by both.
                        let tris = converted
                            .collision_box
                            .iter()
                            .map(|t| t.map(|v| apply_transform(&transform, v)));
                        im.scene.add_collision_tris(id, tris, true, true);
                    } else {
                        let box_tris = converted
                            .collision_box
                            .iter()
                            .map(|t| t.map(|v| apply_transform(&transform, v)));
                        im.scene.add_collision_tris(id, box_tris, true, false);
                        let line_tris = converted
                            .collision_line
                            .iter()
                            .map(|t| t.map(|v| apply_transform(&transform, v)));
                        im.scene.add_collision_tris(id, line_tris, false, true);
                    }
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
    // Per-zone object counts (static-mesh actors, BSP groups), after every object exists.
    let mut counts = vec![0usize; im.scene.zones.len()];
    let mut unzoned = 0usize;
    let mut unzoned_example: Option<String> = None;
    for o in &im.scene.objects {
        match o.zone {
            Some(z) if (z as usize) < counts.len() => counts[z as usize] += 1,
            // Terrain and any actor the BSP and `Region` fallback could not place.
            _ => {
                unzoned += 1;
                if unzoned_example.is_none() {
                    unzoned_example = Some(o.path.clone());
                }
            }
        }
    }
    im.scene.count("zones.objects_without_zone", unzoned);
    if let Some(e) = unzoned_example {
        im.scene
            .examples
            .entry("zones.objects_without_zone".to_owned())
            .or_insert(e);
    }
    for (z, c) in counts.into_iter().enumerate() {
        if let Some(zone) = im.scene.zones.get_mut(z) {
            zone.object_count = c;
        }
    }
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

/// BSP zone of an actor's **effective** location (source Unreal units), or `None` when the
/// level model is unavailable or the location falls outside the leaf/zone table.
fn zone_for_location(
    zone_map: Option<&zones::ZoneMap>,
    model: Option<&model::Model>,
    location: [f32; 3],
) -> Option<u32> {
    let (map, model) = (zone_map?, model?);
    map.zone_of_point(&model.nodes, location).map(u32::from)
}

/// Engine-computed zone of an actor export: its `Region.ZoneNumber`. Used only as a fallback
/// when the BSP point classification finds no zone. Mesh actors the engine left outside the
/// tree store `iLeaf = -1, ZoneNumber = 0` (the outer/void zone), so `iLeaf` is not required.
fn region_zone(package: &Package, data: &[u8], export: usize) -> Option<u32> {
    let props = package
        .read_object_properties(data, export, &Limits::default())
        .ok()?;
    let p = Props::new(package, &props);
    match p.get("Region").map(|x| &x.value) {
        Some(PropertyValue::Struct(StructValue::PointRegion { zone_number, .. })) => {
            Some(u32::from(*zone_number))
        }
        _ => None,
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
    // The level model was already decoded (and any failure counted) at the top of
    // [`import_map`] for actor-zone classification; decode it again here so this function
    // keeps its original signature (the collision accumulation is edited in parallel).
    let Ok(idx) = model::find_level_model(p, &map_pkg.data) else {
        return;
    };
    let Ok(m) = model::decode_model(p, &map_pkg.data, idx) else {
        return;
    };
    let zone_map = zones::ZoneMap::new(&m);
    // Group triangles per (surface material, BSP zone). Keying by zone keeps every object in
    // exactly one render layer (sky vs playable); a mesh never spans two zones.
    let mut groups: BTreeMap<(i64, Option<u32>), (MaterialSlot, SceneMesh)> = BTreeMap::new();
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
            im.scene.count(
                "skip.bsp.sky_backdrop_polygons (backdrop: sky camera shows through)",
                1,
            );
            continue;
        }
        // Polygon zone: BSP centroid classification, else the node's own positive-side zone
        // (`BspNode::zone[1]`; the two agree on every polygon of the corpus, but the direct
        // field always exists even when the centroid traversal lands in a zone-less leaf).
        let zone = zone_map
            .zone_of_polygon(&m.nodes, &poly)
            .or_else(|| {
                let z = usize::from(m.nodes[poly.node].zone[1]);
                (z < im.scene.zones.len()).then_some(z as u8)
            })
            .map(u32::from);
        if let Some(z) = zone
            && let Some(sc) = im.scene.zones.get_mut(z as usize)
        {
            sc.polygon_count += 1;
        }
        let key = (i64::from(surf.material.raw()), zone);
        if let std::collections::btree_map::Entry::Vacant(slot) = groups.entry(key) {
            let (material, material_index) = im.material(map_pkg, surf.material, "bsp");
            slot.insert((
                material.clone(),
                SceneMesh {
                    label: format!("BSP {}", p.object_path(surf.material).unwrap_or("None")),
                    positions: Vec::new(),
                    normals: Vec::new(),
                    uvs: Vec::new(),
                    indices: Vec::new(),
                    material,
                    material_index,
                },
            ));
        }
        let (_, mesh) = groups.get_mut(&key).expect("inserted");
        let tex_size = match im.scene.materials[mesh.material_index].base {
            Some(t) => im.scene.textures[t].size,
            None => [64, 64],
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
    for ((_, zone), (_, mesh)) in groups {
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
            zone,
            colors: None,
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
        let (material, material_index) = match composite {
            Some(img) => {
                im.scene.textures.push(SceneTexture {
                    label: format!("{path} layer composite"),
                    size: [img.width, img.height],
                    image: img,
                    alpha: AlphaKind::Opaque,
                });
                let base = im.scene.textures.len() - 1;
                let idx = im.push_material(ResolvedMaterial {
                    base: Some(base),
                    ..Default::default()
                });
                (MaterialSlot::Texture(base), idx)
            }
            None => {
                let idx = im.push_material(ResolvedMaterial {
                    unsupported: vec!["terrain.layers".into()],
                    ..Default::default()
                });
                (MaterialSlot::Missing("terrain layers".into()), idx)
            }
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
        let terrain_colors = match terrain::color_grid(p, &map_pkg.data, &t, w, h) {
            Ok(grid) => {
                im.scene.count("lighting.terrain.colors", grid.colors.len());
                im.scene
                    .count("lighting.terrain.color_conflicts", grid.conflicts);
                if grid.missing > 0 {
                    // A partial grid would modulate uncovered vertices to black; leave the
                    // terrain uncoloured and report the gap instead.
                    im.scene
                        .count("lighting.terrain.colors_missing", grid.missing);
                    None
                } else {
                    Some(
                        (0..grid.colors.len())
                            .map(|i| {
                                let c = grid.rgba(i);
                                [c[0], c[1], c[2], 255]
                            })
                            .collect(),
                    )
                }
            }
            Err(e) => {
                im.scene.fail("fail.terrain.colors", e.to_string());
                None
            }
        };
        im.scene.meshes.push(SceneMesh {
            label: format!("{path} heightfield"),
            positions,
            normals,
            uvs: mesh.grid_uv.clone(),
            indices: mesh.indices.clone(),
            material,
            material_index,
        });
        im.scene.objects.push(SceneObject {
            mesh: im.scene.meshes.len() - 1,
            transform: identity(),
            path,
            placement: None,
            zone: None,
            colors: terrain_colors,
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

    /// Resolves `XIII_GOG_DIR`, treating a relative value as workspace-relative so the
    /// acceptance command `XIII_GOG_DIR=XIII_Game cargo test` works from the crate directory.
    fn gog_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        if path.is_relative() {
            Some(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(path),
            )
        } else {
            Some(path)
        }
    }

    #[test]
    fn gog_opening_maps_import_without_failures() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
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

    /// Extra corpus case (beyond the spec): BSP point classification against the
    /// engine-computed `Region.iLeaf` of every placed Plage00 actor. Confirms the measured
    /// swapped leaf-slot pairing documented in [`zones`]: the swapped pairing matches almost
    /// every actor, the unswapped pairing almost none. The few mismatches are editor-set or
    /// orphan actors (`Camera`, `PhysicsVolume`, an unused `SkyZoneInfo5`).
    #[test]
    fn gog_bsp_point_classification_matches_engine_region() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let map_pkg = cache.map("Plage00").expect("map");
        let idx = model::find_level_model(&map_pkg.package, &map_pkg.data).expect("level model");
        let m = model::decode_model(&map_pkg.package, &map_pkg.data, idx).expect("decode");
        let zm = zones::ZoneMap::new(&m);
        let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
        let mut region_actors = 0usize;
        let mut swapped = 0usize;
        let mut unswapped = 0usize;
        for a in &actors.all_located {
            let Some(loc) = a.location else { continue };
            let Ok(props) =
                map_pkg
                    .package
                    .read_object_properties(&map_pkg.data, a.export, &Limits::default())
            else {
                continue;
            };
            let p = xiii_decode::common::Props::new(&map_pkg.package, &props);
            let Some(PropertyValue::Struct(xiii_package::StructValue::PointRegion {
                leaf, ..
            })) = p.get("Region").map(|x| &x.value)
            else {
                continue;
            };
            if *leaf < 0 {
                continue;
            }
            region_actors += 1;
            // Swapped pairing (what `zones::ZoneMap` implements): positive side -> leaf[1].
            if zm.leaf_of_point(&m.nodes, loc) == Some(*leaf as usize) {
                swapped += 1;
            }
            // Unswapped pairing, kept here as the counter-hypothesis: positive side -> leaf[0].
            let mut i = zm.root();
            let mut unswapped_leaf = None;
            for _ in 0..=m.nodes.len() {
                let Some(n) = m.nodes.get(i) else { break };
                let d =
                    n.plane[0] * loc[0] + n.plane[1] * loc[1] + n.plane[2] * loc[2] - n.plane[3];
                let (child, slot) = if d >= 0.0 {
                    (n.front, n.leaf[0])
                } else {
                    (n.back, n.leaf[1])
                };
                if child < 0 {
                    unswapped_leaf = (slot >= 0).then_some(slot as usize);
                    break;
                }
                i = child as usize;
            }
            if unswapped_leaf == Some(*leaf as usize) {
                unswapped += 1;
            }
        }
        assert!(
            region_actors >= 300,
            "expected >=300 Plage00 actors with a Region, got {region_actors}"
        );
        assert!(
            swapped * 100 >= region_actors * 95,
            "swapped pairing matched {swapped} of {region_actors}"
        );
        assert!(
            unswapped * 100 <= region_actors * 5,
            "unswapped pairing matched {unswapped} of {region_actors}"
        );
        println!(
            "[zones] Plage00 Region cross-check: swapped {swapped}, unswapped {unswapped}, actors {region_actors}"
        );
    }

    /// The opening campaign maps each have exactly one sky zone (`Engine.SkyZoneInfo`) with a
    /// decoded location and a non-zero count of imported sky-zone geometry.
    #[test]
    fn gog_plage_maps_have_one_sky_zone_with_geometry() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        for map in ["Plage00", "Plage01"] {
            let scene = import_map(&mut cache, map).expect("import");
            let sky: Vec<&zones::SceneZone> = scene.zones.iter().filter(|z| z.is_sky).collect();
            assert_eq!(
                sky.len(),
                1,
                "{map}: expected exactly one sky zone, got {:?}",
                scene.zones
            );
            let sky = sky[0];
            assert!(
                sky.actor_class
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case("Engine.SkyZoneInfo")),
                "{map}: sky zone actor class {:?}",
                sky.actor_class
            );
            assert!(sky.location.is_some(), "{map}: sky zone has no Location");
            assert_eq!(scene.sky_zones, vec![sky.index], "{map}");
            assert!(
                sky.polygon_count > 0,
                "{map}: no sky-zone polygons (zone {} {})",
                sky.index,
                sky.actor_path.as_deref().unwrap_or("?")
            );
            println!(
                "[zones] {map}: sky zone {} {} -> {} polygons, {} objects, bevy location {:?}",
                sky.index,
                sky.actor_path.as_deref().unwrap_or("?"),
                sky.polygon_count,
                sky.object_count,
                sky.location
            );
        }
    }

    /// The blend mapping in [`materials`] is derived from the game's own reflected enums. This
    /// opt-in test pins the enumerator order so a corpus change or a mapping drift is caught.
    #[test]
    fn opt_in_material_enum_names_match_blend_mapping() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let data = std::fs::read(path.join("system/engine.u")).expect("read engine.u");
        let pkg = xiii_script::ScriptPackage::load(
            "engine",
            data,
            &xiii_script::ScriptLimits::default(),
            &Limits::default(),
        )
        .expect("load engine.u");
        let names_of = |path: &str| -> Vec<String> {
            let e = pkg
                .export_by_path(path)
                .unwrap_or_else(|| panic!("enum {path} not found"));
            match pkg.objects.get(&e) {
                Some(xiii_script::ScriptObject::Enum(en)) => en
                    .names
                    .iter()
                    .map(|&n| pkg.name_text(n).to_owned())
                    .collect(),
                other => panic!("{path}: expected enum, got {other:?}"),
            }
        };
        assert_eq!(
            names_of("Shader.EOutputBlending"),
            [
                "OB_Normal",
                "OB_Masked",
                "OB_Modulate",
                "OB_Translucent",
                "OB_Invisible",
                "OB_AlphaBlend",
                "OB_Darken",
                "OB_Brighten",
                "OB_AddWhiteFog",
            ]
        );
        assert_eq!(
            names_of("FinalBlend.EFrameBufferBlending"),
            [
                "FB_Overwrite",
                "FB_Modulate",
                "FB_AlphaBlend",
                "FB_AlphaModulate_MightNotFogCorrectly",
                "FB_Translucent",
                "FB_Darken",
                "FB_Brighten",
                "FB_Invisible",
            ]
        );
        assert_eq!(
            names_of("TexOscillator.ETexOscillationType"),
            ["OT_Pan", "OT_Stretch"]
        );
    }

    /// Opt-in material-resolution invariants on the three maps the task names: no `fail.*`
    /// counters, every surface resolves a base texture, and the feature counters are printed.
    #[test]
    fn opt_in_material_resolution_on_three_maps() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        for map in ["Plage00", "Plage01", "Banque01"] {
            let scene = import_map(&mut cache, map).expect("import");
            let get = |k: &str| scene.counters.get(k).copied().unwrap_or(0);
            let fails: Vec<_> = scene
                .counters
                .keys()
                .filter(|k| k.starts_with("fail."))
                .collect();
            assert!(fails.is_empty(), "{map}: {fails:?} {:?}", scene.examples);
            let missing: Vec<_> = scene
                .materials
                .iter()
                .enumerate()
                .filter(|(_, m)| m.base.is_none())
                .map(|(i, m)| format!("{i}:{:?}", m.class_chain))
                .collect();
            assert!(
                missing.is_empty(),
                "{map}: {} materials without a base texture: {missing:?}",
                missing.len()
            );
            let mut blends: std::collections::BTreeMap<&str, usize> = Default::default();
            let mut unsupported: std::collections::BTreeMap<&str, usize> = Default::default();
            let mut uv_ops = 0usize;
            for m in &scene.materials {
                *blends.entry(m.blend.name()).or_default() += 1;
                uv_ops += m.uv_transform.len();
                for u in &m.unsupported {
                    *unsupported.entry(u.as_str()).or_default() += 1;
                }
            }
            println!(
                "[materials] {map}: materials {} blends {blends:?} two_sided {} uv_ops {uv_ops} unsupported {unsupported:?}",
                scene.materials.len(),
                get("material.two_sided.mesh")
                    + get("material.two_sided.bsp")
                    + get("material.two_sided.terrain"),
            );
        }
    }

    /// `XIII_GOG_DIR` resolved against the workspace root, or `None` in CI.
    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus guard and evidence table for the static-mesh collision flags. Prints the
    /// per-mesh table (used by `local/reports/item1g-*.txt`) and asserts the measured structural
    /// facts: `UseSimpleBoxCollision`/`UseSimpleKarmaCollision` are never set in this corpus, so
    /// the box soup is always the per-triangle set 0 while `UseSimpleLineCollision` selects the
    /// simplified set for the line soup.
    #[test]
    fn opt_in_collision_flag_corpus_evidence() {
        let Some(path) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        // ---- meshes used by placed actors on the three maps ----
        let mut cache = PackageCache::open(&path).expect("open install");
        let mut used: BTreeMap<String, (bool, bool, bool, usize, usize, usize)> = BTreeMap::new();
        for map in ["Plage00", "Plage01", "Banque01"] {
            let map_pkg = cache.map(map).expect("map");
            let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
            let mut per_map: BTreeMap<String, (bool, bool, bool, usize, usize, usize)> =
                BTreeMap::new();
            for a in &actors.static_mesh_actors {
                let class_short = a.class.rsplit('.').next().unwrap_or("");
                if class_short.ends_with("Emitter") || a.hidden {
                    continue;
                }
                if a.draw_type.is_some_and(|dt| dt != 8) {
                    continue;
                }
                let Some(r) = a.static_mesh else { continue };
                let Ok((pkg, idx)) = cache.resolve(&map_pkg, r) else {
                    continue;
                };
                let Ok(props) = pkg.package.read_object_properties(
                    &pkg.data,
                    idx,
                    &xiii_package::Limits::default(),
                ) else {
                    continue;
                };
                let p = xiii_decode::common::Props::new(&pkg.package, &props);
                let label = format!(
                    "{}.{}",
                    pkg.name,
                    pkg.package
                        .object_path(ObjectRef::Export(idx as u32))
                        .unwrap_or("?")
                );
                let Ok(m) = decode_static_mesh(&pkg.package, &pkg.data, idx) else {
                    continue;
                };
                let rec = (
                    p.bool("UseSimpleLineCollision").unwrap_or(false),
                    p.bool("UseSimpleBoxCollision").unwrap_or(false),
                    p.bool("UseSimpleKarmaCollision").unwrap_or(false),
                    m.collision[0].triangles.len(),
                    m.collision[1].triangles.len(),
                    m.indices.len() / 3,
                );
                per_map.entry(label.clone()).or_insert(rec);
                used.entry(label).or_insert(rec);
            }
            let (mut l, mut b, mut k, mut s0, mut s1, mut r) = (0, 0, 0, 0, 0, 0);
            for v in per_map.values() {
                l += v.0 as usize;
                b += v.1 as usize;
                k += v.2 as usize;
                s0 += v.3;
                s1 += v.4;
                r += v.5;
            }
            println!(
                "[evidence] {map}: unique meshes {} line_flag {} box_flag {} karma_flag {} set0_tris {} set1_tris {} render_tris {}",
                per_map.len(),
                l,
                b,
                k,
                s0,
                s1,
                r
            );
            for (label, v) in &per_map {
                println!(
                    "[evidence]   {map} {label}: line={} box={} karma={} set0={} set1={} render={}",
                    v.0, v.1, v.2, v.3, v.4, v.5
                );
            }
        }
        let (mut l, mut b, mut k, mut s0, mut s1, mut r) = (0, 0, 0, 0, 0, 0);
        for v in used.values() {
            l += v.0 as usize;
            b += v.1 as usize;
            k += v.2 as usize;
            s0 += v.3;
            s1 += v.4;
            r += v.5;
        }
        println!(
            "[evidence] three-map union: unique meshes {} line_flag {} box_flag {} karma_flag {} set0_tris {} set1_tris {} render_tris {}",
            used.len(),
            l,
            b,
            k,
            s0,
            s1,
            r
        );
        assert!(
            !used.is_empty(),
            "no placed static meshes on the three maps"
        );
        assert_eq!(
            b, 0,
            "UseSimpleBoxCollision must be unset on the three maps"
        );
        assert_eq!(
            k, 0,
            "UseSimpleKarmaCollision must be unset on the three maps"
        );
        assert!(
            l > 0,
            "UseSimpleLineCollision must be set on the three maps"
        );

        // ---- corpus-wide totals over every .usx package ----
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("usx")) {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&path, &mut files);
        let (mut meshes, mut decode_fail, mut pkg_fail) = (0usize, 0usize, 0usize);
        let (mut cl, mut cb, mut ck) = (0usize, 0usize, 0usize);
        // Property-presence evidence: Unreal only serializes a property that differs from the
        // class default, so a tagged `false` proves the default was `true` and vice versa. A
        // tagged `true` is exactly `cl`/`cb` (an untagged property reads as `false`).
        let (mut box_false_present, mut line_false_present) = (0usize, 0usize);
        let (mut c0, mut c1, mut cr) = (0usize, 0usize, 0usize);
        let (mut e0, mut e1, mut both, mut neither) = (0usize, 0usize, 0usize, 0usize);
        for f in &files {
            let Ok(data) = std::fs::read(f) else {
                pkg_fail += 1;
                continue;
            };
            let Ok(package) = xiii_package::Package::parse(&data, &xiii_package::Limits::default())
            else {
                pkg_fail += 1;
                continue;
            };
            for i in 0..package.exports().len() {
                if !package.export_class_path(i).is_some_and(|c| {
                    c.eq_ignore_ascii_case(xiii_decode::static_mesh::STATIC_MESH_CLASS)
                }) {
                    continue;
                }
                meshes += 1;
                let Ok(m) = decode_static_mesh(&package, &data, i) else {
                    decode_fail += 1;
                    continue;
                };
                let (f_l, f_b, f_k, presence) = package
                    .read_object_properties(&data, i, &xiii_package::Limits::default())
                    .map(|props| {
                        let p = xiii_decode::common::Props::new(&package, &props);
                        (
                            p.bool("UseSimpleLineCollision").unwrap_or(false),
                            p.bool("UseSimpleBoxCollision").unwrap_or(false),
                            p.bool("UseSimpleKarmaCollision").unwrap_or(false),
                            (
                                p.get("UseSimpleLineCollision").is_some(),
                                p.get("UseSimpleBoxCollision").is_some(),
                            ),
                        )
                    })
                    .unwrap_or((false, false, false, (false, false)));
                cl += f_l as usize;
                cb += f_b as usize;
                ck += f_k as usize;
                if presence.0 && !f_l {
                    line_false_present += 1;
                }
                if presence.1 && !f_b {
                    box_false_present += 1;
                }
                let n0 = m.collision[0].triangles.len();
                let n1 = m.collision[1].triangles.len();
                c0 += n0;
                c1 += n1;
                cr += m.indices.len() / 3;
                match (n0 > 0, n1 > 0) {
                    (true, true) => both += 1,
                    (true, false) => e0 += 1,
                    (false, true) => e1 += 1,
                    (false, false) => neither += 1,
                }
            }
        }
        println!(
            "[evidence] corpus: usx files {} pkg_fail {} static_meshes {} decode_fail {}; line_flag_true {} (tagged_false {}) box_flag_true {} (tagged_false {}) karma_flag_true {}; set0_tris {} set1_tris {} render_tris {}; sets(d0,1,both,none) ({e0},{e1},{both},{neither})",
            files.len(),
            pkg_fail,
            meshes,
            decode_fail,
            cl,
            line_false_present,
            cb,
            box_false_present,
            ck,
            c0,
            c1,
            cr
        );
        assert_eq!(pkg_fail, 0, "a .usx package failed to parse");
        assert_eq!(decode_fail, 0, "a static mesh failed to decode");
        // No mesh tags a box/line flag equal to its default; the tagged `false` values prove the
        // box default is `true` and the tagged `true` values prove the line default is `false`.
        assert_eq!(cb, 0, "no mesh must tag UseSimpleBoxCollision=true");
        assert!(
            box_false_present > 0,
            "meshes tagging UseSimpleBoxCollision=false must exist (proves the default is true)"
        );
        assert_eq!(
            line_false_present, 0,
            "no mesh must tag UseSimpleLineCollision=false"
        );
        assert!(
            cl > 0,
            "UseSimpleLineCollision=true must be tagged somewhere in the corpus"
        );
        assert_eq!(
            ck, 0,
            "UseSimpleKarmaCollision must be untagged corpus-wide"
        );
        assert!(
            e1 > 0,
            "there must be meshes whose set 0 is empty and set 1 non-empty"
        );
    }

    #[test]
    fn banque01_staircase_simple_collision_is_imported() {
        let Some(path) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        // `Staticbanque.bankesca2` sets `UseSimpleLineCollision=true` and has an empty
        // per-triangle collision set 0 but 20 simplified triangles in set 1. Under the corrected
        // rule the line flag selects set 1, and the box flag's documented default (`true`) also
        // selects set 1, so the staircase is in both soups and the extent walker can spawn on it.
        let mut cache = PackageCache::open(&path).expect("open install");
        let scene = import_map(&mut cache, "Banque01").expect("import Banque01");
        let stair_contributors = scene
            .collision_sources
            .iter()
            .filter(|s| s.contains("bankesca2"))
            .count();
        assert!(
            stair_contributors > 0,
            "no source contains 'bankesca2'; staircase collision was skipped"
        );
        let in_soup = |indices: &[u32]| {
            indices.iter().any(|&i| {
                scene.collision_sources[scene.collision_triangles[i as usize].1 as usize]
                    .contains("bankesca2")
            })
        };
        assert!(
            in_soup(&scene.collision_box),
            "the box soup must contain bankesca2 (box default true selects set 1)"
        );
        assert!(
            in_soup(&scene.collision_line),
            "the line soup must contain bankesca2 (UseSimpleLineCollision selects set 1)"
        );
        // PathNode119 (export 95) is based on StaticMeshActor707 -> bankesca2 at Unreal
        // (78.0159, -4080.3564, 1076.8217). A downward ray against the box (extent) soup must
        // find the stair floor within the 3 m FindSpot drop that `--reach-test` uses.
        let node = to_bevy_position([78.0159, -4080.3564, 1076.8217]);
        let world = xiii_collision::CollisionWorld::new(scene.box_collision());
        let hit = world
            .ray(node, [node[0], node[1] - 3.0, node[2]])
            .expect("the staircase floor must be within 3 m below PathNode119");
        assert!(
            hit.normal[1] > 0.7,
            "PathNode119 floor normal {:?} is not walkable",
            hit.normal
        );
    }

    /// Opt-in baked-lighting invariants on the three maps that the task names: every placed
    /// instance's colour count matches its mesh's vertex count (no `mismatch`), decoding never
    /// fails, at least one object is lit, and the terrain colour grid is complete and
    /// conflict-free.
    #[test]
    fn opt_in_baked_lighting_invariants() {
        let Some(path) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        for map in ["Plage00", "Plage01", "Banque01"] {
            let scene = import_map(&mut cache, map).expect("import");
            let get = |k: &str| scene.counters.get(k).copied().unwrap_or(0);
            assert_eq!(
                get("lighting.objects.mismatch"),
                0,
                "{map}: colour/mesh count mismatch: {:?}",
                scene.examples
            );
            let fails: Vec<_> = scene
                .counters
                .keys()
                .filter(|k| k.starts_with("fail."))
                .collect();
            assert!(fails.is_empty(), "{map}: {fails:?} {:?}", scene.examples);
            assert!(
                get("lighting.instances.decoded") > 0,
                "{map}: no StaticMeshInstance decoded"
            );
            assert!(
                get("lighting.objects.lit") > 0,
                "{map}: no objects carried baked colours"
            );
            for o in &scene.objects {
                if let Some(c) = &o.colors {
                    assert_eq!(
                        c.len(),
                        scene.meshes[o.mesh].positions.len(),
                        "{map}: {} colour count",
                        o.path
                    );
                }
            }
            if map != "Banque01" {
                assert!(
                    get("lighting.terrain.colors") > 0,
                    "{map}: no terrain colours"
                );
                assert_eq!(get("lighting.terrain.colors_missing"), 0, "{map}");
                assert_eq!(get("lighting.terrain.color_conflicts"), 0, "{map}");
            }
            println!(
                "[lighting] {map}: lit {} unlit {} mismatch {} instances {} colours {} terrain-colours {}",
                get("lighting.objects.lit"),
                get("lighting.objects.unlit"),
                get("lighting.objects.mismatch"),
                get("lighting.instances.decoded"),
                get("lighting.colors.rgba"),
                get("lighting.terrain.colors"),
            );
        }
    }
}
