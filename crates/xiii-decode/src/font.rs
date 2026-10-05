//! `Engine.Font` payloads (package version 100, XIII licensees).
//!
//! Reverse-engineered from the four fonts of the GOG `TexturesPC/XIIIFonts.utx` package
//! (`PoliceF20`, `PoliceF16`, `XIIIConsoleFont`, `XIIISmallFont`), each of which decodes to the
//! payload end exactly with the layout below (4119/4113/4110/4110-byte exports). The class is
//! native: `engine.u` exports no `Engine.Font` class and the object has no tagged properties
//! (its property block is only the `None` terminator), so the whole payload after the terminator
//! is the native serialization.
//!
//! Layout (all counts are Unreal compact indices):
//!
//! ```text
//! compact                page count N (1, 2 or 4 in the corpus)
//! per page:
//!   compact              object reference to the page texture (export index + 1)
//!   compact              glyph count on this page (256 / N)
//!   glyph[count]:
//!     i32 StartU         source texel column
//!     i32 StartV         source texel row
//!     i32 USize          glyph width / advance
//!     i32 VSize          glyph height
//! i32                    glyphs per page (the first page's count)
//! i32                    kerning (0 in the corpus)
//! u8                     IsRemapped (0 in the corpus; the glyph table is indexed by code)
//! ```
//!
//! The glyph records of all pages concatenate into a byte-code table: glyph `i` is the glyph
//! for character code `i` (0..=255), and its page selects the texture. This is why a font with
//! four 256x256 pages stores 64 glyphs per page (the corpus total is always 256). The font's
//! page textures are sibling exports (`<Font>.TextureNa`), so the host resolves each page's
//! `texture` reference against the font package.

use xiii_package::{ObjectRef, Package};

use crate::common::{
    DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport, read_properties,
};

/// Class path of fonts.
pub const FONT_CLASS: &str = "Engine.Font";

/// One glyph, positioned in the texel space of its page texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontGlyph {
    /// Character code (the glyph's index in the concatenated table).
    pub code: u32,
    /// Index into [`Font::pages`].
    pub page: usize,
    /// Source `StartU`.
    pub start_u: i32,
    /// Source `StartV`.
    pub start_v: i32,
    /// Glyph width / advance (Unreal units, canvas pixels).
    pub u_size: i32,
    /// Glyph height.
    pub v_size: i32,
}

impl FontGlyph {
    /// Source rectangle `(u, v, width, height)` in texels.
    pub fn rect(&self) -> (f32, f32, f32, f32) {
        (
            self.start_u as f32,
            self.start_v as f32,
            self.u_size as f32,
            self.v_size as f32,
        )
    }
}

/// One texture page of a font.
#[derive(Debug, Clone, PartialEq)]
pub struct FontPage {
    /// Object reference (compact) of the page texture, resolved against the font package.
    pub texture: ObjectRef,
    /// Glyphs stored on this page, in table order.
    pub glyphs: Vec<FontGlyph>,
}

/// Decoded `Engine.Font`.
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    /// Texture pages.
    pub pages: Vec<FontPage>,
    /// Glyphs per page (the tail's first `i32`).
    pub glyphs_per_page: u32,
    /// Horizontal spacing adjustment (0 in the corpus).
    pub kerning: i32,
    /// `IsRemapped` byte (0 in the corpus).
    pub is_remapped: u8,
    /// Byte accounting.
    pub report: PayloadReport,
}

impl Font {
    /// Glyph for a character code, or `None` when the code is outside the table.
    pub fn glyph(&self, code: u32) -> Option<&FontGlyph> {
        let index = code as usize;
        let mut base = 0usize;
        for page in &self.pages {
            if index < base + page.glyphs.len() {
                return page.glyphs.get(index - base);
            }
            base += page.glyphs.len();
        }
        None
    }

    /// Total number of glyphs (256 in the corpus).
    pub fn glyph_count(&self) -> usize {
        self.pages.iter().map(|p| p.glyphs.len()).sum()
    }

    /// Width and height of `text` as the font measures it: the sum of the glyph advances and the
    /// maximum glyph height (`VSize`). Unknown codes contribute nothing (a diagnostic, not a
    /// silent fallback). `kerning` is added once per character after the first.
    pub fn text_size(&self, text: &str) -> (f32, f32) {
        let mut width = 0.0f32;
        let mut height = 0.0f32;
        let mut n = 0usize;
        for ch in text.chars() {
            let code = ch as u32;
            if let Some(g) = self.glyph(code) {
                if n > 0 {
                    width += self.kerning as f32;
                }
                width += g.u_size as f32;
                height = height.max(g.v_size as f32);
                n += 1;
            }
        }
        (width, height)
    }
}

