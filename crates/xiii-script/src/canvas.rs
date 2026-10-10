//! `Engine.Canvas`: the VM-side half of the script-drawn HUD.
//!
//! The engine's `Canvas` is a native object the renderer hands to `HUD.PostRender` every frame.
//! The VM provides the object and its natives, but records the draw calls instead of drawing:
//! each native appends a typed [`DrawCommand`] to a per-frame list the host drains and renders
//! (Bevy 2D overlay in `--play`; tests read the commands directly).
//!
//! Geometry lives on the `Canvas` instance itself, exactly as the scripts use it: `CurX`/`CurY`
//! (current pen, set by `SetPos`), `OrgX`/`OrgY` (`SetOrigin`), `ClipX`/`ClipY` (the clip/screen
//! size), `DrawColor` (a `Color` struct), `Font`, `Style` and `bCenter`. `StrLen`/`TextSize` need
//! font metrics: the host installs a [`CanvasFonts`] provider (decoded from `Engine.Font`
//! packages by `xiii-decode::font`) and the natives measure with it. Without a provider the
//! measurement is `(0,0)` and a [`TraceKind::Note`] is recorded — never a silent success.
//!
//! `Canvas.MakeColor` is already registered by `registry.rs`; this module deliberately does not
//! duplicate it.

use crate::registry::{NativeCtx, NativeDef, NativeOutcome, NativeStatus};
use crate::value::{ObjRef, Value};
use crate::vm::{TraceKind, VideoTiming, Vm, VmErrorKind, VmResult};

/// Host font metrics used by `StrLen`/`TextSize`.
pub trait CanvasFonts {
    /// Width and height of `text` in the font named by `font` (a `Package.Object` path or the
    /// host's font label), in Unreal canvas units. `None` when the font is not available.
    fn measure(&self, font: &str, text: &str) -> Option<(f32, f32)>;
}

/// One recorded canvas draw command.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawCommand {
    /// `Canvas.DrawText` / `DrawTextClipped` / `DrawTextJustified`.
    Text {
        /// The string to draw.
        text: String,
        /// Pen X (already includes `OrgX`).
        x: f32,
        /// Pen Y (already includes `OrgY`).
        y: f32,
        /// Font object path, when the script set `Canvas.Font`.
        font: Option<String>,
        /// `DrawColor` RGBA.
        color: [u8; 4],
        /// Clip size (`ClipX`, `ClipY`).
        clip: [f32; 2],
        /// `bCenter`: centre the string on `x`.
        center: bool,
        /// UE2 `ERenderStyle`.
        style: u8,
        /// `EJustification` (0 left, 1 centre, 2 right).
        justify: u8,
        /// Drawn with the clipped native.
        clipped: bool,
    },
    /// `Canvas.DrawTile` / `DrawTileClipped` / stretched/justified/scaled variants.
    Tile {
        /// Material object path, `None` for a solid colour rectangle.
        material: Option<String>,
        /// Draw X (already includes `OrgX`).
        x: f32,
        /// Draw Y (already includes `OrgY`).
        y: f32,
        /// Destination width (canvas units).
        xl: f32,
        /// Destination height.
        yl: f32,
        /// Source U (texels); `ul <= 0`/`vl <= 0` means the whole texture.
        u: f32,
        /// Source V (texels).
        v: f32,
        /// Source width (texels).
        ul: f32,
        /// Source height (texels).
        vl: f32,
        /// `DrawColor` RGBA.
        color: [u8; 4],
        /// UE2 `ERenderStyle`.
        style: u8,
        /// Clip size (`ClipX`, `ClipY`).
        clip: [f32; 2],
        /// `EJustification` (0 left, 1 centre, 2 right).
        justify: u8,
        /// Drawn with the clipped native.
        clipped: bool,
    },
    /// Solid colour rectangle (a null-material `DrawTile` or `DrawMsgboxBackground`).
    Rect {
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Width.
        xl: f32,
        /// Height.
        yl: f32,
        /// RGBA.
        color: [u8; 4],
        /// Clip size.
        clip: [f32; 2],
    },
    /// `Canvas.DrawLine`.
    Line {
        /// Start X.
        x1: f32,
        /// Start Y.
        y1: f32,
        /// End X.
        x2: f32,
        /// End Y.
        y2: f32,
        /// RGBA.
        color: [u8; 4],
        /// Clip size.
        clip: [f32; 2],
    },
}

/// Per-VM canvas command buffer plus the host font provider.
#[derive(Default)]
pub struct CanvasState {
    commands: Vec<DrawCommand>,
    fonts: Option<Box<dyn CanvasFonts>>,
    /// item16c host-owned menu configuration.
    pub menu: Option<MenuSettings>,
    /// Active presentation camera, supplied before PostRender; absent in headless simulation.
    pub screen_projection: Option<ScreenProjection>,
}

/// Renderer-independent world-to-clip snapshot. Columns consume Unreal world coordinates;
/// viewport dimensions are pixels. The host owns camera selection and projection conventions.
#[derive(Clone, Copy, Debug)]
pub struct ScreenProjection {
    /// Column-major homogeneous transform.
    pub clip_from_world: [[f32; 4]; 4],
    /// Width and height of the active viewport.
    pub viewport: [f32; 2],
}

impl ScreenProjection {
    /// Homogeneous divide followed by the canvas viewport mapping. No behind-camera clipping
    /// or denominator clamping: retail FSceneNode::Project divides by signed W as well.
    pub fn project(&self, point: [f32; 3]) -> Option<[f32; 3]> {
        if !point.iter().all(|v| v.is_finite())
            || !self.clip_from_world.iter().flatten().all(|v| v.is_finite())
            || !self.viewport.iter().all(|v| v.is_finite() && *v > 0.0)
        {
            return None;
        }
        let v = [point[0], point[1], point[2], 1.0];
        let clip: [f32; 4] = std::array::from_fn(|row| {
            (0..4)
                .map(|col| self.clip_from_world[col][row] * v[col])
                .sum()
        });
        Some([
            (clip[0] / clip[3] + 1.0) * self.viewport[0] * 0.5,
            (1.0 - clip[1] / clip[3]) * self.viewport[1] * 0.5,
            clip[2] / clip[3],
        ])
    }
}

