//! `Engine.Texture` and `Engine.Palette` payloads (package version 100, XIII licensees).
//!
//! Layout after the tagged properties (verified on all 3 507 GOG textures, see README):
//!
//! ```text
//! compact            mip count
//! per mip:
//!   i32              lazy-array skip offset = absolute file offset of the end of the data
//!   compact          data byte count
//!   u8[count]        pixel data
//!   i32 USize, i32 VSize, u8 UBits, u8 VBits
//! if Format in {P8, P4}:  (XIII, licensee >= 42; UModel UnTexture2.cpp)
//!   compact count + count * (u8 R, u8 G, u8 B, u8 A)   inline palette
//! if licensee >= 55:  3 bytes of unknown meaning (UModel skips them; values vary)
//! ```
//!
//! Format numbers come from the game's own `Engine.ETextureFormat` enum in `engine.u`:
//! P8, RGBA7, NoUsed1, DXT1, RGB8, RGBA8, NODATA, DXT3, DXT5, G8, G16, RRRGGGBBB, RGB565, P4.

use xiii_package::Package;

use crate::common::{
    DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport, Props,
    read_properties,
};

/// Class path of textures.
pub const TEXTURE_CLASS: &str = "Engine.Texture";
/// Class path of palettes.
pub const PALETTE_CLASS: &str = "Engine.Palette";

/// `ETextureFormat` as declared in XIII's `engine.u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TextureFormat {
    /// 0: 8-bit palette index.
    P8,
    /// 1
    Rgba7,
    /// 2: unused slot in XIII.
    NoUsed1,
    /// 3: BC1.
    Dxt1,
    /// 4: 24-bit, stored B, G, R.
    Rgb8,
    /// 5: 32-bit, stored B, G, R, A.
    Rgba8,
    /// 6
    NoData,
    /// 7: BC2.
    Dxt3,
    /// 8: BC3.
    Dxt5,
    /// 9: 8-bit grey.
    G8,
    /// 10: 16-bit grey (terrain heightmaps).
    G16,
    /// 11
    Rrrgggbbb,
    /// 12
    Rgb565,
    /// 13: 4-bit palette index.
    P4,
    /// Value outside the enum.
    Unknown(u8),
}

impl TextureFormat {
    /// From the `Format` byte property.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::P8,
            1 => Self::Rgba7,
            2 => Self::NoUsed1,
            3 => Self::Dxt1,
            4 => Self::Rgb8,
            5 => Self::Rgba8,
            6 => Self::NoData,
            7 => Self::Dxt3,
            8 => Self::Dxt5,
            9 => Self::G8,
            10 => Self::G16,
            11 => Self::Rrrgggbbb,
            12 => Self::Rgb565,
            13 => Self::P4,
            n => Self::Unknown(n),
        }
    }

    /// Enum name as in the game data.
    pub fn name(self) -> String {
        match self {
            Self::P8 => "TEXF_P8".into(),
            Self::Rgba7 => "TEXF_RGBA7".into(),
            Self::NoUsed1 => "TEXF_NoUsed1".into(),
            Self::Dxt1 => "TEXF_DXT1".into(),
            Self::Rgb8 => "TEXF_RGB8".into(),
            Self::Rgba8 => "TEXF_RGBA8".into(),
            Self::NoData => "TEXF_NODATA".into(),
            Self::Dxt3 => "TEXF_DXT3".into(),
            Self::Dxt5 => "TEXF_DXT5".into(),
            Self::G8 => "TEXF_G8".into(),
            Self::G16 => "TEXF_G16".into(),
            Self::Rrrgggbbb => "TEXF_RRRGGGBBB".into(),
            Self::Rgb565 => "TEXF_RGB565".into(),
            Self::P4 => "TEXF_P4".into(),
            Self::Unknown(n) => format!("unknown({n})"),
        }
    }

    /// True when an inline palette follows the mips.
    pub fn has_inline_palette(self) -> bool {
        matches!(self, Self::P8 | Self::P4)
    }

    /// Expected data size of one mip, if the format is understood.
    pub fn mip_bytes(self, w: u32, h: u32) -> Option<usize> {
        let (w, h) = (w as usize, h as usize);
        let blocks = w.div_ceil(4).max(1) * h.div_ceil(4).max(1);
        Some(match self {
            Self::P8 | Self::G8 => w * h,
            Self::P4 => (w * h).div_ceil(2),
            Self::G16 | Self::Rgb565 => w * h * 2,
            Self::Rgb8 => w * h * 3,
            Self::Rgba8 => w * h * 4,
            Self::Dxt1 => blocks * 8,
            Self::Dxt3 | Self::Dxt5 => blocks * 16,
            _ => return None,
        })
    }
}

