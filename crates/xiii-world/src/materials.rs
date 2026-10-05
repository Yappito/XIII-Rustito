//! UE2 material-graph resolution.
//!
//! XIII maps reference material objects (`Engine.Texture`, `Engine.Shader`,
//! `Engine.FinalBlend`, `Engine.TexPanner`, `Engine.TexRotator`, `Engine.TexOscillator`,
//! `Engine.TexEnvMap`, the XIII-specific `Engine.SinusModifier`, `Engine.Combiner`, ...)
//! instead of plain textures. This module walks the graph the surface material points at and
//! produces a small, explicit [`ResolvedMaterial`]: the base texture, the blend mode, whether
//! the surface is two-sided, a chain of animated UV transforms and everything it could not model
//! (listed, never dropped).
//!
//! The walker is deliberately independent of the package reader so it can be exercised with a
//! synthetic graph (see the unit tests). The importer implements the [`MaterialLookup`] side.
//!
//! Blend mapping is from the game's own reflected enums (measured with `xiii-tool script` /
//! `xiii-tool inspect` on `system/engine.u`):
//!
//! * `Shader.EOutputBlending` = `OB_Normal, OB_Masked, OB_Modulate, OB_Translucent, OB_Invisible,
//!   OB_AlphaBlend, OB_Darken, OB_Brighten, OB_AddWhiteFog`.
//! * `FinalBlend.EFrameBufferBlending` = `FB_Overwrite, FB_Modulate, FB_AlphaBlend,
//!   FB_AlphaModulate_MightNotFogCorrectly, FB_Translucent, FB_Darken, FB_Brighten, FB_Invisible`.
//! * `TexOscillator.ETexOscillationType` = `OT_Pan, OT_Stretch`.
//!
//! Which of the two final framebuffer operations the *original* engine actually emits for each
//! enum value is not captured here (it is a **hypothesis** based on the enum names); the accepted
//! task requires naming the hypothesis, not proving it against the retail renderer.

use std::collections::HashSet;

/// Maximum material-graph depth before the walker stops (a guard against pathological data).
pub const MAX_CHAIN: usize = 64;

/// Key identifying one material object: lower-case package name and export index.
pub type NodeKey = (String, usize);

/// How a surface combines with the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlendMode {
    /// Fully opaque.
    Opaque,
    /// Alpha test with a threshold in `0..=1` (Bevy `AlphaMode::Mask`).
    Masked(f32),
    /// Alpha blend (Bevy `AlphaMode::Blend`).
    Alpha,
    /// Additive (brighten).
    Additive,
    /// Modulate (multiply by destination).
    Modulate,
    /// Darken.
    Darken,
    /// Not drawn.
    Invisible,
    /// The enum value is recognised but has no faithful mapping here (also listed in
    /// [`ResolvedMaterial::unsupported`]).
    Unsupported,
}

impl BlendMode {
    /// Stable short name for counters and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            BlendMode::Opaque => "opaque",
            BlendMode::Masked(_) => "masked",
            BlendMode::Alpha => "alpha",
            BlendMode::Additive => "additive",
            BlendMode::Modulate => "modulate",
            BlendMode::Darken => "darken",
            BlendMode::Invisible => "invisible",
            BlendMode::Unsupported => "unsupported",
        }
    }
}

/// One texture-coordinate operation, applied in the order encountered from the surface material
/// inward. Rates are per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UvOp {
    /// Constant translation in texture units per second.
    Pan {
        /// U speed.
        speed_u: f32,
        /// V speed.
        speed_v: f32,
    },
    /// Rotation about a UV pivot: `angle = base + rate * t` radians.
    Rotate {
        /// Angle at `t = 0`, radians.
        base: f32,
        /// Angular speed, radians per second.
        rate: f32,
        /// Pivot U.
        center_u: f32,
        /// Pivot V.
        center_v: f32,
    },
    /// Constant scale about the UV origin.
    Scale {
        /// U scale.
        scale_u: f32,
        /// V scale.
        scale_v: f32,
    },
    /// `OT_Pan`: offset = `amplitude * sin(2*pi*rate*t + phase)` per axis.
    OscillatePan {
        /// U amplitude.
        amplitude_u: f32,
        /// V amplitude.
        amplitude_v: f32,
        /// U frequency, cycles per second.
        rate_u: f32,
        /// V frequency, cycles per second.
        rate_v: f32,
        /// U phase, radians.
        phase_u: f32,
        /// V phase, radians.
        phase_v: f32,
    },
    /// `OT_Stretch`: scale = `1 + amplitude * sin(2*pi*rate*t + phase)` per axis.
    OscillateScale {
        /// U amplitude.
        amplitude_u: f32,
        /// V amplitude.
        amplitude_v: f32,
        /// U frequency, cycles per second.
        rate_u: f32,
        /// V frequency, cycles per second.
        rate_v: f32,
        /// U phase, radians.
        phase_u: f32,
        /// V phase, radians.
        phase_v: f32,
    },
}

