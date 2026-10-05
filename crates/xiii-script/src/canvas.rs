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
use crate::vm::{TraceKind, Vm, VmErrorKind, VmResult};

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
    let (mut x, y) = pen(vm, c.this);
    if justify == 1 {
        x -= xl * 0.5;
    } else if justify == 2 {
        x -= xl;
    }
    let color = draw_color(vm, c.this);
    let style = prop_u8(vm, c.this, "Style", 1);
    let clip = clip(vm, c.this);
    if material.is_none() {
        vm.canvas.push(DrawCommand::Rect {
            x,
            y,
            xl,
            yl,
            color,
            clip,
        });
    } else {
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
    }
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
        "engine.u Canvas.DrawTile decoded; records a Tile (or Rect when Tex is None)",
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
/// No video decoder is linked; the call is accepted, recorded and reports success so the
/// decoded `XIIIMenu` new-game path proceeds (the host labels the skipped video).
fn video_player_open(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = text(vm, a, 0).unwrap_or_default();
    vm.note(TraceKind::Note(format!(
        "item16 VideoPlayer.Open({name:?}) accepted (no Bink decoder; not played)"
    )));
    val(Value::Bool(true))
}

/// `Engine.VideoPlayer.Play()` (native 483).
fn video_player_play(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "item16 VideoPlayer.Play() accepted (no Bink decoder; not played)".into(),
    ));
    val(Value::Void)
}

/// `Engine.VideoPlayer.Stop()` (native 482).
fn video_player_stop(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(
        "item16 VideoPlayer.Stop() accepted (no Bink decoder)".into(),
    ));
    val(Value::Void)
}

/// `Engine.VideoPlayer.GetStatus() -> int` (native 476; the same number is
/// `ScriptedTexture.TextSize(string, out, out, Font)`, which the VM's argument-count
/// resolution keeps distinct). `0` means "no video playing", so
/// `XIIIMenu.PlayingVideo.Tick` runs `EndOfVideo` on the next host drive.
fn video_player_get_status(
    _vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Int(0))
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

/// `Engine.PlayerController.ClientTravel(string URL, byte TravelType, bool bItems)`.
///
/// This is the game's own map-load request (`XIIIMenu.EndOfVideo` calls it with `Plage00`
/// after the new-game video). The headless VM cannot change map, so the request is recorded
/// in the trace via [`format_travel_note`]; the host performs the travel step.
fn client_travel(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let url = text(vm, a, 0)?;
    let travel_type = match a.get(1) {
        Some(Value::Byte(b)) => *b,
        Some(Value::Int(i)) => *i as u8,
        _ => 0,
    };
    let items = flag(a, 2);
    vm.note(TraceKind::Note(format_travel_note(
        &url,
        travel_type,
        items,
    )));
    val(Value::Void)
}

/// The menu-path natives defined by this block. `registry::builtin_defs` extends the
/// built-in table with these.
#[allow(clippy::vec_init_then_push)]
pub fn menu_defs() -> Vec<NativeDef> {
    let mut v: Vec<NativeDef> = Vec::new();
    v.push(def(
        "VideoPlayer.Open",
        "native(484) final native static function bool Open(string Filename)",
        "engine.u VideoPlayer.Open decoded; XIIIMenu.InternalOnClick opens sVideo (default cine00)",
        video_player_open,
    ));
    v.push(def(
        "VideoPlayer.Play",
        "native(483) final native static function Play()",
        "engine.u VideoPlayer.Play decoded; XIIIMenu.InternalOnClick calls it",
        video_player_play,
    ));
    v.push(def(
        "VideoPlayer.Stop",
        "native(482) final native static function Stop()",
        "engine.u VideoPlayer.Stop decoded; XIIIMenu.InternalOnKeyEvent stops the video",
        video_player_stop,
    ));
    v.push(def(
        "VideoPlayer.GetStatus",
        "native(476) final native static function int GetStatus()",
        "engine.u VideoPlayer.GetStatus decoded; XIIIMenu.PlayingVideo.Tick ends on 0 (index 476 also names ScriptedTexture.TextSize, separated by argument count)",
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
    v.push(def(
        "PlayerController.ClientTravel",
        "native(0) net reliable native event static function ClientTravel(string URL, byte<ETravelType> TravelType, bool bItems)",
        "engine.u PlayerController.ClientTravel decoded; XIIIMenu.EndOfVideo travels to Plage00 (the game's own map-load request)",
        client_travel,
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