/// One stored mip level.
#[derive(Debug, Clone, PartialEq)]
pub struct Mip {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// `UBits` (log2 width).
    pub ubits: u8,
    /// `VBits` (log2 height).
    pub vbits: u8,
    /// Raw pixel data (format-specific).
    pub data: Vec<u8>,
    /// Absolute file offset of the data.
    pub data_offset: usize,
}

/// Decoded texture metadata plus raw mips.
#[derive(Debug, Clone, PartialEq)]
pub struct Texture {
    /// Pixel format (property `Format`; absent means the class default, observed P8).
    pub format: TextureFormat,
    /// True when `Format` was absent from the property block.
    pub format_defaulted: bool,
    /// `USize` / `VSize` properties.
    pub size: [u32; 2],
    /// `bMasked`: palette index 0 is transparent.
    pub masked: bool,
    /// `bAlphaTexture`.
    pub alpha_texture: bool,
    /// U/V clamp modes (0 wrap, 1 clamp).
    pub clamp: [u8; 2],
    /// Stored mips, largest first.
    pub mips: Vec<Mip>,
    /// Inline palette (R, G, B, A as stored).
    pub palette: Option<Vec<[u8; 4]>>,
    /// The three bytes after the mips/palette (licensee >= 55); meaning unknown.
    pub trailing: Option<[u8; 3]>,
    /// Byte accounting.
    pub report: PayloadReport,
}

/// RGBA8 image (row-major, top row first, as stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

fn bad_prop(name: &'static str, reason: impl Into<String>) -> DecodeErrorKind {
    DecodeErrorKind::BadProperty {
        name,
        reason: reason.into(),
    }
}

/// Decodes an `Engine.Texture` export.
pub fn decode_texture(package: &Package, data: &[u8], export: usize) -> DecodeResult<Texture> {
    let licensee = package.summary().licensee;
    let props = read_properties(package, data, export, TEXTURE_CLASS)?;
    let p = Props::new(package, &props);
    let ctx = |e: DecodeError| e.in_export(package, export);
    let (format, format_defaulted) = match p.byte("Format") {
        Some(b) => (TextureFormat::from_byte(b), false),
        None if p.get("Format").is_some() => {
            return Err(ctx(DecodeError::new(bad_prop("Format", "not a byte"))));
        }
        None => (TextureFormat::P8, true),
    };
    let dim = |name: &'static str| -> DecodeResult<u32> {
        match p.int(name) {
            Some(v) if (1..=8192).contains(&v) => Ok(v as u32),
            Some(v) => Err(ctx(DecodeError::new(bad_prop(name, format!("{v}"))))),
            None => Err(ctx(DecodeError::new(DecodeErrorKind::MissingProperty(
                name,
            )))),
        }
    };
    let size = [dim("USize")?, dim("VSize")?];
    let masked = p.bool("bMasked").unwrap_or(false);
    let alpha_texture = p.bool("bAlphaTexture").unwrap_or(false);
    let clamp = [
        p.byte("UClampMode").unwrap_or(0),
        p.byte("VClampMode").unwrap_or(0),
    ];

    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let mip_count = r.count("mips", 4 + 1 + 10).map_err(ctx)?;
    let mut mips = Vec::with_capacity(mip_count);
    for _ in 0..mip_count {
        let (n, skip) = r.lazy_array_header("mip.data", 1).map_err(ctx)?;
        let data_offset = r.pos();
        let bytes = r
            .bytes(n)
            .map_err(|e| ctx(e.in_field("mip.data")))?
            .to_vec();
        r.expect_at("mip.data", skip).map_err(ctx)?;
        let w = r.i32().map_err(ctx)?;
        let h = r.i32().map_err(ctx)?;
        let ubits = r.u8().map_err(ctx)?;
        let vbits = r.u8().map_err(ctx)?;
        if !(1..=8192).contains(&w) || !(1..=8192).contains(&h) {
            return Err(ctx(DecodeError::at(
                DecodeErrorKind::Invalid(format!("mip size {w}x{h}")),
                r.pos(),
            )
            .in_field("mip.size")));
        }
        mips.push(Mip {
            width: w as u32,
            height: h as u32,
            ubits,
            vbits,
            data: bytes,
            data_offset,
        });
    }
    let palette = if format.has_inline_palette() && licensee >= 42 {
        Some(
            r.array("palette", 4, |r| {
                let b = r.bytes(4)?;
                Ok([b[0], b[1], b[2], b[3]])
            })
            .map_err(ctx)?,
        )
    } else {
        None
    };
    let trailing = if licensee >= 55 {
        let b = r.unknown("texture.trailing", 3).map_err(ctx)?;
        Some([b[0], b[1], b[2]])
    } else {
        None
    };
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(Texture {
        format,
        format_defaulted,
        size,
        masked,
        alpha_texture,
        clamp,
        mips,
        palette,
        trailing,
        report,
    })
}