/// A material graph resolved to what the renderer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMaterial {
    /// Index into the scene's texture list, when a base texture was reached.
    pub base: Option<usize>,
    /// Framebuffer/alpha blend.
    pub blend: BlendMode,
    /// The surface is drawn from both sides.
    pub two_sided: bool,
    /// Animated UV transform chain (empty for a static material).
    pub uv_transform: Vec<UvOp>,
    /// Optional colour multiply (RGBA, linear 0..=1).
    pub color_tint: Option<[f32; 4]>,
    /// Material classes/features that were recognised but not modelled.
    pub unsupported: Vec<String>,
    /// Short class names visited from the surface material inward (diagnostics).
    pub class_chain: Vec<String>,
}

impl Default for ResolvedMaterial {
    fn default() -> Self {
        Self {
            base: None,
            blend: BlendMode::Opaque,
            two_sided: false,
            uv_transform: Vec::new(),
            color_tint: None,
            unsupported: Vec::new(),
            class_chain: Vec::new(),
        }
    }
}

/// One decoded material object, normalised for the walker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaterialNode {
    /// Short class name (e.g. `Shader`).
    pub class: String,
    /// Decoded texture index, only for `Texture`.
    pub texture: Option<usize>,
    /// Blend implied by the texture's own alpha flags, only for `Texture`.
    pub texture_alpha: Option<BlendMode>,
    /// `Shader.Diffuse` / generic colour input.
    pub diffuse: Option<NodeKey>,
    /// Generic `Material` link (most modifiers and `FinalBlend`).
    pub material: Option<NodeKey>,
    /// `Combiner.Material1`.
    pub material1: Option<NodeKey>,
    /// `Combiner.Material2`.
    pub material2: Option<NodeKey>,
    /// `Shader.OutputBlending`.
    pub output_blending: Option<u8>,
    /// `FinalBlend.FrameBufferBlending`.
    pub frame_buffer_blending: Option<u8>,
    /// `TwoSided`.
    pub two_sided: Option<bool>,
    /// `FinalBlend.AlphaTest`.
    pub alpha_test: Option<bool>,
    /// `FinalBlend.AlphaRef` (byte).
    pub alpha_ref: Option<u8>,
    /// UV operations this node contributes.
    pub uv_ops: Vec<UvOp>,
    /// Optional constant colour tint (`ColorModifier.Color`).
    pub color: Option<[f32; 4]>,
    /// Properties present here that are not modelled (diagnostics, one per property).
    pub ignored: Vec<String>,
}

/// Decodes one material object to a [`MaterialNode`]; the importer implements this.
pub trait MaterialLookup {
    /// `None` when the key cannot be resolved or decoded.
    fn node(&mut self, key: &NodeKey) -> Option<MaterialNode>;
}

impl<F> MaterialLookup for F
where
    F: FnMut(&NodeKey) -> Option<MaterialNode>,
{
    fn node(&mut self, key: &NodeKey) -> Option<MaterialNode> {
        self(key)
    }
}

/// Maps a `Shader.EOutputBlending` value to a blend mode and an optional "not modelled" note.
///
/// Values are the reflected enumerator order (measured, see the module docs). The enum names
/// suggest the mapping; the exact original framebuffer state is a hypothesis.
pub fn output_blending(value: u8) -> (BlendMode, Option<&'static str>) {
    match value {
        0 => (BlendMode::Opaque, None),                        // OB_Normal
        1 => (BlendMode::Masked(0.5), None),                   // OB_Masked
        2 => (BlendMode::Modulate, None),                      // OB_Modulate
        3 => (BlendMode::Alpha, None),                         // OB_Translucent
        4 => (BlendMode::Invisible, None),                     // OB_Invisible
        5 => (BlendMode::Alpha, None),                         // OB_AlphaBlend
        6 => (BlendMode::Darken, None),                        // OB_Darken
        7 => (BlendMode::Additive, None),                      // OB_Brighten
        8 => (BlendMode::Unsupported, Some("OB_AddWhiteFog")), // OB_AddWhiteFog
        _ => (BlendMode::Unsupported, Some("OB_unknown")),
    }
}