fn world_to_screen(
    vm: &mut Vm<'_>,
    ctx: &NativeCtx,
    args: &mut [Value],
) -> VmResult<NativeOutcome> {
    let Some(Value::Vector(point)) = args.first() else {
        return Err(vm.err(VmErrorKind::Other(
            "WorldToScreen requires a vector Location".into(),
        )));
    };
    // Retail evaluates these optional arguments before PlayerCalcView selects the final view.
    // This host snapshot is that active view; overrides are not claimed as supported.
    if !ctx.omitted(1) || !ctx.omitted(2) {
        return Err(vm.err(VmErrorKind::Other(
            "WorldToScreen explicit camera arguments require PlayerCalcView support".into(),
        )));
    }
    let projection = vm.canvas.screen_projection.as_ref().ok_or_else(|| {
        vm.err(VmErrorKind::Other(
            "WorldToScreen requires an active presentation camera".into(),
        ))
    })?;
    let projected = projection.project(*point).ok_or_else(|| {
        vm.err(VmErrorKind::Other(
            "WorldToScreen invalid viewport, matrix or Location".into(),
        ))
    })?;
    Ok(NativeOutcome::Value(Value::Vector(projected)))
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn projection() -> ScreenProjection {
        // Forward depth = source X, right = source Y, up = source Z.
        ScreenProjection {
            clip_from_world: [
                [0.0, 0.0, 1.0, 1.0],
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 0.0],
            ],
            viewport: [1280.0, 720.0],
        }
    }

    #[test]
    fn projection_preserves_signed_depth_and_outside_viewport_positions() {
        let p = projection();
        assert_eq!(p.project([10.0, 0.0, 0.0]), Some([640.0, 360.0, 1.0]));
        assert_eq!(p.project([10.0, 20.0, 10.0]), Some([1920.0, 0.0, 1.0]));
        assert_eq!(p.project([-10.0, 20.0, 10.0]), Some([-640.0, 720.0, 1.0]));
        let eye_plane = p.project([0.0, 1.0, 1.0]).unwrap();
        assert!(eye_plane[0].is_infinite() && eye_plane[1].is_infinite());
    }

    #[test]
    fn projection_rejects_invalid_host_data_and_nonfinite_locations() {
        let mut p = projection();
        assert!(p.project([f32::NAN, 0.0, 0.0]).is_none());
        p.viewport[0] = 0.0;
        assert!(p.project([10.0, 0.0, 0.0]).is_none());
        p.viewport[0] = 1280.0;
        p.clip_from_world[0][0] = f32::INFINITY;
        assert!(p.project([10.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn world_to_screen_requires_a_camera_and_rejects_unsupported_overrides() {
        let set = crate::linker::ScriptSet::new();
        let mut vm = Vm::new(&set, crate::vm::VmLimits::default());
        let mut ctx = NativeCtx {
            this: 0,
            in_state_code: false,
            path: "Interaction.WorldToScreen".into(),
            omitted: vec![false, true, true],
        };
        let mut args = [Value::Vector([10.0, 0.0, 0.0]), Value::Void, Value::Void];
        assert!(world_to_screen(&mut vm, &ctx, &mut args).is_err());
        vm.canvas.screen_projection = Some(projection());
        assert_eq!(
            world_to_screen(&mut vm, &ctx, &mut args).unwrap(),
            NativeOutcome::Value(Value::Vector([640.0, 360.0, 1.0]))
        );
        ctx.omitted[1] = false;
        assert!(world_to_screen(&mut vm, &ctx, &mut args).is_err());
        ctx.omitted[1] = true;
        args[0] = Value::Void;
        assert!(world_to_screen(&mut vm, &ctx, &mut args).is_err());
    }
}

impl CanvasState {
    /// Commands recorded since the last [`CanvasState::drain`].
    pub fn commands(&self) -> &[DrawCommand] {
        &self.commands
    }

    /// Removes and returns the recorded commands (the per-frame drain).
    pub fn drain(&mut self) -> Vec<DrawCommand> {
        std::mem::take(&mut self.commands)
    }

    /// Drops the trigger-recorded commands without returning them.
    pub fn clear(&mut self) {
        self.commands.clear();
    }

    /// Installs the host font-metrics provider.
    pub fn set_fonts(&mut self, fonts: Box<dyn CanvasFonts>) {
        self.fonts = Some(fonts);
    }

    /// True when a font provider is installed.
    pub fn has_fonts(&self) -> bool {
        self.fonts.is_some()
    }

    fn measure(&self, font: Option<&str>, text: &str) -> Option<(f32, f32)> {
        match (self.fonts.as_ref(), font) {
            (Some(f), Some(path)) => f.measure(path, text),
            _ => None,
        }
    }

    fn push(&mut self, command: DrawCommand) {
        self.commands.push(command);
    }
}

// -------------------------------------------------------------------------------------------
// helpers

fn val(v: Value) -> VmResult<NativeOutcome> {
    Ok(NativeOutcome::Value(v))
}

fn bad(vm: &Vm<'_>, expected: &'static str, v: &Value) -> crate::vm::VmError {
    vm.err(VmErrorKind::TypeMismatch {
        expected,
        found: v.type_name(),
    })
}

fn float(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<f32> {
    match a.get(i) {
        Some(Value::Float(v)) => Ok(*v),
        Some(Value::Int(v)) => Ok(*v as f32),
        Some(Value::Byte(v)) => Ok(f32::from(*v)),
        Some(v) => Err(bad(vm, "float", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn text(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<String> {
    match a.get(i) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(v) => Err(bad(vm, "string", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn flag(a: &[Value], i: usize) -> bool {
    matches!(a.get(i), Some(Value::Bool(true)))
}

/// `Package.Object` path of a value: a static reference, an instance name, a host font label, or
/// a native class path. `None` for null/other values.
fn value_path(vm: &Vm<'_>, v: &Value) -> Option<String> {
    match v {
        Value::Object(Some(ObjRef::Static(g))) => Some(vm.set().path(*g)),
        Value::Object(Some(ObjRef::Instance(i))) => {
            vm.objects.get(*i as usize).map(|o| o.name.clone())
        }
        // An asset in a non-script package (a `.utx` texture, `.usx` mesh, ...) resolved by
        // `DynamicLoadObject`: the host needs its `Package.Object` path to decode and draw it.
        Value::Object(Some(r @ ObjRef::External(_))) => vm.external_path(r),
        Value::NativeClass(s) => Some(s.clone()),
        Value::Name(s) | Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

/// Reads a float property of the Canvas (missing -> `default`).
fn prop_f32(vm: &Vm<'_>, this: u32, name: &str, default: f32) -> f32 {
    match vm.get_property(this, name) {
        Some(Value::Float(v)) => *v,
        Some(Value::Int(v)) => *v as f32,
        Some(Value::Byte(v)) => f32::from(*v),
        _ => default,
    }
}

/// Reads a byte property of the Canvas (missing -> `default`).
fn prop_u8(vm: &Vm<'_>, this: u32, name: &str, default: u8) -> u8 {
    match vm.get_property(this, name) {
        Some(Value::Byte(v)) => *v,
        Some(Value::Int(v)) => *v as u8,
        _ => default,
    }
}

/// Reads a bool property of the Canvas (missing -> false).
fn prop_bool(vm: &Vm<'_>, this: u32, name: &str) -> bool {
    matches!(vm.get_property(this, name), Some(Value::Bool(true)))
}

/// RGB(A) of a `Color` value (`b`,`g`,`r`,`a` members, lowercase), falling back to white.
fn color_of(v: &Value) -> [u8; 4] {
    let component = |name: &str, default: u8| match v {
        Value::Struct(fields) => fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .and_then(|(_, fv)| match fv {
                Value::Byte(b) => Some(*b),
                Value::Int(i) => Some(*i as u8),
                _ => None,
            })
            .unwrap_or(default),
        _ => default,
    };
    [
        component("r", 255),
        component("g", 255),
        component("b", 255),
        component("a", 255),
    ]
}

/// `Canvas.DrawColor`.
fn draw_color(vm: &Vm<'_>, this: u32) -> [u8; 4] {
    vm.get_property(this, "DrawColor")
        .map_or([255; 4], color_of)
}

/// `Canvas.Font` object path, when the script set one.
fn canvas_font(vm: &Vm<'_>, this: u32) -> Option<String> {
    vm.get_property(this, "Font")
        .and_then(|v| value_path(vm, v))
}

/// Clip size (`ClipX`, `ClipY`).
fn clip(vm: &Vm<'_>, this: u32) -> [f32; 2] {
    [
        prop_f32(vm, this, "ClipX", 0.0),
        prop_f32(vm, this, "ClipY", 0.0),
    ]
}

/// Pen position including the origin.
fn pen(vm: &Vm<'_>, this: u32) -> (f32, f32) {
    (
        prop_f32(vm, this, "CurX", 0.0) + prop_f32(vm, this, "OrgX", 0.0),
        prop_f32(vm, this, "CurY", 0.0) + prop_f32(vm, this, "OrgY", 0.0),
    )
}

/// Measures `text`, recording a visible note and returning `(0,0)` when the font is not decoded.
fn measure(vm: &mut Vm<'_>, this: u32, text: &str) -> (f32, f32) {
    let font = canvas_font(vm, this);
    match vm.canvas.measure(font.as_deref(), text) {
        Some(size) => size,
        None => {
            vm.note(TraceKind::Note(format!(
                "Canvas: no decoded font to measure {:?} (Canvas.Font = {})",
                text,
                font.as_deref().unwrap_or("None")
            )));
            (0.0, 0.0)
        }
    }
}

// -------------------------------------------------------------------------------------------
// natives

fn set_pos(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    vm.set_property(c.this, "CurX", 0, Value::Float(x));
    vm.set_property(c.this, "CurY", 0, Value::Float(y));
    val(Value::Void)
}

fn set_origin(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    vm.set_property(c.this, "OrgX", 0, Value::Float(x));
    vm.set_property(c.this, "OrgY", 0, Value::Float(y));
    val(Value::Void)
}

fn set_clip(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    vm.set_property(c.this, "ClipX", 0, Value::Float(x));
    vm.set_property(c.this, "ClipY", 0, Value::Float(y));
    val(Value::Void)
}

fn set_draw_color(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let component = |i: usize| -> VmResult<u8> {
        match a.get(i) {
            Some(Value::Byte(v)) => Ok(*v),
            Some(Value::Int(v)) => Ok(*v as u8),
            Some(v) => Err(bad(vm, "byte", v)),
            None => Ok(0),
        }
    };
    let (r, g, b) = (component(0)?, component(1)?, component(2)?);
    // `A` is optional: the VM fills an omitted optional argument with a zero value, so a
    // present zero must be distinguished from "caller omitted it" (e.g. the decoded
    // `XIIIWindow.DrawLabel` calls `SetDrawColor(255,255,255)` and expects opaque white).
    let alpha = if c.omitted(3) { 255 } else { component(3)? };
    let color = Value::Struct(vec![
        ("b".to_owned(), Value::Byte(b)),
        ("g".to_owned(), Value::Byte(g)),
        ("r".to_owned(), Value::Byte(r)),
        ("a".to_owned(), Value::Byte(alpha)),
    ]);
    vm.set_property(c.this, "DrawColor", 0, color);
    val(Value::Void)
}

fn str_len(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = text(vm, a, 0)?;
    let (w, h) = measure(vm, c.this, &s);
    if a.len() > 1 {
        a[1] = Value::Float(w);
    }
    if a.len() > 2 {
        a[2] = Value::Float(h);
    }
    val(Value::Void)
}

fn text_size_native(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    str_len(vm, c, a)
}

/// Records the text command and advances the pen. The advance is a **hypothesis** (UE2 advances
/// `CurX` by the measured width and, on a carriage return, moves to the next line); the HUD
/// mostly calls `SetPos` first, so this only affects consecutive `DrawText` calls.
fn draw_text_inner(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
    clipped: bool,
    justify: Option<u8>,
) -> VmResult<NativeOutcome> {
    let s = text(vm, a, 0)?;
    let cr = flag(a, 1);
    let (w, h) = measure(vm, c.this, &s);
    let (mut x, y) = pen(vm, c.this);
    let color = draw_color(vm, c.this);
    let style = prop_u8(vm, c.this, "Style", 1);
    let center = prop_bool(vm, c.this, "bCenter");
    let justify = justify.unwrap_or(0);
    if center || justify == 1 {
        x -= w * 0.5;
    } else if justify == 2 {
        x -= w;
    }
    vm.canvas.push(DrawCommand::Text {
        text: s,
        x,
        y,
        font: canvas_font(vm, c.this),
        color,
        clip: clip(vm, c.this),
        center,
        style,
        justify,
        clipped,
    });
    let cur_x = x + w;
    vm.set_property(c.this, "CurX", 0, Value::Float(cur_x));
    if cr {
        vm.set_property(c.this, "CurX", 0, Value::Float(0.0));
        vm.set_property(c.this, "CurY", 0, Value::Float(y + h));
    }
    val(Value::Void)
}

fn draw_text(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    draw_text_inner(vm, c, a, false, None)
}

fn draw_text_clipped(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    draw_text_inner(vm, c, a, true, None)
}

fn draw_text_justified(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // `DrawTextJustified(String, Justification, X1, Y1, X2, Y2)`: place the string inside the
    // box with the requested justification; the recorded command carries the resolved pen.
    let s = text(vm, a, 0)?;
    let justify = match a.get(1) {
        Some(Value::Byte(b)) => *b,
        Some(Value::Int(i)) => *i as u8,
        _ => 0,
    };
    let (x1, y1) = (
        float(vm, a, 2).unwrap_or(0.0),
        float(vm, a, 3).unwrap_or(0.0),
    );
    let (x2, _y2) = (float(vm, a, 4).unwrap_or(x1), float(vm, a, 5).unwrap_or(y1));
    let (w, _h) = measure(vm, c.this, &s);
    let x = match justify {
        1 => (x1 + x2) * 0.5 - w * 0.5,
        2 => x2 - w,
        _ => x1,
    };
    let color = draw_color(vm, c.this);
    let style = prop_u8(vm, c.this, "Style", 1);
    vm.canvas.push(DrawCommand::Text {
        text: s,
        x,
        y: y1,
        font: canvas_font(vm, c.this),
        color,
        clip: clip(vm, c.this),
        center: justify == 1,
        style,
        justify,
        clipped: false,
    });
    // Put the pen after the box, as the engine's justified draw does.
    vm.set_property(c.this, "CurX", 0, Value::Float(x2));
    vm.set_property(c.this, "CurY", 0, Value::Float(y1));
    val(Value::Void)
}

#[allow(clippy::too_many_arguments)]
fn push_tile(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    material: Option<String>,
    xl: f32,
    yl: f32,
    u: f32,
    v: f32,
    ul: f32,
    vl: f32,
    clipped: bool,
    justify: u8,
) {
    // Retail execDrawTile (Engine.dll 0x1036237c..0x103623a0) warns and returns
    // for a null material, before drawing or advancing the pen. A missing texture
    // is not an instruction to synthesize a solid white rectangle.
    if material.is_none() {
        vm.note(TraceKind::Note(
            "Canvas.DrawTile: null material; retail draw rejected".into(),
        ));
        return;
    }
    let (mut x, y) = pen(vm, c.this);
    if justify == 1 {
        x -= xl * 0.5;
    } else if justify == 2 {
        x -= xl;
    }
    let color = draw_color(vm, c.this);
    let style = prop_u8(vm, c.this, "Style", 1);
    let clip = clip(vm, c.this);
    vm.canvas.push(DrawCommand::Tile {
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
        style,
        clip,
        justify,
        clipped,
    });
    vm.set_property(c.this, "CurX", 0, Value::Float(x + xl));
}

fn draw_tile(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let material = a.first().and_then(|v| value_path(vm, v));
    let (xl, yl) = (float(vm, a, 1)?, float(vm, a, 2)?);
    let (u, v, ul, vl) = (
        float(vm, a, 3).unwrap_or(0.0),
        float(vm, a, 4).unwrap_or(0.0),
        float(vm, a, 5).unwrap_or(0.0),
        float(vm, a, 6).unwrap_or(0.0),
    );
    push_tile(vm, c, material, xl, yl, u, v, ul, vl, false, 0);
    val(Value::Void)
}

fn draw_tile_clipped(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let material = a.first().and_then(|v| value_path(vm, v));
    let (xl, yl) = (float(vm, a, 1)?, float(vm, a, 2)?);
    let (u, v, ul, vl) = (
        float(vm, a, 3).unwrap_or(0.0),
        float(vm, a, 4).unwrap_or(0.0),
        float(vm, a, 5).unwrap_or(0.0),
        float(vm, a, 6).unwrap_or(0.0),
    );
    push_tile(vm, c, material, xl, yl, u, v, ul, vl, true, 0);
    val(Value::Void)
}

fn draw_tile_stretched(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let material = a.first().and_then(|v| value_path(vm, v));
    let (xl, yl) = (float(vm, a, 1)?, float(vm, a, 2)?);
    // `ul`/`vl` = 0 means "the whole texture" to the host.
    push_tile(vm, c, material, xl, yl, 0.0, 0.0, 0.0, 0.0, false, 0);
    val(Value::Void)
}

fn draw_tile_scaled(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let material = a.first().and_then(|v| value_path(vm, v));
    let (xs, ys) = (float(vm, a, 1)?, float(vm, a, 2)?);
    push_tile(vm, c, material, xs, ys, 0.0, 0.0, 0.0, 0.0, false, 0);
    val(Value::Void)
}

fn draw_tile_justified(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let material = a.first().and_then(|v| value_path(vm, v));
    let justify = match a.get(1) {
        Some(Value::Byte(b)) => *b,
        Some(Value::Int(i)) => *i as u8,
        _ => 0,
    };
    let (xl, yl) = (float(vm, a, 2)?, float(vm, a, 3)?);
    push_tile(vm, c, material, xl, yl, 0.0, 0.0, 0.0, 0.0, false, justify);
    val(Value::Void)
}

fn draw_line(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x1, y1, x2, y2) = (
        float(vm, a, 0)?,
        float(vm, a, 1)?,
        float(vm, a, 2)?,
        float(vm, a, 3)?,
    );
    let color = a.get(4).map_or_else(|| draw_color(vm, c.this), color_of);
    let clip = clip(vm, c.this);
    vm.canvas.push(DrawCommand::Line {
        x1,
        y1,
        x2,
        y2,
        color,
        clip,
    });
    val(Value::Void)
}

fn wrap_string(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = text(vm, a, 0)?;
    let dx = float(vm, a, 2).unwrap_or(0.0);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        let (w, _) = measure(vm, c.this, &candidate);
        if !line.is_empty() && dx > 0.0 && w > dx {
            lines.push(std::mem::take(&mut line));
            line = word.to_owned();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    if a.len() > 1 {
        a[1] = Value::Array(lines.into_iter().map(Value::Str).collect());
    }
    val(Value::Void)
}

fn draw_msgbox_background(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    // Partial: the decoded boxes are not reproduced; a clip-sized rectangle is recorded.
    let (w, h) = (
        float(vm, a, 4).unwrap_or(0.0),
        float(vm, a, 5).unwrap_or(0.0),
    );
    let (x, y) = (
        float(vm, a, 1).unwrap_or(0.0),
        float(vm, a, 2).unwrap_or(0.0),
    );
    let color = draw_color(vm, c.this);
    let clip = clip(vm, c.this);
    vm.canvas.push(DrawCommand::Rect {
        x,
        y,
        xl: w.max(1.0),
        yl: h.max(1.0),
        color,
        clip,
    });
    val(Value::Void)
}

fn get_screen_height(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Int(prop_f32(vm, c.this, "ClipY", 0.0) as i32))
}

fn draw_actor(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let actor = a.first().and_then(|v| value_path(vm, v));
    vm.note(TraceKind::Note(format!(
        "Canvas.DrawActor({}): actor-to-canvas rendering is not implemented (Partial)",
        actor.as_deref().unwrap_or("None")
    )));
    val(Value::Void)
}

fn draw_portal(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y, w, h) = (
        float(vm, a, 0).unwrap_or(0.0),
        float(vm, a, 1).unwrap_or(0.0),
        float(vm, a, 2).unwrap_or(0.0),
        float(vm, a, 3).unwrap_or(0.0),
    );
    vm.note(TraceKind::Note(
        "Canvas.DrawPortal: portal camera is recorded as a rectangle (Partial)".into(),
    ));
    let color = draw_color(vm, c.this);
    let clip = clip(vm, c.this);
    vm.canvas.push(DrawCommand::Rect {
        x,
        y,
        xl: w.max(1.0),
        yl: h.max(1.0),
        color,
        clip,
    });
    val(Value::Void)
}

fn draw_cine_frame(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let on = flag(a, 0);
    vm.note(TraceKind::Note(format!(
        "Canvas.DrawCineFrame({on}): the letterbox bars are not drawn (Partial)"
    )));
    val(Value::Void)
}

fn deal_with_reset(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "Canvas.DealWithResetEvent: no device-reset handling in the VM (Partial)".into(),
    ));
    val(Value::Void)
}

// --- `Engine.HUD` base-class draw natives called from the HUD PostRender chain. They are the
// HUD's own (non-Canvas) native entry points; without their PNG/renderer semantics the call is
// recorded as a visible partial note so `PostRender` completes.

/// `Engine.HUD.DrawPlayerInfo(object<Canvas> C)` (native, no decoded bytecode).
///
/// **Partial reimplementation** (documented, not the retail art): place the pen at the HUD's
/// `XP`/`YP` (set by `XIIIBaseHud.XIIIDrawPlayerInfo` before the call) and record a text command
/// with the pawn's `Health`. The retail native's panels/icons are not reproduced; this keeps the
/// `PostRender` chain producing a visible player-info element instead of silently drawing
/// nothing.
fn hud_draw_player_info(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let canvas = match a.first() {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => *i,
        _ => {
            vm.note(TraceKind::Note(
                "HUD.DrawPlayerInfo: null Canvas (Partial)".into(),
            ));
            return val(Value::Void);
        }
    };
    let x = vm.f32_prop(c.this, "XP");
    let y = vm.f32_prop(c.this, "YP");
    let health =
        vm.obj_prop(c.this, "PawnOwner")
            .and_then(|p| match vm.get_property(p, "Health") {
                Some(Value::Int(h)) => Some(*h),
                Some(Value::Float(f)) => Some(*f as i32),
                _ => None,
            });
    let name = vm
        .obj_prop(c.this, "PlayerOwner")
        .and_then(|p| match vm.get_property(p, "PlayerReplicationInfo") {
            Some(Value::Object(Some(ObjRef::Instance(pri)))) => Some(*pri),
            _ => None,
        })
        .and_then(|pri| match vm.get_property(pri, "PlayerName") {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        });
    let text = match (name, health) {
        (Some(n), Some(h)) => format!("{n}  {h}"),
        (None, Some(h)) => format!("HEALTH {h}"),
        _ => "XIII".to_owned(),
    };
    vm.set_property(canvas, "CurX", 0, Value::Float(x));
    vm.set_property(canvas, "CurY", 0, Value::Float(y));
    vm.canvas.push(DrawCommand::Text {
        text,
        x,
        y,
        font: canvas_font(vm, canvas),
        color: draw_color(vm, canvas),
        clip: clip(vm, canvas),
        center: false,
        style: prop_u8(vm, canvas, "Style", 1),
        justify: 0,
        clipped: false,
    });
    vm.note(TraceKind::Note(
        "HUD.DrawPlayerInfo: partial reimplementation (name + Health; retail panels not reproduced)"
            .into(),
    ));
    val(Value::Void)
}

fn hud_draw_weapons_list(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "HUD.DrawWeaponsList: base HUD weapons native not implemented (Partial)".into(),
    ));
    val(Value::Void)
}

fn hud_draw_ammo(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "HUD.DrawAmmo: base HUD ammo native not implemented (Partial)".into(),
    ));
    val(Value::Void)
}

fn hud_draw_std_background(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "HUD.DrawStdBackground: base HUD background native not implemented (Partial)".into(),
    ));
    val(Value::Void)
}

fn hud_draw_3d_line(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "HUD.Draw3DLine: 3D line pass not implemented (Partial)".into(),
    ));
    val(Value::Void)
}

// -------------------------------------------------------------------------------------------
// registry

fn def(
    path: &'static str,
    signature: &'static str,
    evidence: &'static str,
    f: crate::registry::NativeFn,
) -> NativeDef {
    NativeDef {
        path,
        signature,
        evidence,
        status: NativeStatus::Implemented,
        short_circuit: None,
        f,
    }
}

fn partial(
    status: &'static str,
    path: &'static str,
    signature: &'static str,
    evidence: &'static str,
    f: crate::registry::NativeFn,
) -> NativeDef {
    NativeDef {
        status: NativeStatus::Partial(status),
        ..def(path, signature, evidence, f)
    }
}

/// The `Engine.Canvas` natives implemented by this module. `registry::builtin_defs` extends the
/// built-in table with these; `Canvas.MakeColor` lives in `registry.rs` and is not repeated.
#[allow(clippy::vec_init_then_push)]
pub fn canvas_defs() -> Vec<NativeDef> {
    let mut v: Vec<NativeDef> = Vec::new();
    v.push(def(
        "Engine.Canvas.SetPos",
        "native(269) final static function SetPos(float X, float Y)",
        "engine.u Canvas.SetPos decoded; UE2 UCanvas::SetPos sets CurX/CurY",
        set_pos,
    ));
    v.push(def(
        "Engine.Canvas.SetOrigin",
        "native(270) final static function SetOrigin(float X, float Y)",
        "engine.u Canvas.SetOrigin decoded; UE2 UCanvas::SetOrigin sets OrgX/OrgY",
        set_origin,
    ));
    v.push(def(
        "Engine.Canvas.SetClip",
        "native(271) final static function SetClip(float X, float Y)",
        "engine.u Canvas.SetClip decoded; UE2 UCanvas::SetClip sets ClipX/ClipY",
        set_clip,
    ));
    v.push(def(
        "Engine.Canvas.SetDrawColor",
        "native(273) final static function SetDrawColor(byte R, byte G, byte B, optional byte A)",
        "engine.u Canvas.SetDrawColor decoded; UE2 UCanvas::SetDrawColor stores the Color struct",
        set_draw_color,
    ));
    v.push(def(
        "Engine.Canvas.StrLen",
        "native(464) final static function StrLen(string String, out float XL, out float YL)",
        "engine.u Canvas.StrLen decoded; measures with Canvas.Font metrics (host provider)",
        str_len,
    ));
    v.push(def(
        "Engine.Canvas.TextSize",
        "native(470) final static function TextSize(string String, out float XL, out float YL)",
        "engine.u Canvas.TextSize decoded; same measurement as StrLen",
        text_size_native,
    ));
    v.push(def(
        "Engine.Canvas.DrawText",
        "native(465) final static function DrawText(string Text, bool CR)",
        "engine.u Canvas.DrawText decoded; records a Text command at CurX/CurY",
        draw_text,
    ));
    v.push(def(
        "Engine.Canvas.DrawTextClipped",
        "native(469) final static function DrawTextClipped(string Text, bool bCheckHotKey)",
        "engine.u Canvas.DrawTextClipped decoded; records a clipped Text command",
        draw_text_clipped,
    ));
    v.push(def(
        "Engine.Canvas.DrawTile",
        "native(466) final static function DrawTile(object<Material> Tex, float XL, float YL, float U, float V, float UL, float VL)",
        "engine.u Canvas.DrawTile decoded; Engine.dll execDrawTile 0x1036237c rejects null material before draw/pen advance; records decoded material tiles",
        draw_tile,
    ));
    v.push(def(
        "Engine.Canvas.DrawTileClipped",
        "native(468) final static function DrawTileClipped(object<Material> Tex, float XL, float YL, float U, float V, float UL, float VL)",
        "engine.u Canvas.DrawTileClipped decoded; records a clipped Tile",
        draw_tile_clipped,
    ));
    v.push(def(
        "Engine.Canvas.DrawTileStretched",
        "native(241) final static function DrawTileStretched(object<Material> Mat, float XL, float YL)",
        "engine.u Canvas.DrawTileStretched decoded; records a Tile with the whole texture (UL/VL=0)",
        draw_tile_stretched,
    ));
    v.push(def(
        "Engine.Canvas.DrawTileScaled",
        "native(253) final static function DrawTileScaled(object<Material> Mat, float XScale, float YScale)",
        "engine.u Canvas.DrawTileScaled decoded; records a Tile sized by the scales",
        draw_tile_scaled,
    ));
    v.push(def(
        "Engine.Canvas.DrawTileJustified",
        "native(257) final static function DrawTileJustified(object<Material> Mat, byte Justification, float XL, float YL)",
        "engine.u Canvas.DrawTileJustified decoded; records a Tile with the justification",
        draw_tile_justified,
    ));
    v.push(def(
        "Engine.Canvas.DrawTextJustified",
        "native(268) final static function DrawTextJustified(string String, byte Justification, float X1, float Y1, float X2, float Y2)",
        "engine.u Canvas.DrawTextJustified decoded; records a Text placed in the box",
        draw_text_justified,
    ));
    v.push(def(
        "Engine.Canvas.DrawLine",
        "native(240) final static function DrawLine(float X1, float Y1, float X2, float Y2, struct<Color> LineColor)",
        "engine.u Canvas.DrawLine decoded; records a Line (falls back to DrawColor when unreadable)",
        draw_line,
    ));
    v.push(def(
        "Engine.Canvas.WrapStringToArray",
        "native(239) final static function WrapStringToArray(string Text, out array<string> OutArray, float dx, string EOL)",
        "engine.u Canvas.WrapStringToArray decoded; word-wrap by measured width (Partial: no hot-key markup)",
        wrap_string,
    ));
    v.push(def(
        "Engine.Canvas.GetScreenHeight",
        "native(0) final static function int GetScreenHeight()",
        "engine.u Canvas.GetScreenHeight decoded; returns ClipY",
        get_screen_height,
    ));
    v.push(partial(
        "records a clip-sized rectangle; the decoded msgbox geometry is not reproduced",
        "Engine.Canvas.DrawMsgboxBackground",
        "native(0) final static function DrawMsgboxBackground(bool bOnlyCenter, float OrgX, float OrgY, float LineWidth, float LineHeight, float Width, float Height)",
        "engine.u Canvas.DrawMsgboxBackground decoded; no msgbox model in the VM",
        draw_msgbox_background,
    ));
    v.push(partial(
        "the actor-to-canvas projection is not implemented; a trace note is recorded",
        "Engine.Canvas.DrawActor",
        "native(467) final static function DrawActor(object<Actor> A, bool Wireframe, bool ClearZ, float DisplayFOV)",
        "engine.u Canvas.DrawActor decoded; needs a canvas camera/mesh pass",
        draw_actor,
    ));
    v.push(partial(
        "uses the active host camera's projection (including its near-plane/depth convention); explicit camera arguments fail visibly rather than bypassing retail PlayerCalcView",
        "Engine.Interaction.WorldToScreen",
        "native(0) function vector WorldToScreen(vector Location, optional vector CameraLocation, optional rotator CameraRotation)",
        "Engine.dll execWorldToScreen 0x103858d0 calls PlayerCalcView, FCameraSceneNode, FSceneNode::Project 0x103c99d0 (signed homogeneous divide), then canvas transform 0x10385670; XIIIPlayerInteraction.DrawInteractions 0x0B71 uses returned X/Y for focus marker",
        world_to_screen,
    ));
    v.push(partial(
        "the portal camera pass is not rendered; a rectangle is recorded and a note added",
        "Engine.Canvas.DrawPortal",
        "native(480) final static function DrawPortal(int X, int Y, int width, int Height, object<Actor> CamActor, struct<Vector> CamLocation, struct<Rotator> CamRotation, int FOV, bool ClearZ)",
        "engine.u Canvas.DrawPortal decoded; needs a second camera pass",
        draw_portal,
    ));
    v.push(partial(
        "records a trace note; the letterbox bars are not drawn",
        "Engine.Canvas.DrawCineFrame",
        "native(238) final static function DrawCineFrame(bool bOnOff)",
        "engine.u Canvas.DrawCineFrame decoded; cinema bars are presentation, not recorded",
        draw_cine_frame,
    ));
    v.push(partial(
        "records a trace note; no device reset exists in the VM",
        "Engine.Canvas.DealWithResetEvent",
        "native(285) final static function DealWithResetEvent()",
        "engine.u Canvas.DealWithResetEvent decoded; no device-reset path",
        deal_with_reset,
    ));
    // `Engine.HUD` base-class draw natives (called by `XIIIBaseHud.PostRender` via
    // `XIIIDrawPlayerInfo` and friends). Implemented as visible partial notes so the script HUD
    // path completes and the report can list what is missing.
    v.push(partial(
        "partial reimplementation: records a Canvas text command with the player name/pawn Health \
         at the HUD XP/YP; the retail native panels/icons are not reproduced",
        "Engine.HUD.DrawPlayerInfo",
        "native(0) final native function DrawPlayerInfo(object<Canvas> C)",
        "engine.u HUD.DrawPlayerInfo decoded (Canvas, void); called by XIIIBaseHud.XIIIDrawPlayerInfo",
        hud_draw_player_info,
    ));
    v.push(partial(
        "the native weapons panel is not reimplemented; a trace note is recorded",
        "Engine.HUD.DrawWeaponsList",
        "native(0) final native function DrawWeaponsList(object<Canvas> C)",
        "engine.u HUD.DrawWeaponsList decoded (Canvas, void)",
        hud_draw_weapons_list,
    ));
    v.push(partial(
        "the native ammo panel is not reimplemented; a trace note is recorded",
        "Engine.HUD.DrawAmmo",
        "native(0) final native function DrawAmmo(object<Canvas> C, string ItemText)",
        "engine.u HUD.DrawAmmo decoded (Canvas, ItemText, void)",
        hud_draw_ammo,
    ));
    v.push(partial(
        "the native background panel is not reimplemented; a trace note is recorded",
        "Engine.HUD.DrawStdBackground",
        "native(0) final native function DrawStdBackground(object<Canvas> C, float Height, float CenterWidth)",
        "engine.u HUD.DrawStdBackground decoded (Canvas, Height, CenterWidth, void)",
        hud_draw_std_background,
    ));
    v.push(partial(
        "the 3D debug-line pass is not reimplemented; a trace note is recorded",
        "Engine.HUD.Draw3DLine",
        "native(0) final native function Draw3DLine()",
        "engine.u HUD.Draw3DLine decoded (void)",
        hud_draw_3d_line,
    ));
    v
}

// -------------------------------------------------------------------------------------------
// item16: front-end menu path natives
//
// The decoded front end runs the menu classes (`XIDInterf.XIIIRootWindow`,
// `XIDInterf.XIIIMenu`, `GUI.GUIController`) through the VM. The menu scripts reach four
// engine services the headless VM must supply:
//
//   * `Engine.VideoPlayer.*` (native 476/482..484): the new-game entry starts the `cine00`
//     intro video; `XIIIMenu.PlayingVideo.Tick` polls `VideoPlayer.GetStatus` (native 476,
//     disassembled against the same-numbered `ScriptedTexture.TextSize`; the VM resolves it
//     by argument count), and `XIIIMenu.EndOfVideo` then calls `ClientTravel`.
//   * `Engine.PlayerController.ClientTravel` (native 0, by name): the game's own map-load
//     request. The VM has no world to travel to, so the native records a structured trace
//     note; the host (`xiii-app --menu`) reads it and performs the travel step.
//   * `Engine.Actor.PlayMenu` / `StopAllSounds` / `PauseAllSounds` / `ResumeAllSounds`
//     (natives 351/342/340/338): menu click/rollover sounds. The VM has no audio device;
//     these are accepted and labelled Partial, not silently completed.
//
// The natives live in this block (`canvas.rs`) because the menu draws through the recorded
// `Engine.Canvas` command list this module already owns, and they are registered by
// `Registry::builtin` through `registry::builtin_defs`.

/// Marker prefix of the structured `ClientTravel` trace note. Keep in sync with
/// [`parse_travel_note`].
pub const TRAVEL_NOTE_PREFIX: &str = "item16 ClientTravel ";

/// One map-load request decoded from `PlayerController.ClientTravel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelRequest {
    /// The raw URL argument (may carry `?options`).
    pub url: String,
    /// The map name (the part of `url` before `?`), e.g. `Plage00`.
    pub map: String,
    /// The decoded `ETravelType` byte.
    pub travel_type: u8,
    /// The decoded `bItems` flag.
    pub items: bool,
}

/// Builds the structured trace note recorded by `ClientTravel`.
pub fn format_travel_note(url: &str, travel_type: u8, items: bool) -> String {
    format!(
        "{TRAVEL_NOTE_PREFIX}url={url} travel={travel_type} items={}",
        u8::from(items)
    )
}

/// Parses a [`format_travel_note`] note back into a [`TravelRequest`]. Returns `None` for an
/// unrelated note or a malformed one (never a guessed URL).
pub fn parse_travel_note(note: &str) -> Option<TravelRequest> {
    let rest = note.strip_prefix(TRAVEL_NOTE_PREFIX)?;
    let mut url = None;
    let mut travel_type = 0u8;
    let mut items = false;
    for field in rest.split(' ') {
        if let Some(v) = field.strip_prefix("url=") {
            url = Some(v.to_owned());
        } else if let Some(v) = field.strip_prefix("travel=") {
            travel_type = v.parse().ok()?;
        } else if let Some(v) = field.strip_prefix("items=") {
            items = v != "0";
        }
    }
    let url = url?;
    let map = url.split('?').next().unwrap_or(&url).to_ascii_lowercase();
    Some(TravelRequest {
        url,
        map,
        travel_type,
        items,
    })
}

/// `Engine.VideoPlayer.Open(string Filename) -> bool` (native 484).
///
/// Records the clip in the VM (`Vm::video_open`). With the host decoder installed
/// (`Vm::set_video_host`) the host decodes and will play the clip and `GetStatus` follows the
/// host playback; otherwise a host-registered Bink-header duration lets `GetStatus` time the
/// clip (labelled fallback), else it reports finished immediately (the labelled Partial). The
/// boolean is `true` whenever the clip is timed, so the decoded `XIIIMenu`/`XIII` code proceeds
/// either way.
fn video_player_open(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = text(vm, a, 0).unwrap_or_default();
    let timed = vm.video_open(&name);
    let note = match (vm.video_timing(), vm.video_duration()) {
        (Some(VideoTiming::Host), Some(d)) => format!(
            "item21 VideoPlayer.Open({name:?}): host xiii-video decoder plays this clip \
             ({d:.3}s); GetStatus follows the host playback"
        ),
        (Some(VideoTiming::Duration), Some(d)) if vm.has_video_host() => format!(
            "item21 VideoPlayer.Open({name:?}): host could not decode the file; timed from the \
             Bink-header duration {d:.3}s (labelled fallback)"
        ),
        (Some(VideoTiming::Duration), Some(d)) => format!(
            "item18 VideoPlayer.Open({name:?}) accepted (no Bink decoder; not played; \
             duration known ({d:.3}s), GetStatus times it)"
        ),
        _ if vm.has_video_host() => format!(
            "item21 VideoPlayer.Open({name:?}): not decoded and no duration registered; \
             GetStatus reports finished"
        ),
        _ => format!(
            "item18 VideoPlayer.Open({name:?}) accepted (no Bink decoder; not played; {})",
            if timed {
                "duration known, GetStatus times it"
            } else {
                "no duration registered, GetStatus reports finished"
            }
        ),
    };
    vm.note(TraceKind::Note(note));
    val(Value::Bool(timed))
}

/// `Engine.VideoPlayer.Play()` (native 483): starts the clip (`Vm::video_play`). With the host
/// decoder installed the host playback starts; otherwise only the clip clock starts.
fn video_player_play(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let host = vm.has_video_host() && vm.video_timing() == Some(VideoTiming::Host);
    vm.video_play();
    vm.note(TraceKind::Note(if host {
        "item21 VideoPlayer.Play(): host playback started".into()
    } else {
        "item18 VideoPlayer.Play() accepted (no Bink decoder; not played)".into()
    }));
    val(Value::Void)
}

/// `Engine.VideoPlayer.Stop()` (native 482): clears the clip (`Vm::video_stop`); with the host
/// decoder installed the host playback stops and the fullscreen overlay is torn down.
fn video_player_stop(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let host = vm.has_video_host();
    vm.video_stop();
    vm.note(TraceKind::Note(if host {
        "item21 VideoPlayer.Stop(): host playback stopped".into()
    } else {
        "item18 VideoPlayer.Stop() accepted (no Bink decoder)".into()
    }));
    val(Value::Void)
}

/// `Engine.VideoPlayer.GetStatus() -> int` (native 476; the same number is
/// `ScriptedTexture.TextSize(string, out, out, Font)`, which the VM's argument-count
/// resolution keeps distinct). `1` while a host-timed clip is playing, `0` when it finished (or
/// has no known duration), so `PlayingVideo.PlayerTick` runs `ServerTravel` at the real end.
fn video_player_get_status(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Int(vm.video_status()))
}