/// Decoded `Engine.Palette`: 256 colours (R, G, B, A as stored).
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    /// Colours.
    pub colors: Vec<[u8; 4]>,
    /// Byte accounting.
    pub report: PayloadReport,
}

/// Decodes an `Engine.Palette` export (`TArray<FColor>`).
pub fn decode_palette(package: &Package, data: &[u8], export: usize) -> DecodeResult<Palette> {
    let props = read_properties(package, data, export, PALETTE_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let colors = r
        .array("colors", 4, |r| {
            let b = r.bytes(4)?;
            Ok([b[0], b[1], b[2], b[3]])
        })
        .map_err(ctx)?;
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(Palette { colors, report })
}

impl Texture {
    /// Decodes mip `level` to RGBA8. Fails for formats that are recognized but not
    /// implemented, or when the stored data size does not match the format.
    pub fn decode_mip(&self, level: usize) -> DecodeResult<RgbaImage> {
        let mip = self.mips.get(level).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Invalid(format!(
                "mip {level} of {}",
                self.mips.len()
            )))
        })?;
        decode_pixels(
            self.format,
            mip.width,
            mip.height,
            &mip.data,
            self.palette.as_deref(),
            self.masked,
        )
        .map_err(|mut e| {
            if e.offset.is_none() {
                e.offset = Some(mip.data_offset as u64);
            }
            e
        })
    }

    /// Decodes every stored mip.
    /// Decodes every stored mip that has data. Mips stored with zero data bytes (observed
    /// for the smallest 1x1 level of 9 DXT textures) are returned as `None`.
    pub fn decode_all(&self) -> DecodeResult<Vec<Option<RgbaImage>>> {
        (0..self.mips.len())
            .map(|i| {
                if self.mips[i].data.is_empty() {
                    Ok(None)
                } else {
                    self.decode_mip(i).map(Some)
                }
            })
            .collect()
    }

    /// Number of stored mips with zero data bytes.
    pub fn empty_mips(&self) -> usize {
        self.mips.iter().filter(|m| m.data.is_empty()).count()
    }
}

