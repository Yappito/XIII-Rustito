//! HUD presentation for `--play`: the game's own `HUD.PostRender(Canvas)`.
//!
//! Requirement (item10): every **rendered frame** (not every fixed tick) the host hands the
//! script HUD a host-created `Engine.Canvas`, calls the HUD's `PostRender`, and renders the
//! typed [`DrawCommand`]s the `Engine.Canvas` natives recorded. The VM owns the Canvas geometry
//! (`ClipX`/`ClipY`, `CurX`/`CurY`, `DrawColor`, `Font`, `Style`); the host owns the window size
//! and the pixels.
//!
//! Fonts are decoded from `TexturesPC/XIIIFonts.utx` with [`xiii_decode::font`]: the four
//! corpus fonts (`PoliceF20`, `PoliceF16`, `XIIIConsoleFont`, `XIIISmallFont`), their page
//! textures and their glyph rectangles. `StrLen`/`TextSize` measure with the decoded metrics.
//!
//! **Host bridge (documented deviation).** The decoded `XIIIFontInfo` class default properties
//! are all `None`, and no script assignment of the HUD's `SmallFont`/`MedFont`/`BigFont`/
//! `LargeFont` was found in the decoded bytecode, so the host assigns the decoded fonts to the
//! HUD by size before the first frame. The assignment is reported at startup. Tiles whose
//! material did not resolve to a decodable `Engine.Texture` render as nothing and are counted in
//! [`HudRuntime::missing_materials`] (never a silent success).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::PrimaryWindow;

use xiii_decode::font::{Font as DecodedFont, FontGlyph, decode_font};
use xiii_decode::texture::{RgbaImage, decode_texture};
use xiii_package::ObjectRef as PkgRef;
use xiii_script::canvas::{CanvasFonts, DrawCommand};
use xiii_script::{ObjRef, ObjectId, Value, Vm};
use xiii_world::PackageCache;
use xiii_world::runtime;

use super::session::Session;

/// One font page: its uploaded texture handle and texel size.
pub struct HudFontPage {
    /// Uploaded page image.
    pub handle: Handle<Image>,
    /// Page width in texels.
    pub width: u32,
    /// Page height in texels.
    pub height: u32,
}

/// One decoded font plus its uploaded page textures.
pub struct HudFont {
    /// `Package.Object` path, e.g. `XIIIFonts.PoliceF20`.
    pub path: String,
    /// Page textures, indexed by [`FontGlyph::page`].
    pub pages: Vec<HudFontPage>,
    /// Decoded metrics and glyph table.
    pub font: DecodedFont,
}

impl HudFont {
    /// Glyph for a character code.
    pub fn glyph(&self, code: u32) -> Option<&FontGlyph> {
        self.font.glyph(code)
    }

    /// Width and height of `text` in canvas units.
    pub fn measure(&self, text: &str) -> (f32, f32) {
        self.font.text_size(text)
    }
}

/// All decoded HUD fonts, addressable by full path or leaf name (case-insensitive).
pub struct FontDb {
    /// Fonts in decode order.
    pub fonts: Vec<HudFont>,
    by_key: HashMap<String, usize>,
}

impl FontDb {
    /// Finds a font by `Package.Object` path or leaf name (case-insensitive).
    pub fn find(&self, key: &str) -> Option<&HudFont> {
        // Strip a leading `Package.` if present, and try both the full key and the leaf.
        let lowered = key.to_ascii_lowercase();
        let leaf = lowered.rsplit('.').next().unwrap_or(&lowered).to_owned();
        self.by_key
            .get(&lowered)
            .or_else(|| self.by_key.get(&leaf))
            .map(|i| &self.fonts[*i])
    }

    /// Font assigned to the HUD's `SmallFont`/`MedFont`/`BigFont`/`LargeFont` (by size order).
    /// Documented host bridge: the script's own font selection has no decoded assignment.
    fn ordered(&self) -> Vec<&HudFont> {
        let mut fonts: Vec<&HudFont> = self.fonts.iter().collect();
        fonts.sort_by_key(|f| {
            f.font
                .pages
                .iter()
                .flat_map(|p| p.glyphs.iter())
                .map(|g| g.v_size)
                .max()
                .unwrap_or(0)
        });
        fonts
    }
}

/// `CanvasFonts` provider installed in the VM (measures `StrLen`/`TextSize`).
struct VmFonts(Arc<FontDb>);