/// `Engine.Actor.StopAllSounds()` (native 342). No audio device; accepted.
fn actor_stop_all_sounds(
    _vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// `Engine.Actor.PauseAllSounds()` (native 340). No audio device; accepted.
fn actor_pause_all_sounds(
    _vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// `Engine.Actor.ResumeAllSounds()` (native 338). No audio device; accepted.
fn actor_resume_all_sounds(
    _vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// The menu-path natives defined by this block. `registry::builtin_defs` extends the
/// built-in table with these.
#[allow(clippy::vec_init_then_push)]
pub fn menu_defs() -> Vec<NativeDef> {
    let mut v: Vec<NativeDef> = Vec::new();
    v.push(partial(
        "with the host decoder installed the host decodes and plays the clip and GetStatus follows the host playback (item21); otherwise the clip is accepted but not displayed and a host-registered Bink-header duration lets GetStatus time it (item18), else it reports finished",
        "VideoPlayer.Open",
        "native(484) final native static function bool Open(string Filename)",
        "engine.u VideoPlayer.Open decoded; XIIIMenu.InternalOnClick opens sVideo (default cine00); XIIIPlayerController.GameEndedSuccess.Timer opens MapInfo.EndMapVideo (cine01)",
        video_player_open,
    ));
    v.push(partial(
        "with the host decoder installed the host playback starts (item21); otherwise the clip is accepted but not displayed and GetStatus times the registered duration (item18)",
        "VideoPlayer.Play",
        "native(483) final native static function Play()",
        "engine.u VideoPlayer.Play decoded; XIIIMenu.InternalOnClick and XIIIPlayerController.PlayingVideo.BeginState call it",
        video_player_play,
    ));
    v.push(partial(
        "with the host decoder installed the host playback stops and the overlay is torn down (item21); otherwise the clip is cleared (item18)",
        "VideoPlayer.Stop",
        "native(482) final native static function Stop()",
        "engine.u VideoPlayer.Stop decoded; XIIIMenu.InternalOnKeyEvent (Enter/Escape while a video plays) stops the video",
        video_player_stop,
    ));
    v.push(partial(
        "with the host decoder installed completion is the host playback's actual end and decode failures report the game's error status 2 (item21); otherwise GetStatus reports the real Bink duration when the host registered it, else 0 (item18)",
        "VideoPlayer.GetStatus",
        "native(476) final native static function int GetStatus()",
        "engine.u VideoPlayer.GetStatus decoded; PlayingVideo.PlayerTick (xiii) and XIIIMenu.PlayingVideo.Tick poll it (1 playing, 0 end, 2 error); index 476 also names ScriptedTexture.TextSize, separated by argument count",
        video_player_get_status,
    ));
    // `Actor.PlayMenu` (native 351) is already registered by the cartoon-panel block
    // (`crates/xiii-script/src/cartoon.rs`), which emits a `PlaySound` presentation event; the
    // menu reuses it rather than overriding it here.
    v.push(partial(
        "no audio device: the call is accepted and discarded (the VM has no mixer)",
        "Actor.StopAllSounds",
        "native(342) final native static function StopAllSounds()",
        "engine.u Actor.StopAllSounds decoded; XIIIMenu.InternalOnClick calls it on new game",
        actor_stop_all_sounds,
    ));
    v.push(partial(
        "no audio device: the call is accepted and discarded (the VM has no mixer)",
        "Actor.PauseAllSounds",
        "native(340) final native static function PauseAllSounds()",
        "engine.u Actor.PauseAllSounds decoded; XIIIRootWindow.UWindows.BeginState calls it",
        actor_pause_all_sounds,
    ));
    v.push(partial(
        "no audio device: the call is accepted and discarded (the VM has no mixer)",
        "Actor.ResumeAllSounds",
        "native(338) final native static function ResumeAllSounds()",
        "engine.u Actor.ResumeAllSounds decoded; XIIIRootWindow.UWindows.EndState calls it",
        actor_resume_all_sounds,
    ));
    // `PlayerController.ClientTravel` is registered once, in `registry::builtin_defs` (the item15
    // travel block): that single implementation records both this block's trace note
    // (`format_travel_note`) and the item15 host `TravelRequest`, so the menu host and `--play`
    // both work from one registration.
    v
}

// -------------------------------------------------------------------------------------------
// item16b: GUI-frame natives needed by the decoded front-end
//
// The item16 host called the page's `Created` directly, bypassing
// `gui.GUIController.InternalMenuInit` / `XIIIGUIBaseButton.InitComponent` because the VM did
// not interpret delegate opcodes. This block supplies the two controller services those
// decoded functions use so the real init path can run (item16b):
//
//   * `GUI.GUIComponent.InitComponent` (block `gui-all.txt`, `gui.u` @15432) does
//     `self.Style = self.Controller.GetStyle(self.StyleName)`. No style table is loaded, so
//     `GetStyle` returns `None` (Partial, labelled): `XIIIWindow`/`XIIIGUIBaseButton` carry
//     their own `WhiteColor`/`BlackColor`/`HighlightColor` defaults, which is what the menu
//     drawing reads.
//   * `GUIController.OpenMenuWithClass` (gui.u @36514) calls the final native
//     `InitStateFrame(NewMenu)` before `InternalMenuInit`. The host bridge does not use the
//     page stack, but the call is accepted and recorded so the decoded flow is not blocked.
//
// Both are registered from `registry::builtin_defs` through the same menu block hook.

/// `GUI.GUIController.GetStyle(string StyleName) -> object<GUIStyles>`.
///
/// No style table is loaded; returns `None` and records a Partial note the first time. The
/// menu classes do not rely on the style object for their colours.
fn gui_controller_get_style(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let name = a.first().and_then(|v| match v {
        Value::Str(s) | Value::Name(s) => Some(s.clone()),
        _ => None,
    });
    vm.note(TraceKind::Note(format!(
        "GUI.GUIController.GetStyle({}) -> None (no style table loaded; Partial, item16b)",
        name.as_deref().unwrap_or("None")
    )));
    val(Value::Object(None))
}

/// `GUI.GUIController.InitStateFrame(object<GUIPage> Page)` (final native).
fn gui_controller_init_state_frame(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "GUI.GUIController.InitStateFrame accepted (host bridge does not run the page stack; Partial, item16b)"
            .into(),
    ));
    val(Value::Void)
}

/// The item16b GUI-frame natives. `registry::builtin_defs` extends the built-in table with
/// these.
#[allow(clippy::vec_init_then_push)]
pub fn item16b_defs() -> Vec<NativeDef> {
    let mut v: Vec<NativeDef> = Vec::new();
    v.push(partial(
        "no GUI style table loaded: returns None and records a Partial note; menu colours come from the class defaults",
        "GUIController.GetStyle",
        "native(0) event function object<GUIStyles> GetStyle(string StyleName)",
        "gui.u GUIComponent.InitComponent 0x0016 reads Controller.GetStyle(StyleName); gui.GUIController.GetStyle decoded",
        gui_controller_get_style,
    ));
    v.push(partial(
        "host bridge does not run the GUIController page stack; the call is accepted and recorded",
        "GUIController.InitStateFrame",
        "native(0) final static function InitStateFrame(object<GUIPage> Page)",
        "gui.u GUIController.OpenMenuWithClass 0x000F calls InitStateFrame(NewMenu)",
        gui_controller_init_state_frame,
    ));
    v
}

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn travel_note_round_trips_and_extracts_the_map() {
        let note = format_travel_note("Plage00?Difficulty=1", 0, false);
        let r = parse_travel_note(&note).expect("parses");
        assert_eq!(r.url, "Plage00?Difficulty=1");
        assert_eq!(r.map, "plage00");
        assert_eq!(r.travel_type, 0);
        assert!(!r.items);
    }

    #[test]
    fn travel_note_rejects_unrelated_and_malformed_input() {
        assert!(parse_travel_note("some other note").is_none());
        assert!(parse_travel_note("item16 ClientTravel travel=0 items=0").is_none());
        assert!(parse_travel_note("item16 ClientTravel url=Plage00 travel=x items=0").is_none());
    }

    #[test]
    fn travel_note_preserves_items_and_travel_type() {
        let r = parse_travel_note(&format_travel_note("Banque01", 3, true)).unwrap();
        assert_eq!(
            (r.map.as_str(), r.travel_type, r.items),
            ("banque01", 3, true)
        );
    }
}