/// Maps a `FinalBlend.EFrameBufferBlending` value to a blend mode and an optional note.
pub fn frame_buffer_blending(value: u8) -> (BlendMode, Option<&'static str>) {
    match value {
        0 => (BlendMode::Opaque, None),   // FB_Overwrite
        1 => (BlendMode::Modulate, None), // FB_Modulate
        2 => (BlendMode::Alpha, None),    // FB_AlphaBlend
        3 => (
            BlendMode::Alpha,
            Some("FB_AlphaModulate_MightNotFogCorrectly"),
        ), // FB_AlphaModulate
        4 => (BlendMode::Alpha, None),    // FB_Translucent
        5 => (BlendMode::Darken, None),   // FB_Darken
        6 => (BlendMode::Additive, None), // FB_Brighten
        7 => (BlendMode::Invisible, None), // FB_Invisible
        _ => (BlendMode::Unsupported, Some("FB_unknown")),
    }
}

/// Resolves a material graph from `start` inward. Cycles and unresolvable keys are recorded in
/// [`ResolvedMaterial::unsupported`] instead of looping.
pub fn resolve(start: Option<NodeKey>, lookup: &mut dyn MaterialLookup) -> ResolvedMaterial {
    let mut out = ResolvedMaterial::default();
    let mut visited: HashSet<NodeKey> = HashSet::new();
    let Some(start) = start else {
        return out;
    };
    let mut stack = vec![start];
    let mut guard = 0usize;
    // The first material class (from the surface inward) that implies a framebuffer operation
    // decides the blend; a bare texture only decides when nothing above it did. `BlendMode`'s
    // default (`Opaque`) is indistinguishable from an explicit Overwrite, so this flag carries
    // the decision separately.
    let mut blend_set = false;
    while let Some(key) = stack.pop() {
        guard += 1;
        if guard > MAX_CHAIN {
            push_once(&mut out.unsupported, "chain.too_long");
            break;
        }
        if !visited.insert(key.clone()) {
            push_once(&mut out.unsupported, &format!("cycle:{}.{}", key.0, key.1));
            continue;
        }
        let Some(node) = lookup.node(&key) else {
            push_once(
                &mut out.unsupported,
                &format!("unresolved:{}.{}", key.0, key.1),
            );
            continue;
        };
        out.class_chain.push(node.class.clone());
        apply(&mut out, node, &mut stack, &mut blend_set);
    }
    out
}

/// Adds `note` once, keeping the first occurrence of each distinct feature.
fn push_once(list: &mut Vec<String>, note: &str) {
    if !list.iter().any(|n| n == note) {
        list.push(note.to_owned());
    }
}

/// Applies `blend` only when no outer material has already chosen one.
fn set_blend(
    out: &mut ResolvedMaterial,
    blend: BlendMode,
    blend_set: &mut bool,
    note: Option<&'static str>,
) {
    if !*blend_set {
        out.blend = blend;
        *blend_set = true;
    }
    if let Some(note) = note {
        push_once(&mut out.unsupported, note);
    }
}