impl CanvasFonts for VmFonts {
    fn measure(&self, font: &str, text: &str) -> Option<(f32, f32)> {
        self.0.find(font).map(|f| f.measure(text))
    }
}

/// Runtime HUD state: the script object pair, the decoded fonts and the current command list.
#[derive(Resource)]
pub struct HudRuntime {
    /// Script HUD the renderer drives (`XIIIBaseHud`), if found.
    pub hud: Option<ObjectId>,
    /// Host-created `Canvas`.
    pub canvas: Option<ObjectId>,
    /// Decoded fonts.
    pub fonts: Arc<FontDb>,
    /// Commands recorded by the last `PostRender`.
    pub commands: Vec<DrawCommand>,
    /// Bevy UI entities for those commands (rebuilt each frame).
    entities: Vec<Entity>,
    /// Clip size (canvas pixels) used for the last frame.
    pub clip: [f32; 2],
    /// First `PostRender` error, if any (reported in the overlay).
    pub error: Option<String>,
    /// Frames on which `PostRender` was called.
    pub frames: u64,
    /// Commands recorded so far (sum over frames).
    pub total_commands: u64,
    /// Glyph UI nodes spawned by the last [`draw`] call.
    pub glyphs_drawn: u64,
    /// Materials referenced by `DrawTile` that did not resolve to a texture.
    pub missing_materials: BTreeMap<String, u64>,
    /// Uploaded texture cache (`Package.Object` -> image; `None` = tried and failed).
    texture_cache: HashMap<String, Option<Handle<Image>>>,
    /// Package cache for lazy texture resolution.
    packages: PackageCache,
}

impl HudRuntime {
    /// The HUD script object path (display name) when found.
    pub fn hud_name(&self, vm: &Vm<'_>) -> Option<String> {
        self.hud.map(|h| vm.objects[h as usize].name.clone())
    }
}

/// Loads `XIIIFonts.utx`, decodes every `Engine.Font` and uploads its page textures.
pub fn load_fonts(game_dir: &Path, images: &mut Assets<Image>) -> Result<FontDb, String> {
    let mut cache = PackageCache::open(game_dir)?;
    let loaded = cache
        .get("XIIIFonts")
        .map_err(|e| format!("opening XIIIFonts.utx: {e}"))?;
    let package = &loaded.package;
    let mut fonts = Vec::new();
    let mut by_key = HashMap::new();
    for export in 0..package.exports().len() {
        if !package
            .export_class_path(export)
            .is_some_and(|c| c.eq_ignore_ascii_case(xiii_decode::font::FONT_CLASS))
        {
            continue;
        }
        let path = package
            .object_path(PkgRef::Export(export as u32))
            .unwrap_or("?")
            .to_owned();
        let font_path = format!("XIIIFonts.{path}");
        let font =
            decode_font(package, &loaded.data, export).map_err(|e| format!("{font_path}: {e}"))?;
        let mut pages = Vec::with_capacity(font.pages.len());
        for page in &font.pages {
            let (texture_export, label) = match page.texture {
                PkgRef::Export(i) => (
                    i as usize,
                    package
                        .object_path(PkgRef::Export(i))
                        .unwrap_or("?")
                        .to_owned(),
                ),
                other => return Err(format!("{font_path}: page texture {other:?} is not local")),
            };
            let texture = decode_texture(package, &loaded.data, texture_export)
                .map_err(|e| format!("XIIIFonts.{label}: {e}"))?;
            let image = texture
                .decode_mip(0)
                .map_err(|e| format!("XIIIFonts.{label}: {e}"))?;
            let width = image.width;
            let height = image.height;
            let opaque = image.pixels.chunks(4).filter(|p| p[3] > 0).count();
            println!(
                "[hud]   page XIIIFonts.{label}: {width}x{height}, {opaque} texels with alpha>0"
            );
            pages.push(HudFontPage {
                handle: images.add(image_from_font_page(&image)),
                width,
                height,
            });
        }
        let leaf = path
            .rsplit('.')
            .next()
            .unwrap_or(&path)
            .to_ascii_lowercase();
        by_key.insert(font_path.to_ascii_lowercase(), fonts.len());
        by_key.insert(leaf, fonts.len());
        println!(
            "[hud] decoded font {font_path}: {} page(s) {:?}, {} glyphs",
            pages.len(),
            pages
                .iter()
                .map(|p| (p.width, p.height))
                .collect::<Vec<_>>(),
            font.glyph_count()
        );
        fonts.push(HudFont {
            path: font_path,
            pages,
            font,
        });
    }
    if fonts.is_empty() {
        return Err("XIIIFonts.utx has no decodable Engine.Font exports".to_owned());
    }
    Ok(FontDb { fonts, by_key })
}