// -------------------------------------------------------------------------------------------
// item16c: filesystem-free menu configuration and native engine services.

/// Native menu settings. The application loads installation defaults plus a user INI, and
/// flushes `save_requested` to the user directory. No native opens a file or executes a shell.
#[derive(Debug, Clone, Default)]
pub struct MenuSettings {
    /// Case-insensitive (section, key) settings, including key bindings.
    pub values: std::collections::BTreeMap<(String, String), String>,
    /// SaveConfig requests a host flush; cleared only after a successful write.
    pub save_requested: bool,
    /// Live audio preview, in the retail master-volume units (dB).
    pub master_db: f32,
    /// Retail music selector: 0 off, 1 light, 2 normal.
    pub music: i32,
    /// Host-supported video modes, supplied before the page opens.
    pub resolutions: Vec<(u32, u32)>,
    /// StopMusic requests disposal of the current music voice.
    pub stop_music: bool,
}

impl MenuSettings {
    /// Case-insensitive lookup; Input is the engine console alias for Engine.Input.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.values
            .get(&(config_section(section), key.to_ascii_lowercase()))
            .map(String::as_str)
    }

    /// Sets one value without saving (the game's SaveConfig determines persistence).
    pub fn set(&mut self, section: &str, key: &str, value: String) {
        self.values
            .insert((config_section(section), key.to_ascii_lowercase()), value);
    }
}