fn apply(
    out: &mut ResolvedMaterial,
    node: MaterialNode,
    stack: &mut Vec<NodeKey>,
    blend_set: &mut bool,
) {
    match node.class.as_str() {
        "Texture" => {
            match node.texture {
                Some(t) => {
                    out.base.get_or_insert(t);
                }
                None => push_once(&mut out.unsupported, "texture.unresolved"),
            }
            if !*blend_set && let Some(b) = node.texture_alpha {
                out.blend = b;
                *blend_set = true;
            }
        }
        "Shader" => {
            // `OB_Normal` (0) means "leave the decision to the input", so only a non-zero,
            // explicitly-tagged OutputBlending overrides the texture's own alpha flags.
            if let Some(v) = node.output_blending
                && v != 0
            {
                let (blend, note) = output_blending(v);
                set_blend(out, blend, blend_set, note);
            }
            if node.two_sided == Some(true) {
                out.two_sided = true;
            }
            // An Opacity input would need a second texture/combine; not modelled.
            for ig in &node.ignored {
                push_once(&mut out.unsupported, ig);
            }
            match node.diffuse {
                Some(d) => stack.push(d),
                None => push_once(&mut out.unsupported, "shader.diffuse_none"),
            }
        }
        "FinalBlend" => {
            if node.alpha_test == Some(true) {
                let ref_byte = node.alpha_ref.unwrap_or(0);
                set_blend(
                    out,
                    BlendMode::Masked(f32::from(ref_byte) / 255.0),
                    blend_set,
                    None,
                );
            } else {
                let (blend, note) = frame_buffer_blending(node.frame_buffer_blending.unwrap_or(0));
                set_blend(out, blend, blend_set, note);
            }
            if node.two_sided == Some(true) {
                out.two_sided = true;
            }
            match node.material {
                Some(m) => stack.push(m),
                None => push_once(&mut out.unsupported, "finalblend.material_none"),
            }
        }
        "Combiner" => {
            push_once(&mut out.unsupported, "combiner(inputs merged to first)");
            match (node.material1, node.material2) {
                (Some(m1), _) => stack.push(m1),
                (None, Some(m2)) => stack.push(m2),
                (None, None) => push_once(&mut out.unsupported, "combiner.material_none"),
            }
        }
        "TexPanner" | "TexRotator" | "TexScaler" | "TexOscillator" | "TexModifier"
        | "TexCoordSource" | "ColorModifier" => {
            out.uv_transform.extend(node.uv_ops);
            if let Some(c) = node.color {
                out.color_tint = Some(c);
            }
            for ig in &node.ignored {
                push_once(&mut out.unsupported, ig);
            }
            follow_material(out, node.material, stack);
        }
        "TexEnvMap" => {
            push_once(&mut out.unsupported, "texenvmap");
            for ig in &node.ignored {
                push_once(&mut out.unsupported, ig);
            }
            follow_material(out, node.material, stack);
        }
        "SinusModifier" => {
            push_once(&mut out.unsupported, "sinusmodifier");
            for ig in &node.ignored {
                push_once(&mut out.unsupported, ig);
            }
            follow_material(out, node.material, stack);
        }
        other => {
            push_once(&mut out.unsupported, &format!("class.{other}"));
            for ig in &node.ignored {
                push_once(&mut out.unsupported, ig);
            }
            follow_material(out, node.material, stack);
        }
    }
}