/// Uploads a font page as a white RGBA image whose alpha is the glyph mask. The XIII font
/// textures are palette bitmaps whose non-transparent texels are dark (the palette colour, used
/// by the retail renderer as coverage, not as the drawn colour), so the UV colour is replaced
/// with white and the `DrawColor` tint supplies the actual colour.
fn image_from_font_page(img: &RgbaImage) -> Image {
    let mut pixels = img.pixels.clone();
    for p in pixels.as_chunks_mut::<4>().0 {
        p[0] = 255;
        p[1] = 255;
        p[2] = 255;
    }
    let page = RgbaImage {
        width: img.width,
        height: img.height,
        pixels,
    };
    image_from_rgba(&page)
}

fn image_from_rgba(img: &RgbaImage) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        img.pixels.clone(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        ..ImageSamplerDescriptor::linear()
    });
    image
}

/// A `Color` struct value with the `b`,`g`,`r`,`a` member names the VM's struct access uses.
fn color_value(r: u8, g: u8, b: u8, a: u8) -> Value {
    Value::Struct(vec![
        ("b".to_owned(), Value::Byte(b)),
        ("g".to_owned(), Value::Byte(g)),
        ("r".to_owned(), Value::Byte(r)),
        ("a".to_owned(), Value::Byte(a)),
    ])
}

/// Builds the HUD runtime: decodes the fonts, creates the Canvas, assigns the host font bridge
/// and installs the VM font provider. Returns the runtime for insertion as a non-send resource.
pub fn setup(
    session: &mut Session,
    game_dir: &Path,
    images: &mut Assets<Image>,
) -> Result<HudRuntime, String> {
    let fonts = Arc::new(load_fonts(game_dir, images)?);
    let controller = session.controller;
    let vm = session.vm_mut();
    let set = vm.set();
    let canvas_class = runtime::resolve_class_path(set, "Engine.Canvas")
        .ok_or("Engine.Canvas class is not in the loaded script set")?;
    let canvas = vm
        .spawn(canvas_class, "HUDCanvas")
        .map_err(|e| format!("creating Canvas: {e}"))?;
    vm.set_property(canvas, "DrawColor", 0, color_value(255, 255, 255, 255));
    vm.set_property(canvas, "BorderColor", 0, color_value(0, 0, 0, 0));
    vm.set_property(canvas, "ClipX", 0, Value::Float(0.0));
    vm.set_property(canvas, "ClipY", 0, Value::Float(0.0));
    vm.set_property(canvas, "CurX", 0, Value::Float(0.0));
    vm.set_property(canvas, "CurY", 0, Value::Float(0.0));
    vm.set_property(canvas, "OrgX", 0, Value::Float(0.0));
    vm.set_property(canvas, "OrgY", 0, Value::Float(0.0));
    vm.set_property(canvas, "Style", 0, Value::Byte(1));

    // Locate the script HUD (`PlayerController.myHUD`, else any live HUD actor).
    let hud = controller
        .and_then(|c| instance_prop(vm, c, "myHUD"))
        .or_else(|| find_hud(vm));
    if let Some(hud) = hud {
        // Host font bridge: assign the decoded fonts to the HUD's own font properties and to
        // the Canvas's (the engine's base `HUD.Use*Font` reads `Canvas.SmallFont`). The decoded
        // `XIIIFontInfo` defaults are all `None`, so this mapping is a documented host
        // deviation; fonts are ordered by glyph height.
        let ordered = fonts.ordered();
        let pick = |i: usize| {
            ordered
                .get(i.min(ordered.len().saturating_sub(1)))
                .map(|f| f.path.clone())
        };
        let names = [
            ("SmallFont", pick(0)),
            ("MedFont", pick(1)),
            ("BigFont", pick(2)),
            ("LargeFont", pick(3)),
            ("HugeFont", pick(3)),
            ("NumericLargeFont", pick(3)),
            ("MsgFont", pick(2)),
            ("ItemFont", pick(0)),
            ("HeadFont", pick(3)),
            ("DialogFont", pick(1)),
        ];
        for (prop, font) in &names {
            if let Some(path) = font {
                // `set_property` returns false for a property the HUD does not declare; that is
                // fine (the name list is a superset of the decoded font properties).
                let _ = vm.set_property(hud, prop, 0, Value::Name(path.clone()));
                let _ = vm.set_property(canvas, prop, 0, Value::Name(path.clone()));
            }
        }
        println!(
            "[hud] script HUD {} found; host font bridge: {}",
            vm.objects[hud as usize].name,
            names
                .iter()
                .filter_map(|(p, f)| f.as_ref().map(|f| format!("{p}={f}")))
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!("[hud] no live HUD actor found; PostRender will not be called");
    }
    vm.set_canvas_fonts(Box::new(VmFonts(fonts.clone())));

    let packages = PackageCache::open(game_dir)?;
    Ok(HudRuntime {
        hud,
        canvas: Some(canvas),
        fonts,
        commands: Vec::new(),
        entities: Vec::new(),
        clip: [0.0, 0.0],
        error: None,
        frames: 0,
        total_commands: 0,
        glyphs_drawn: 0,
        missing_materials: BTreeMap::new(),
        texture_cache: HashMap::new(),
        packages,
    })
}

fn instance_prop(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(p))))
            if vm.objects.get(*p as usize).is_some_and(|o| !o.deleted) =>
        {
            Some(*p)
        }
        _ => None,
    }
}