fn config_section(section: &str) -> String {
    let lower = section.to_ascii_lowercase();
    match lower.as_str() {
        "input" => "engine.input".into(),
        "xiiimenuvideoclientwindow"
        | "xiiimenuaudioclientwindow"
        | "xiiimenucontrolswindow"
        | "xiiimenuadvancedcontrolswindow" => format!("xidinterf.{lower}"),
        "xiiiplayercontroller" => format!("xiii.{lower}"),
        other => other.into(),
    }
}

fn menu_error(vm: &Vm<'_>, message: impl Into<String>) -> crate::vm::VmError {
    vm.err(VmErrorKind::Other(message.into()))
}

/// Parses scalar property text against its existing reflected type. Rejects non-finite floats,
/// overflow and compound types rather than silently installing a wrongly typed value.
pub fn parse_property_text(old: &Value, text: &str) -> Result<Value, String> {
    let invalid = || format!("invalid {} property text {text:?}", old.type_name());
    Ok(match old {
        Value::Bool(_) => match text.to_ascii_lowercase().as_str() {
            "true" | "1" => Value::Bool(true),
            "false" | "0" => Value::Bool(false),
            _ => return Err(invalid()),
        },
        Value::Byte(_) => {
            let parsed = text
                .parse::<u8>()
                .ok()
                .or_else(|| match text.to_ascii_lowercase().as_str() {
                    "ct_strafelooksameaxis" => Some(0),
                    "ct_strafelooknotsameaxis" => Some(1),
                    _ => None,
                })
                .ok_or_else(invalid)?;
            Value::Byte(parsed)
        }
        Value::Int(_) => Value::Int(text.parse().map_err(|_| invalid())?),
        Value::Float(_) => {
            let v: f32 = text.parse().map_err(|_| invalid())?;
            if !v.is_finite() {
                return Err(invalid());
            }
            Value::Float(v)
        }
        Value::Str(_) => Value::Str(text.to_owned()),
        Value::Name(_) => Value::Name(text.to_owned()),
        _ => return Err(invalid()),
    })
}