/// Converts raw texture data to RGBA8.
pub fn decode_pixels(
    format: TextureFormat,
    width: u32,
    height: u32,
    data: &[u8],
    palette: Option<&[[u8; 4]]>,
    masked: bool,
) -> DecodeResult<RgbaImage> {
    let expected = format.mip_bytes(width, height).ok_or_else(|| {
        DecodeError::new(DecodeErrorKind::Unsupported(format!(
            "texture format {}",
            format.name()
        )))
    })?;
    if data.len() != expected {
        return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
            "{} {width}x{height}: {} data bytes, expected {expected}",
            format.name(),
            data.len()
        ))));
    }
    let n = width as usize * height as usize;
    let mut px = vec![0u8; n * 4];
    match format {
        TextureFormat::P8 | TextureFormat::P4 => {
            let pal = palette.ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Invalid(
                    "paletted texture without palette".into(),
                ))
            })?;
            for i in 0..n {
                let idx = if format == TextureFormat::P8 {
                    data[i] as usize
                } else {
                    let b = data[i / 2];
                    (if i % 2 == 0 { b & 0x0f } else { b >> 4 }) as usize
                };
                let c = pal.get(idx).ok_or_else(|| {
                    DecodeError::new(DecodeErrorKind::Invalid(format!(
                        "palette index {idx} outside {} colours",
                        pal.len()
                    )))
                })?;
                // Stored alpha is not meaningful for opaque palettes (often 0); bMasked
                // makes index 0 transparent (UE1/UE2 rule, UModel UPalette::Serialize).
                let a = if masked && idx == 0 { 0 } else { 255 };
                px[i * 4..i * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], a]);
            }
        }
        TextureFormat::G8 => {
            for i in 0..n {
                px[i * 4..i * 4 + 4].copy_from_slice(&[data[i], data[i], data[i], 255]);
            }
        }
        TextureFormat::G16 => {
            for i in 0..n {
                let v = u16::from_le_bytes([data[2 * i], data[2 * i + 1]]);
                let g = (v >> 8) as u8;
                px[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        TextureFormat::Rgb565 => {
            for i in 0..n {
                let v = u16::from_le_bytes([data[2 * i], data[2 * i + 1]]);
                let c = rgb565(v);
                px[i * 4..i * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        TextureFormat::Rgb8 => {
            for i in 0..n {
                let s = &data[i * 3..i * 3 + 3];
                px[i * 4..i * 4 + 4].copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        }
        TextureFormat::Rgba8 => {
            for i in 0..n {
                let s = &data[i * 4..i * 4 + 4];
                px[i * 4..i * 4 + 4].copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        }
        TextureFormat::Dxt1 | TextureFormat::Dxt3 | TextureFormat::Dxt5 => {
            decode_bc(format, width as usize, height as usize, data, &mut px);
        }
        _ => {
            return Err(DecodeError::new(DecodeErrorKind::Unsupported(format!(
                "texture format {}",
                format.name()
            ))));
        }
    }
    Ok(RgbaImage {
        width,
        height,
        pixels: px,
    })
}

/// G16 heightmap samples (row-major) for terrain.
pub fn g16_samples(tex: &Texture, level: usize) -> DecodeResult<Vec<u16>> {
    if tex.format != TextureFormat::G16 {
        return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
            "expected TEXF_G16, found {}",
            tex.format.name()
        ))));
    }
    let mip = tex
        .mips
        .get(level)
        .ok_or_else(|| DecodeError::new(DecodeErrorKind::Invalid("missing mip".into())))?;
    let expected = mip.width as usize * mip.height as usize * 2;
    if mip.data.len() != expected {
        return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
            "G16 data {} bytes, expected {expected}",
            mip.data.len()
        ))));
    }
    Ok(mip
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect())
}

fn rgb565(v: u16) -> [u8; 3] {
    let r = ((v >> 11) & 31) as u32;
    let g = ((v >> 5) & 63) as u32;
    let b = (v & 31) as u32;
    [
        ((r * 255 + 15) / 31) as u8,
        ((g * 255 + 31) / 63) as u8,
        ((b * 255 + 15) / 31) as u8,
    ]
}