fn bad(field: &'static str, reason: impl Into<String>) -> DecodeError {
    DecodeError::new(DecodeErrorKind::Invalid(reason.into())).in_field(field)
}

/// Decodes an `Engine.Font` export.
pub fn decode_font(package: &Package, data: &[u8], export: usize) -> DecodeResult<Font> {
    let props = read_properties(package, data, export, FONT_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;

    let page_count = r.count("font.pages", 1).map_err(ctx)?;
    if page_count == 0 {
        return Err(ctx(bad("font.pages", "font has no texture pages")));
    }
    let mut pages = Vec::with_capacity(page_count);
    let mut code = 0u32;
    for p in 0..page_count {
        let texture = r
            .object_ref(package)
            .map_err(|e| ctx(e.in_field("font.page.texture")))?;
        let glyph_count = r
            .count("font.page.glyphs", 16)
            .map_err(|e| ctx(e.in_field("font.page.glyphs")))?;
        let mut glyphs = Vec::with_capacity(glyph_count);
        for _ in 0..glyph_count {
            let start_u = r.i32().map_err(|e| ctx(e.in_field("font.glyph.start_u")))?;
            let start_v = r.i32().map_err(|e| ctx(e.in_field("font.glyph.start_v")))?;
            let u_size = r.i32().map_err(|e| ctx(e.in_field("font.glyph.u_size")))?;
            let v_size = r.i32().map_err(|e| ctx(e.in_field("font.glyph.v_size")))?;
            if u_size < 0 || v_size < 0 || start_u < 0 || start_v < 0 {
                return Err(ctx(bad(
                    "font.glyph",
                    format!(
                        "negative glyph metric at page {p}: ({start_u},{start_v},{u_size},{v_size})"
                    ),
                )));
            }
            glyphs.push(FontGlyph {
                code,
                page: p,
                start_u,
                start_v,
                u_size,
                v_size,
            });
            code += 1;
        }
        pages.push(FontPage { texture, glyphs });
    }

    let glyphs_per_page = r
        .i32()
        .map_err(|e| ctx(e.in_field("font.glyphs_per_page")))?;
    if glyphs_per_page < 0 {
        return Err(ctx(bad(
            "font.glyphs_per_page",
            format!("{glyphs_per_page} is negative"),
        )));
    }
    let kerning = r.i32().map_err(|e| ctx(e.in_field("font.kerning")))?;
    let is_remapped = r.u8().map_err(|e| ctx(e.in_field("font.is_remapped")))?;
    // The corpus stores `IsRemapped = 0` (direct byte-code indexing) and consumes the payload
    // exactly. A remapped font would need the `CharRemap` map, which is not decoded.
    if is_remapped != 0 {
        return Err(ctx(DecodeError::new(DecodeErrorKind::Unsupported(
            format!("font.IsRemapped = {is_remapped}: the CharRemap table is not decoded"),
        ))));
    }
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(Font {
        pages,
        glyphs_per_page: glyphs_per_page as u32,
        kerning,
        is_remapped,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, Bytes, parse};

    /// Encodes a compact index (inverse of `Cursor::compact_index`).
    fn compact(value: i32) -> Vec<u8> {
        crate::common::test_package::compact(value)
    }

    /// Builds a synthetic `Engine.Font` export. `pages` is a list of `(texture_export,
    /// glyphs)`; a glyph is `(start_u, start_v, u_size, v_size)`.
    #[allow(clippy::type_complexity)]
    fn font_package(
        pages: &[(i32, &[(i32, i32, i32, i32)])],
        glyphs_per_page: i32,
    ) -> (Vec<u8>, usize) {
        let mut b = Builder::new();
        // Sibling texture exports so the page object references resolve.
        let tex0 = b.export("Texture", "T0", vec![0u8]);
        let tex1 = b.export("Texture", "T1", vec![0u8]);
        let _ = (tex0, tex1);
        let mut native = compact(pages.len() as i32);
        for (tex, gs) in pages {
            native.extend(compact(*tex));
            native.extend(compact(gs.len() as i32));
            for (su, sv, us, vs) in gs.iter() {
                native.extend_from_slice(&su.to_le_bytes());
                native.extend_from_slice(&sv.to_le_bytes());
                native.extend_from_slice(&us.to_le_bytes());
                native.extend_from_slice(&vs.to_le_bytes());
            }
        }
        native.extend_from_slice(&glyphs_per_page.to_le_bytes());
        native.extend_from_slice(&0i32.to_le_bytes()); // kerning
        native.push(0); // IsRemapped
        let payload = Bytes::default().raw(&[0]).raw(&native).0; // property terminator + native
        let i = b.export("Font", "F", payload);
        (b.build(), i)
    }

    #[test]
    fn synthetic_two_page_font_round_trips() {
        // Two pages of two glyphs; page 0 references export 0 (T0), page 1 export 1 (T1).
        let (bytes, i) = font_package(
            &[
                (1, &[(0, 1, 6, 17), (10, 1, 6, 17)]),
                (2, &[(0, 1, 8, 17), (12, 1, 8, 17)]),
            ],
            2,
        );
        let p = parse(&bytes);
        let f = decode_font(&p, &bytes, i).unwrap();
        assert_eq!(f.glyph_count(), 4);
        assert_eq!(f.glyphs_per_page, 2);
        assert_eq!(f.kerning, 0);
        assert_eq!(f.is_remapped, 0);
        assert_eq!(f.pages.len(), 2);
        assert_eq!(f.glyph(b'A' as u32), None);
        assert_eq!(f.glyph(1).unwrap().start_u, 10);
        assert_eq!(f.glyph(2).unwrap().page, 1);
        assert_eq!(f.glyph(3).unwrap().u_size, 8);
        // Codes 1 and 2: widths 6 + 8, height 17.
        assert_eq!(f.text_size("\u{1}\u{2}"), (14.0, 17.0));
        assert_eq!(f.report.unknown_bytes(), 0);
    }

    /// Builds a one-page one-glyph font whose native data has `tail` extra bytes. Used to check
    /// that a payload with bytes the decoder cannot account for is rejected.
    fn font_with_tail(tail: &[u8]) -> (Vec<u8>, usize) {
        let mut b = Builder::new();
        b.export("Texture", "T0", vec![0u8]);
        let mut native = compact(1);
        native.extend(compact(1));
        native.extend(compact(1));
        native.extend_from_slice(&0i32.to_le_bytes()); // StartU
        native.extend_from_slice(&1i32.to_le_bytes()); // StartV
        native.extend_from_slice(&6i32.to_le_bytes()); // USize
        native.extend_from_slice(&17i32.to_le_bytes()); // VSize
        native.extend_from_slice(&1i32.to_le_bytes()); // glyphs per page
        native.extend_from_slice(&0i32.to_le_bytes()); // kerning
        native.push(0); // IsRemapped
        native.extend_from_slice(tail);
        let payload = Bytes::default().raw(&[0]).raw(&native).0;
        let i = b.export("Font", "F", payload);
        (b.build(), i)
    }

    #[test]
    fn synthetic_font_rejects_bad_layouts() {
        // A glyph with a negative metric is rejected rather than stored.
        let (bytes, i) = font_package(&[(1, &[(-1, 1, 6, 17)])], 1);
        let p = parse(&bytes);
        assert!(decode_font(&p, &bytes, i).is_err());

        // A zero page count is rejected.
        let (bytes, i) = font_package(&[], 0);
        let p = parse(&bytes);
        assert!(decode_font(&p, &bytes, i).is_err());

        // An extra byte is an unconsumed tail with offset context.
        let (bytes, i) = font_with_tail(&[0xEE]);
        let p = parse(&bytes);
        let e = decode_font(&p, &bytes, i).unwrap_err();
        assert!(
            matches!(e.kind, DecodeErrorKind::UnconsumedTail { bytes: 1 }),
            "{e}"
        );
        assert!(e.offset.is_some() && e.export == Some(i as u32));
    }

    /// Opt-in corpus test: every font of the GOG `XIIIFonts.utx` decodes to its payload end with
    /// 256 glyphs, a valid page texture, and non-negative metrics.
    #[test]
    fn opt_in_gog_fonts_decode() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut path = std::path::PathBuf::from(&root);
        if path.is_relative() {
            path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(path);
        }
        let file = path.join("TexturesPC/XIIIFonts.utx");
        let data = std::fs::read(&file).expect("read XIIIFonts.utx");
        let p = parse(&data);
        let mut fonts = 0usize;
        for e in 0..p.exports().len() {
            let class = p.export_class_path(e).unwrap_or("?");
            if !class.eq_ignore_ascii_case(FONT_CLASS) {
                continue;
            }
            let f = decode_font(&p, &data, e).unwrap_or_else(|err| panic!("export {e}: {err}"));
            let name = p.object_path(ObjectRef::Export(e as u32)).unwrap_or("?");
            assert_eq!(f.glyph_count(), 256, "{name}");
            assert_eq!(f.report.unknown_bytes(), 0, "{name}");
            assert!(f.report.unsupported_tail.is_none(), "{name}");
            for page in &f.pages {
                assert!(
                    matches!(page.texture, ObjectRef::Export(_)),
                    "{name} page texture is not a local export"
                );
            }
            println!(
                "[font test] {name}: {} pages, {} glyphs, per-page {}, kerning {}",
                f.pages.len(),
                f.glyph_count(),
                f.glyphs_per_page,
                f.kerning
            );
            fonts += 1;
        }
        assert_eq!(fonts, 4, "expected the four corpus fonts");
    }
}