/// Applies INI values for the spawned object's class before its script `Created` runs.
/// The reflected slot remains the authority for the property's type; unsupported compound
/// values are rejected instead of replaced with a guessed representation.
pub fn apply_menu_config(vm: &mut Vm<'_>, object: crate::value::ObjectId) -> VmResult<()> {
    let class = vm.set().path(vm.objects[object as usize].class);
    let configured = vm
        .canvas
        .menu
        .as_ref()
        .ok_or_else(|| menu_error(vm, "menu config provider is unavailable"))?;
    let entries: Vec<(String, String)> = configured
        .values
        .iter()
        .filter(|((section, _), _)| section.eq_ignore_ascii_case(&class))
        .map(|((_, key), value)| (key.clone(), value.clone()))
        .collect();
    for (key, text) in entries {
        let Some(old) = vm.get_property(object, &key) else {
            return Err(menu_error(
                vm,
                format!("config {class}.{key} has no reflected property"),
            ));
        };
        let value = parse_property_text(old, &text).map_err(|e| menu_error(vm, e))?;
        if !vm.set_property(object, &key, 0, value) {
            return Err(menu_error(
                vm,
                format!("could not assign config {class}.{key}"),
            ));
        }
    }
    Ok(())
}

fn get_property_text(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = text(vm, a, 0)?;
    let value = vm
        .get_property(c.this, &name)
        .ok_or_else(|| menu_error(vm, format!("GetPropertyText: unknown property {name}")))?;
    val(Value::Str(vm.value_text(value)))
}