/// BC1/BC2/BC3 block decompression (standard S3TC rules; BC1 uses 3-colour + transparent
/// mode when `c0 <= c1`).
fn decode_bc(format: TextureFormat, w: usize, h: usize, data: &[u8], out: &mut [u8]) {
    let block_size = if format == TextureFormat::Dxt1 { 8 } else { 16 };
    let bw = w.div_ceil(4).max(1);
    let bh = h.div_ceil(4).max(1);
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * block_size;
            let block = &data[off..off + block_size];
            let (alpha_part, color_part) = if block_size == 16 {
                (&block[..8], &block[8..])
            } else {
                (&block[..0], block)
            };
            let c0 = u16::from_le_bytes([color_part[0], color_part[1]]);
            let c1 = u16::from_le_bytes([color_part[2], color_part[3]]);
            let bits =
                u32::from_le_bytes([color_part[4], color_part[5], color_part[6], color_part[7]]);
            let (p0, p1) = (rgb565(c0), rgb565(c1));
            let mut pal = [[0u8; 4]; 4];
            pal[0] = [p0[0], p0[1], p0[2], 255];
            pal[1] = [p1[0], p1[1], p1[2], 255];
            let mix = |a: u8, b: u8, wa: u32, wb: u32, d: u32| {
                ((a as u32 * wa + b as u32 * wb) / d) as u8
            };
            if c0 > c1 || block_size == 16 {
                for k in 0..3 {
                    pal[2][k] = mix(p0[k], p1[k], 2, 1, 3);
                    pal[3][k] = mix(p0[k], p1[k], 1, 2, 3);
                }
                pal[2][3] = 255;
                pal[3][3] = 255;
            } else {
                for k in 0..3 {
                    pal[2][k] = mix(p0[k], p1[k], 1, 1, 2);
                }
                pal[2][3] = 255;
                pal[3] = [0, 0, 0, 0];
            }
            // alpha
            let mut alpha = [255u8; 16];
            match format {
                TextureFormat::Dxt3 => {
                    for (i, a) in alpha.iter_mut().enumerate() {
                        let nib = (alpha_part[i / 2] >> ((i % 2) * 4)) & 0x0f;
                        *a = nib * 17;
                    }
                }
                TextureFormat::Dxt5 => {
                    let (a0, a1) = (alpha_part[0] as u32, alpha_part[1] as u32);
                    let mut ap = [0u32; 8];
                    ap[0] = a0;
                    ap[1] = a1;
                    if a0 > a1 {
                        for (i, v) in ap.iter_mut().enumerate().skip(2) {
                            let i = i as u32;
                            *v = ((8 - i) * a0 + (i - 1) * a1) / 7;
                        }
                    } else {
                        for (i, v) in ap.iter_mut().enumerate().take(6).skip(2) {
                            let i = i as u32;
                            *v = ((6 - i) * a0 + (i - 1) * a1) / 5;
                        }
                        ap[6] = 0;
                        ap[7] = 255;
                    }
                    let mut abits = 0u64;
                    for (i, b) in alpha_part[2..8].iter().enumerate() {
                        abits |= (*b as u64) << (8 * i);
                    }
                    for (i, a) in alpha.iter_mut().enumerate() {
                        *a = ap[((abits >> (3 * i)) & 7) as usize] as u8;
                    }
                }
                _ => {}
            }
            for py in 0..4 {
                for pxi in 0..4 {
                    let (x, y) = (bx * 4 + pxi, by * 4 + py);
                    if x >= w || y >= h {
                        continue;
                    }
                    let i = py * 4 + pxi;
                    let sel = ((bits >> (2 * i)) & 3) as usize;
                    let mut c = pal[sel];
                    if block_size == 16 {
                        c[3] = alpha[i];
                    }
                    let o = (y * w + x) * 4;
                    out[o..o + 4].copy_from_slice(&c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_enum_matches_engine_u() {
        assert_eq!(TextureFormat::from_byte(3), TextureFormat::Dxt1);
        assert_eq!(TextureFormat::from_byte(10), TextureFormat::G16);
        assert_eq!(TextureFormat::from_byte(13), TextureFormat::P4);
        assert_eq!(TextureFormat::from_byte(200), TextureFormat::Unknown(200));
        assert_eq!(TextureFormat::Dxt1.mip_bytes(64, 32), Some(1024));
        assert_eq!(TextureFormat::Dxt5.mip_bytes(64, 32), Some(2048));
        assert_eq!(TextureFormat::Dxt1.mip_bytes(1, 1), Some(8));
        assert_eq!(TextureFormat::Rgba7.mip_bytes(4, 4), None);
    }

    #[test]
    fn bc1_solid_and_transparent_modes() {
        // c0 = pure red (0xF800) > c1 = blue: 4-colour mode, all texels select c0.
        let block = [0x00, 0xF8, 0x1F, 0x00, 0, 0, 0, 0];
        let img = decode_pixels(TextureFormat::Dxt1, 4, 4, &block, None, false).unwrap();
        assert_eq!(&img.pixels[..4], &[255, 0, 0, 255]);
        // c0 <= c1 and selector 3 everywhere: transparent black.
        let block = [0x1F, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF];
        let img = decode_pixels(TextureFormat::Dxt1, 4, 4, &block, None, false).unwrap();
        assert_eq!(&img.pixels[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn bc3_alpha_interpolation() {
        let mut block = [0u8; 16];
        block[0] = 255; // a0
        block[1] = 0; // a1, 8-value mode
        // texel 0 index 1 (a1 = 0), texel 1 index 0 (a0 = 255)
        block[2] = 0b0000_0001;
        block[8] = 0x00;
        block[9] = 0xF8; // c0 red
        let img = decode_pixels(TextureFormat::Dxt5, 4, 4, &block, None, false).unwrap();
        assert_eq!(img.pixels[3], 0);
        assert_eq!(img.pixels[7], 255);
        assert_eq!(&img.pixels[..3], &[255, 0, 0]);
    }

    #[test]
    fn bgra_and_palette() {
        let img = decode_pixels(TextureFormat::Rgba8, 1, 1, &[1, 2, 3, 4], None, false).unwrap();
        assert_eq!(img.pixels, vec![3, 2, 1, 4]);
        let pal = vec![[10, 20, 30, 0], [40, 50, 60, 0]];
        let img = decode_pixels(TextureFormat::P8, 2, 1, &[0, 1], Some(&pal), true).unwrap();
        assert_eq!(img.pixels, vec![10, 20, 30, 0, 40, 50, 60, 255]);
        assert!(decode_pixels(TextureFormat::P8, 2, 1, &[0, 2], Some(&pal), false).is_err());
        assert!(decode_pixels(TextureFormat::P8, 2, 1, &[0], Some(&pal), false).is_err());
        assert!(decode_pixels(TextureFormat::Rgba7, 1, 1, &[0; 4], None, false).is_err());
    }

    use crate::common::test_package::{Builder, Bytes, parse};

    /// Synthetic DXT1 4x4 texture with one mip; `tweak` edits the native part.
    fn dxt1_package(licensee: u16, tweak: impl Fn(Bytes, usize) -> Bytes) -> (Vec<u8>, usize) {
        let mut b = Builder::new();
        b.licensee = licensee;
        let (f, u, v) = (b.name("Format"), b.name("USize"), b.name("VSize"));
        let props = Bytes::default()
            .c(f)
            .u8(0x01)
            .u8(3)
            .c(u)
            .u8(0x22)
            .i32(4)
            .c(v)
            .u8(0x22)
            .i32(4)
            .c(0);
        let make = |start: usize| {
            let native_start = start + props.len();
            let data_start = native_start + 1 + 4 + 1;
            let body = Bytes(props.0.clone())
                .c(1)
                .i32((data_start + 8) as i32)
                .c(8)
                .raw(&[0x00, 0xF8, 0x1F, 0x00, 0, 0, 0, 0])
                .i32(4)
                .i32(4)
                .u8(2)
                .u8(2);
            tweak(body, data_start)
        };
        let i = b.export("Texture", "T", make(0).0);
        let start = b.payload_offset(i);
        b.set_payload(i, make(start).0);
        (b.build(), i)
    }

    #[test]
    fn synthetic_texture_round_trip_and_trailing_bytes() {
        let (bytes, i) = dxt1_package(58, |b, _| b.raw(&[1, 2, 3]));
        let p = parse(&bytes);
        let t = decode_texture(&p, &bytes, i).unwrap();
        assert_eq!(t.format, TextureFormat::Dxt1);
        assert_eq!(t.trailing, Some([1, 2, 3]));
        assert_eq!(t.report.unknown_bytes(), 3);
        assert_eq!(&t.decode_mip(0).unwrap().pixels[..4], &[255, 0, 0, 255]);
        // licensee < 55 has no trailing bytes
        let (bytes, i) = dxt1_package(50, |b, _| b);
        let p = parse(&bytes);
        assert_eq!(decode_texture(&p, &bytes, i).unwrap().trailing, None);
    }

    #[test]
    fn synthetic_texture_rejects_bad_layouts() {
        // Missing trailing bytes for licensee 58: EOF inside the payload.
        let (bytes, i) = dxt1_package(58, |b, _| b);
        let p = parse(&bytes);
        assert!(decode_texture(&p, &bytes, i).is_err());
        // An extra byte is reported as an unconsumed tail with its offset.
        let (bytes, i) = dxt1_package(58, |b, _| b.raw(&[0, 0, 0, 9]));
        let p = parse(&bytes);
        let e = decode_texture(&p, &bytes, i).unwrap_err();
        assert!(
            matches!(e.kind, DecodeErrorKind::UnconsumedTail { bytes: 1 }),
            "{e}"
        );
        assert!(e.offset.is_some() && e.export == Some(i as u32));
        // A lazy-array skip offset that does not match the data end is rejected.
        let (bytes, i) = dxt1_package(58, |mut b, data_start| {
            let at = b.0.len() - 10 - 8 - 1 - 4;
            b.0[at..at + 4].copy_from_slice(&((data_start + 7) as i32).to_le_bytes());
            b.raw(&[0, 0, 0])
        });
        let p = parse(&bytes);
        assert!(decode_texture(&p, &bytes, i).is_err());
    }
}