/// First live actor that is a `HUD`.
pub(crate) fn find_hud(vm: &Vm<'_>) -> Option<ObjectId> {
    (0..vm.objects.len() as ObjectId).find(|&i| {
        let o = &vm.objects[i as usize];
        o.is_actor && !o.deleted && vm.is_a(i, "HUD")
    })
}

/// Per-rendered-frame system: set `ClipX`/`ClipY` from the window, call the HUD's `PostRender`,
/// drain the recorded commands into [`HudRuntime`].
pub fn refresh(
    window: Query<&Window, With<PrimaryWindow>>,
    mut session: NonSendMut<Result<Session, String>>,
    mut hud: ResMut<HudRuntime>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    let Ok(session) = session.as_mut() else {
        return;
    };
    let (Some(canvas), Some(hud_id)) = (hud.canvas, hud.hud) else {
        return;
    };
    let size = window
        .single()
        .map(|w| [w.width(), w.height()])
        .unwrap_or([1280.0, 720.0]);
    hud.clip = size;
    let vm = session.vm_mut();
    vm.set_property(canvas, "ClipX", 0, Value::Float(size[0]));
    vm.set_property(canvas, "ClipY", 0, Value::Float(size[1]));
    let arg = Value::Object(Some(ObjRef::Instance(canvas)));
    match vm.send_event(hud_id, "PostRender", vec![arg]) {
        Ok(_) => {
            hud.frames += 1;
        }
        Err(e) => {
            if hud.error.is_none() {
                hud.error = Some(format!("{e}"));
                eprintln!("[hud] PostRender failed: {e}");
            }
        }
    }
    let commands = vm.drain_canvas();
    hud.total_commands += commands.len() as u64;
    hud.commands = commands;
    perf.span("hud_postrender", t0);
}