fn set_property_text(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = text(vm, a, 0)?;
    let input = text(vm, a, 1)?;
    let old = vm
        .get_property(c.this, &name)
        .ok_or_else(|| menu_error(vm, format!("SetPropertyText: unknown property {name}")))?;
    let value = parse_property_text(old, &input).map_err(|e| menu_error(vm, e))?;
    vm.set_property(c.this, &name, 0, value);
    val(Value::Void)
}

fn save_config(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    // The retail video page stores these three User* properties, then SaveConfig. Other
    // options use ConsoleCommand SET, which already updates the settings dictionary.
    let class = vm.set().path(vm.objects[c.this as usize].class);
    let mut entries = Vec::new();
    for key in [
        "UserBrightness",
        "UserGamma",
        "UserContrast",
        "DecalX",
        "DecalY",
        "bUseRumble",
        "iAutoAimMode",
        "bInverseLook",
        "fLookSpeed",
        "UserPadConfig",
        "ConfigType",
    ] {
        if let Some(value) = vm.get_property(c.this, key) {
            entries.push((key, vm.value_text(value)));
        }
    }
    let Some(settings) = vm.canvas.menu.as_mut() else {
        return Err(menu_error(
            vm,
            "SaveConfig requires a host configuration provider",
        ));
    };
    for (key, value) in entries {
        settings.set(&class, key, value);
    }
    settings.save_requested = true;
    val(Value::Void)
}

/// UE2 key names used by the retail PC key-binding pages (KEYNAME 0..254).
pub fn menu_key_name(key: u8) -> String {
    match key {
        1 => "LeftMouse",
        2 => "RightMouse",
        4 => "MiddleMouse",
        8 => "Backspace",
        9 => "Tab",
        13 => "Enter",
        16 => "Shift",
        17 => "Ctrl",
        18 => "Alt",
        19 => "Pause",
        20 => "CapsLock",
        27 => "Escape",
        32 => "Space",
        33 => "PageUp",
        34 => "PageDown",
        35 => "End",
        36 => "Home",
        37 => "Left",
        38 => "Up",
        39 => "Right",
        40 => "Down",
        45 => "Insert",
        46 => "Delete",
        236 => "MouseWheelUp",
        237 => "MouseWheelDown",
        48..=57 | 65..=90 => return char::from(key).to_string(),
        112..=123 => return format!("F{}", key - 111),
        _ => return String::new(),
    }
    .into()
}

fn ok(v: Value) -> Result<NativeOutcome, String> {
    Ok(NativeOutcome::Value(v))
}