fn follow_material(
    out: &mut ResolvedMaterial,
    material: Option<NodeKey>,
    stack: &mut Vec<NodeKey>,
) {
    match material {
        Some(m) => stack.push(m),
        None => push_once(&mut out.unsupported, "material_none"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn key(name: &str, idx: usize) -> NodeKey {
        (name.to_owned(), idx)
    }

    fn texture(idx: usize, alpha: Option<BlendMode>) -> MaterialNode {
        MaterialNode {
            class: "Texture".into(),
            texture: Some(idx),
            texture_alpha: alpha,
            ..Default::default()
        }
    }

    fn graph(nodes: Vec<(NodeKey, MaterialNode)>) -> impl FnMut(&NodeKey) -> Option<MaterialNode> {
        let map: HashMap<NodeKey, MaterialNode> = nodes.into_iter().collect();
        move |k: &NodeKey| map.get(k).cloned()
    }

    #[test]
    fn finalblend_over_shader_over_panner_over_texture() {
        // FinalBlend(FB_AlphaBlend) -> Shader(Diffuse, OB_Modulate ignored as inner) ->
        // TexPanner(Pan U) -> Texture.
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "FinalBlend".into(),
                    material: Some(key("p", 1)),
                    frame_buffer_blending: Some(2),
                    two_sided: Some(true),
                    ..Default::default()
                },
            ),
            (
                key("p", 1),
                MaterialNode {
                    class: "Shader".into(),
                    diffuse: Some(key("p", 2)),
                    output_blending: Some(2),
                    ..Default::default()
                },
            ),
            (
                key("p", 2),
                MaterialNode {
                    class: "TexPanner".into(),
                    material: Some(key("p", 3)),
                    uv_ops: vec![UvOp::Pan {
                        speed_u: 0.5,
                        speed_v: 0.0,
                    }],
                    ..Default::default()
                },
            ),
            (key("p", 3), texture(7, Some(BlendMode::Opaque))),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(r.base, Some(7));
        // Outer FinalBlend blend wins over the inner Shader's OutputBlending.
        assert_eq!(r.blend, BlendMode::Alpha);
        assert!(r.two_sided);
        assert_eq!(
            r.uv_transform,
            vec![UvOp::Pan {
                speed_u: 0.5,
                speed_v: 0.0
            }]
        );
        assert!(r.unsupported.is_empty(), "{:?}", r.unsupported);
        assert_eq!(
            r.class_chain,
            ["FinalBlend", "Shader", "TexPanner", "Texture"]
        );
    }

    #[test]
    fn texture_alpha_used_when_shader_is_normal() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "Shader".into(),
                    diffuse: Some(key("p", 1)),
                    output_blending: None,
                    ..Default::default()
                },
            ),
            (key("p", 1), texture(3, Some(BlendMode::Masked(0.5)))),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(r.base, Some(3));
        assert_eq!(r.blend, BlendMode::Masked(0.5));
    }

    #[test]
    fn shader_masked_maps_to_threshold() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "Shader".into(),
                    diffuse: Some(key("p", 1)),
                    output_blending: Some(1),
                    ..Default::default()
                },
            ),
            (key("p", 1), texture(3, None)),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(r.blend, BlendMode::Masked(0.5));
    }

    #[test]
    fn finalblend_alphatest_overrides_frame_buffer_blending() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "FinalBlend".into(),
                    material: Some(key("p", 1)),
                    frame_buffer_blending: Some(6),
                    alpha_test: Some(true),
                    alpha_ref: Some(128),
                    ..Default::default()
                },
            ),
            (key("p", 1), texture(1, None)),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        // 128/255; the Additive mapping is superseded by the alpha test.
        assert_eq!(r.blend, BlendMode::Masked(128.0 / 255.0));
    }

    #[test]
    fn sinus_modifier_is_reported_but_still_resolves_base() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "SinusModifier".into(),
                    material: Some(key("p", 1)),
                    ..Default::default()
                },
            ),
            (
                key("p", 1),
                MaterialNode {
                    class: "TexOscillator".into(),
                    material: Some(key("p", 2)),
                    uv_ops: vec![UvOp::OscillateScale {
                        amplitude_u: 0.1,
                        amplitude_v: 0.0,
                        rate_u: 0.2,
                        rate_v: 0.0,
                        phase_u: 0.0,
                        phase_v: 0.0,
                    }],
                    ..Default::default()
                },
            ),
            (key("p", 2), texture(9, None)),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(r.base, Some(9));
        assert!(r.unsupported.contains(&"sinusmodifier".to_owned()));
        assert_eq!(r.uv_transform.len(), 1);
    }

    #[test]
    fn cycle_is_reported_not_looped() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "TexPanner".into(),
                    material: Some(key("p", 1)),
                    ..Default::default()
                },
            ),
            (
                key("p", 1),
                MaterialNode {
                    class: "FinalBlend".into(),
                    material: Some(key("p", 0)),
                    ..Default::default()
                },
            ),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert!(r.base.is_none());
        assert!(
            r.unsupported.iter().any(|u| u.starts_with("cycle:")),
            "{:?}",
            r.unsupported
        );
    }

    #[test]
    fn unresolved_key_is_reported() {
        let mut lookup = graph(vec![]);
        let r = resolve(Some(key("missing", 4)), &mut lookup);
        assert!(r.base.is_none());
        assert_eq!(r.unsupported, ["unresolved:missing.4"]);
    }

    #[test]
    fn none_start_is_empty_opaque() {
        let mut lookup = graph(vec![]);
        let r = resolve(None, &mut lookup);
        assert_eq!(r, ResolvedMaterial::default());
    }

    #[test]
    fn self_cycle_at_start_is_reported_once() {
        let mut lookup = graph(vec![(
            key("p", 0),
            MaterialNode {
                class: "TexPanner".into(),
                material: Some(key("p", 0)),
                ..Default::default()
            },
        )]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(
            r.unsupported
                .iter()
                .filter(|u| u.starts_with("cycle:"))
                .count(),
            1
        );
    }

    #[test]
    fn combiner_uses_first_input_and_reports_second() {
        let mut lookup = graph(vec![
            (
                key("p", 0),
                MaterialNode {
                    class: "Combiner".into(),
                    material1: Some(key("p", 1)),
                    material2: Some(key("p", 2)),
                    ..Default::default()
                },
            ),
            (key("p", 1), texture(1, None)),
            (key("p", 2), texture(2, None)),
        ]);
        let r = resolve(Some(key("p", 0)), &mut lookup);
        assert_eq!(r.base, Some(1));
        assert!(
            r.unsupported.iter().any(|u| u.starts_with("combiner")),
            "{:?}",
            r.unsupported
        );
    }
}