/// Per-rendered-frame system: rebuild the Bevy UI nodes from [`HudRuntime::commands`].
#[allow(clippy::too_many_arguments)]
pub fn draw(
    mut commands: Commands,
    mut hud: ResMut<HudRuntime>,
    mut images: ResMut<Assets<Image>>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    for e in hud.entities.drain(..) {
        commands.entity(e).despawn();
    }
    let clip = hud.clip;
    if clip[0] <= 0.0 || clip[1] <= 0.0 {
        return;
    }
    let cmds = std::mem::take(&mut hud.commands);
    // The fonts and package cache are borrowed immutably for the loop; `hud` is mutated only for
    // the missing-material counter, which is collected separately.
    let fonts = hud.fonts.clone();
    let mut missing: BTreeMap<String, u64> = BTreeMap::new();
    let mut glyphs = 0u64;
    for cmd in &cmds {
        match cmd {
            DrawCommand::Rect {
                x,
                y,
                xl,
                yl,
                color,
                ..
            } => {
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(*x),
                            top: px(*y),
                            width: px(*xl),
                            height: px(*yl),
                            ..default()
                        },
                        BackgroundColor(to_color(*color)),
                        GlobalZIndex(10),
                    ))
                    .id();
                hud.entities.push(e);
            }
            DrawCommand::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                ..
            } => {
                let (left, top) = (x1.min(*x2), y1.min(*y2));
                let (w, h) = ((x2 - x1).abs().max(1.0), (y2 - y1).abs().max(1.0));
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(left),
                            top: px(top),
                            width: px(w),
                            height: px(h),
                            ..default()
                        },
                        BackgroundColor(to_color(*color)),
                        GlobalZIndex(10),
                    ))
                    .id();
                hud.entities.push(e);
            }
            DrawCommand::Tile {
                material,
                x,
                y,
                xl,
                yl,
                u,
                v,
                ul,
                vl,
                color,
                ..
            } => {
                let Some(path) = material else {
                    continue;
                };
                let handle = match hud.texture_cache.get(path) {
                    Some(h) => h.clone(),
                    None => {
                        let decoded = decode_texture_path(&mut hud.packages, path, &mut images);
                        hud.texture_cache.insert(path.clone(), decoded.clone());
                        decoded
                    }
                };
                let Some(handle) = handle else {
                    *missing.entry(path.clone()).or_default() += 1;
                    continue;
                };
                let rect = if *ul > 0.0 && *vl > 0.0 {
                    Some(Rect::new(*u, *v, *u + *ul, *v + *vl))
                } else {
                    None
                };
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(*x),
                            top: px(*y),
                            width: px(*xl),
                            height: px(*yl),
                            ..default()
                        },
                        ImageNode {
                            image: handle,
                            rect,
                            color: to_color(*color),
                            ..default()
                        },
                        GlobalZIndex(10),
                    ))
                    .id();
                hud.entities.push(e);
            }
            DrawCommand::Text {
                text,
                x,
                y,
                font,
                color,
                ..
            } => {
                let Some(font) = font.as_deref().and_then(|f| fonts.find(f)) else {
                    continue;
                };
                let mut pen_x = *x;
                for ch in text.chars() {
                    let Some(glyph) = font.glyph(ch as u32) else {
                        continue;
                    };
                    let Some(page) = font.pages.get(glyph.page) else {
                        continue;
                    };
                    if glyph.u_size <= 0 || glyph.v_size <= 0 {
                        continue;
                    }
                    let gx = glyph.start_u as f32;
                    let gy = glyph.start_v as f32;
                    let gw = glyph.u_size as f32;
                    let gh = glyph.v_size as f32;
                    // Skip glyphs entirely outside the clip.
                    if pen_x + gw < 0.0 || pen_x > clip[0] || *y + gh < 0.0 || *y > clip[1] {
                        pen_x += gw;
                        continue;
                    }
                    let e = commands
                        .spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: px(pen_x),
                                top: px(*y),
                                width: px(gw),
                                height: px(gh),
                                ..default()
                            },
                            ImageNode {
                                image: page.handle.clone(),
                                rect: Some(Rect::new(gx, gy, gx + gw, gy + gh)),
                                color: to_color(*color),
                                ..default()
                            },
                            GlobalZIndex(10),
                        ))
                        .id();
                    hud.entities.push(e);
                    glyphs += 1;
                    pen_x += gw;
                }
            }
        }
    }
    hud.glyphs_drawn = glyphs;
    for (path, n) in missing {
        let entry = hud.missing_materials.entry(path).or_default();
        if *entry == 0 {
            println!("[hud] DrawTile material not decoded to a texture (counted, not drawn)");
        }
        *entry += n;
    }
    perf.span("hud_draw", t0);
}

fn to_color(c: [u8; 4]) -> Color {
    Color::srgba_u8(c[0], c[1], c[2], c[3])
}

/// Resolves a `Package.Object` path to a decoded texture image handle.
fn decode_texture_path(
    cache: &mut PackageCache,
    path: &str,
    images: &mut Assets<Image>,
) -> Option<Handle<Image>> {
    let (package, object) = path.rsplit_once('.')?;
    let loaded = cache.get(package).ok()?;
    let pkg = &loaded.package;
    let export = (0..pkg.exports().len()).find(|&e| {
        pkg.object_path(PkgRef::Export(e as u32))
            .is_some_and(|q| q.eq_ignore_ascii_case(object))
    })?;
    if !pkg
        .export_class_path(export)
        .is_some_and(|c| c.eq_ignore_ascii_case("Engine.Texture"))
    {
        return None;
    }
    let texture = decode_texture(pkg, &loaded.data, export).ok()?;
    let image = texture.decode_mip(0).ok()?;
    Some(images.add(image_from_rgba(&image)))
}