/// Handles the menu console subset. Unknown commands fail with the original script stack.
/// Campaign ConsoleCommand retains its existing implementation when no menu provider exists.
/// Errors are plain strings; the registry caller wraps them into a VM error after the settings
/// borrow has ended.
pub fn menu_console(vm: &mut Vm<'_>, command: &str) -> Result<NativeOutcome, String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let verb = parts.first().copied().unwrap_or("").to_ascii_lowercase();
    if command
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("SETRES"))
    {
        let resolution = &command[6..];
        let resolution = resolution.trim();
        let (w, h) = resolution
            .split_once('x')
            .ok_or_else(|| format!("malformed resolution {resolution:?}"))?;
        let size = (
            w.parse::<u32>().map_err(|_| "invalid resolution width")?,
            h.parse::<u32>().map_err(|_| "invalid resolution height")?,
        );
        let settings = vm
            .canvas
            .menu
            .as_mut()
            .ok_or("menu console requires settings")?;
        if !settings.resolutions.contains(&size) {
            return Err(format!("unsupported resolution {resolution}"));
        }
        settings.set("port", "resolution", resolution.to_owned());
        return ok(Value::Str(String::new()));
    }
    match (verb.as_str(), parts.as_slice()) {
        ("toggleime", [_, "0"]) => ok(Value::Str(String::new())),
        ("get", [_, section, key]) => {
            let settings = vm
                .canvas
                .menu
                .as_ref()
                .ok_or("menu console requires settings")?;
            let value = settings
                .get(section, key)
                .or_else(|| {
                    (section.eq_ignore_ascii_case("gameinfo")
                        && key.eq_ignore_ascii_case("difficulty"))
                    .then_some("1")
                })
                .ok_or_else(|| format!("missing setting {section}.{key}"))?;
            ok(Value::Str(value.to_owned()))
        }
        ("get", [_, property]) => {
            let (section, key) = property
                .rsplit_once('.')
                .unwrap_or(("engine.client", property));
            let value = vm
                .canvas
                .menu
                .as_ref()
                .ok_or("menu console requires settings")?
                .get(section, key)
                .ok_or_else(|| format!("missing setting {section}.{key}"))?;
            ok(Value::Str(value.to_owned()))
        }
        ("set", [_, section, key, rest @ ..]) => {
            vm.canvas
                .menu
                .as_mut()
                .ok_or("menu console requires settings")?
                .set(section, key, rest.join(" "));
            ok(Value::Str(String::new()))
        }
        ("getcurrentres", [_]) => {
            let res = vm
                .canvas
                .menu
                .as_ref()
                .ok_or("menu console requires settings")?
                .get("port", "resolution")
                .unwrap_or("1280x720");
            ok(Value::Str(res.to_owned()))
        }
        ("setres", [_, res]) => {
            let valid = res
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
            let known = vm
                .canvas
                .menu
                .as_ref()
                .ok_or("menu console requires settings")?
                .resolutions
                .contains(&valid.ok_or_else(|| format!("malformed resolution {res:?}"))?);
            if !known {
                return Err(format!("unsupported resolution {res}"));
            }
            vm.canvas
                .menu
                .as_mut()
                .ok_or("menu console requires settings")?
                .set("port", "resolution", (*res).to_owned());
            ok(Value::Str(String::new()))
        }
        ("brightness" | "gamma" | "contrast", [_, v]) => {
            let value: f32 = v.parse().map_err(|_| "invalid display value")?;
            if !value.is_finite() || !(0.0..=2.0).contains(&value) {
                return Err("display value outside supported range".into());
            }
            vm.canvas
                .menu
                .as_mut()
                .ok_or("menu console requires settings")?
                .set("engine.client", &verb, value.to_string());
            ok(Value::Str(String::new()))
        }
        ("keyname", [_, n]) => {
            let key = n.parse::<u8>().map_err(|_| "invalid KEYNAME index")?;
            ok(Value::Str(menu_key_name(key)))
        }
        ("keybinding", [_, key]) => {
            let binding = vm
                .canvas
                .menu
                .as_ref()
                .ok_or("menu console requires settings")?
                .get("input", key)
                .unwrap_or("");
            ok(Value::Str(binding.to_owned()))
        }
        _ => Err(format!("unsupported menu ConsoleCommand {command:?}")),
    }
}

fn set_volume(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let volume = float(vm, a, 0)?;
    if !volume.is_finite() || !(-100.0..=0.0).contains(&volume) {
        return Err(menu_error(vm, "SetVolume outside -100..0 dB"));
    }
    let Some(settings) = vm.canvas.menu.as_mut() else {
        return Err(menu_error(vm, "SetVolume requires host mixer"));
    };
    settings.master_db = volume;
    val(Value::Void)
}

fn set_music_slider(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let volume = float(vm, a, 0)?;
    if !matches!(volume, 0.0 | 1.0 | 2.0) {
        return Err(menu_error(vm, "invalid music selector"));
    }
    let Some(settings) = vm.canvas.menu.as_mut() else {
        return Err(menu_error(vm, "SetMusicSliderPos requires host mixer"));
    };
    settings.music = volume as i32;
    val(Value::Void)
}

fn stop_music(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(settings) = vm.canvas.menu.as_mut() else {
        return Err(menu_error(vm, "StopMusic requires host mixer"));
    };
    settings.stop_music = true;
    val(Value::Void)
}

fn available_res(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(settings) = vm.canvas.menu.as_ref() else {
        return Err(menu_error(vm, "GetAvailableRes requires host modes"));
    };
    if a.is_empty() {
        return Err(menu_error(vm, "GetAvailableRes missing output array"));
    }
    a[0] = Value::Array(
        settings
            .resolutions
            .iter()
            .map(|&(w, h)| {
                Value::Struct(vec![
                    ("PixelWidth".into(), Value::Int(w as i32)),
                    ("PixelHeight".into(), Value::Int(h as i32)),
                ])
            })
            .collect(),
    );
    val(Value::Void)
}

/// item16c registry contribution. Scoped semantics fail explicitly outside the menu host.
pub fn item16c_defs() -> Vec<NativeDef> {
    [
        ("Object.SaveConfig", "native(536) final function SaveConfig()", save_config as crate::registry::NativeFn),
        ("Object.GetPropertyText", "native(200) final native static function string GetPropertyText(string PropName)", get_property_text),
        ("Object.SetPropertyText", "native(201) final native static function SetPropertyText(string PropName, string PropValue)", set_property_text),
        ("Engine.Actor.SetVolume", "native(361) final function SetVolume(float Volume)", set_volume),
        ("Engine.Actor.SetMusicSliderPos", "native(362) final function SetMusicSliderPos(int Pos)", set_music_slider),
        ("Engine.Actor.StopMusic", "native(345) final function StopMusic()", stop_music),
        ("Engine.LevelInfo.GetAvailableRes", "native(0) function GetAvailableRes(out array<VideoMode> Modes)", available_res),
    ].into_iter().map(|(path,signature,f)| partial(
        "item16c menu provider: scalar property text, menu settings and host modes; no filesystem or shell access",
        path, signature, "retail Core/Engine declarations; XIDInterf XIIIMenuAudioClientWindow/InternalOnKeyEvent and XIIIMenuVideoClientWindow.Created", f
    )).collect()
}

#[cfg(test)]
mod item16c_tests {
    use super::*;
    use crate::linker::ScriptSet;
    use crate::vm::VmLimits;

    #[test]
    fn property_text_round_trips_scalars_and_rejects_overflow_or_compounds() {
        assert_eq!(
            parse_property_text(&Value::Bool(false), "1"),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            parse_property_text(&Value::Byte(0), "255"),
            Ok(Value::Byte(255))
        );
        assert_eq!(
            parse_property_text(&Value::Byte(0), "CT_StrafeLookNotSameAxis"),
            Ok(Value::Byte(1))
        );
        assert!(parse_property_text(&Value::Byte(0), "256").is_err());
        assert_eq!(
            parse_property_text(&Value::Int(0), "-2147483648"),
            Ok(Value::Int(i32::MIN))
        );
        assert!(parse_property_text(&Value::Int(0), "2147483648").is_err());
        assert_eq!(
            parse_property_text(&Value::Float(0.0), "0.25"),
            Ok(Value::Float(0.25))
        );
        assert!(parse_property_text(&Value::Float(0.0), "NaN").is_err());
        assert_eq!(
            parse_property_text(&Value::Name("None".into()), "Walk"),
            Ok(Value::Name("Walk".into()))
        );
        assert!(parse_property_text(&Value::Array(vec![]), "x").is_err());
    }

    #[test]
    fn key_names_cover_boundary_codes_without_inventing_unknown_keys() {
        assert_eq!(menu_key_name(65), "A");
        assert_eq!(menu_key_name(112), "F1");
        assert_eq!(menu_key_name(123), "F12");
        assert!(menu_key_name(0).is_empty());
        assert!(menu_key_name(254).is_empty());
    }

    #[test]
    fn menu_console_reads_writes_and_rejects_unavailable_host_modes() {
        let set = ScriptSet::new();
        let mut vm = Vm::new(&set, VmLimits::default());
        let mut settings = MenuSettings {
            resolutions: vec![(1280, 720)],
            ..MenuSettings::default()
        };
        settings.set("HXAudio.HXAudioSubsystem", "MasterVolume", "0".into());
        vm.canvas.menu = Some(settings);

        menu_console(&mut vm, "SET HXAudio.HXAudioSubsystem MasterVolume -7")
            .expect("SET updates the in-memory config overlay");
        assert_eq!(
            menu_console(&mut vm, "GET HXAudio.HXAudioSubsystem MasterVolume"),
            Ok(NativeOutcome::Value(Value::Str("-7".into())))
        );
        menu_console(&mut vm, "SETRES1280x720").expect("supported mode accepted");
        assert_eq!(
            menu_console(&mut vm, "GETCURRENTRES"),
            Ok(NativeOutcome::Value(Value::Str("1280x720".into())))
        );
        assert!(menu_console(&mut vm, "SETRES9999x9999").is_err());
        assert!(menu_console(&mut vm, "SHELL something").is_err());
    }
}
