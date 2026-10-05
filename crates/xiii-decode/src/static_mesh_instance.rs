//! `Engine.StaticMeshInstance` payload (package version 100, licensee 58).
//!
//! A `StaticMeshActor` references one `Engine.StaticMeshInstance` export through its
//! `StaticMeshInstance` object property. That export holds the per-instance baked vertex
//! lighting, so it is per **placed actor**, not per mesh asset.
//!
//! ```text
//! after the tagged-property block:
//!   TArray<FColor> Colors                 // one entry per StaticMesh render vertex
//!   compact NumLights
//!   NumLights x {
//!     i16  LightIndex                     // index into the level's light list; -1 (0xffff) seen
//!     compact VisibilityByteCount         // == ceil(Colors.len() / 8): one bit per vertex
//!     u8[VisibilityByteCount] Visibility  // per-vertex light visibility bits
//!     i32  Unknown                        // measured values 0 and 1; meaning not established
//!   }
//! ```
//!
//! `FColor` on disk is the UE2/UE3 `B,G,R,A` memory layout (verified by the engine's
//! `FColor` union on little-endian platforms and independently by the Plage terrain, whose
//! decoded colours are warm sand only when read as BGRA). [`StaticMeshInstance::rgba`] swaps
//! the blue and red bytes.
//!
//! The layout was established from the GOG bytes; no reference reader covers this
//! native-only class. Every decoded instance consumes the payload exactly, and the
//! `VisibilityByteCount == ceil(Colors/8)` relation held for all 867 instances of
//! Plage00/Plage01/Banque01 (measured 2026-10-05).

use xiii_package::Package;

use crate::common::{
    DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport, read_properties,
};

/// Class path of static mesh instances.
pub const STATIC_MESH_INSTANCE_CLASS: &str = "Engine.StaticMeshInstance";

/// One per-light vertex visibility record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLight {
    /// Light index (not an object reference; `-1`/`0xffff` occurs).
    pub light: i16,
    /// Per-vertex visibility bits, one bit per render vertex.
    pub visibility: Vec<u8>,
    /// Trailing i32. Measured values are 0 and 1; semantics not established.
    pub unknown: i32,
}

/// Decoded static mesh instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticMeshInstance {
    /// Per-vertex colours as stored (BGRA byte order), one per render vertex.
    pub colors: Vec<[u8; 4]>,
    /// Per-light visibility records.
    pub lights: Vec<InstanceLight>,
    /// Byte accounting.
    pub report: PayloadReport,
}

impl StaticMeshInstance {
    /// Colour `i` converted to RGBA (blue and red swapped).
    pub fn rgba(&self, i: usize) -> [u8; 4] {
        let c = self.colors[i];
        [c[2], c[1], c[0], c[3]]
    }

    /// True when every colour is fully transparent (`[0,0,0,0]`): the actor has no baked
    /// lighting and must not be modulated to black. Measured: 167 of the 867 instances of the
    /// three opening maps store an all-zero colour array (and every light `unknown` is 0 there).
    pub fn is_unlit(&self) -> bool {
        self.colors.iter().all(|c| *c == [0, 0, 0, 0])
    }

    /// Fraction of stored colours whose 4th byte is not 255. The 4th byte is 255 for baked
    /// vertices and 0 for the all-zero (unlit) arrays.
    pub fn alpha_255(&self) -> usize {
        self.colors.iter().filter(|c| c[3] == 255).count()
    }
}

/// Decodes an `Engine.StaticMeshInstance` export exactly.
pub fn decode_static_mesh_instance(
    package: &Package,
    data: &[u8],
    export: usize,
) -> DecodeResult<StaticMeshInstance> {
    let props = read_properties(package, data, export, STATIC_MESH_INSTANCE_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let s = decode_body(&mut r).map_err(ctx)?;
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(StaticMeshInstance { report, ..s })
}

fn decode_body(r: &mut PayloadReader<'_>) -> DecodeResult<StaticMeshInstance> {
    let colors = r.array("colors", 4, |r| {
        let b = r.bytes(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    })?;
    let lights = r.array("lights", 7, |r| {
        let light = r.i16()?;
        let visibility = r.array("lights.visibility", 1, |r| r.u8())?;
        let unknown = r.i32()?;
        Ok(InstanceLight {
            light,
            visibility,
            unknown,
        })
    })?;
    // Cross-check that makes a misaligned read fail loudly: one visibility bit per colour.
    let expected = colors.len().div_ceil(8);
    for (i, l) in lights.iter().enumerate() {
        if l.visibility.len() != expected {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!(
                    "light {i} has {} visibility bytes for {} colours (expected {expected})",
                    l.visibility.len(),
                    colors.len()
                )),
                r.pos(),
            )
            .in_field("lights.visibility"));
        }
    }
    Ok(StaticMeshInstance {
        colors,
        lights,
        report: PayloadReport {
            payload: r.payload(),
            properties_end: 0,
            unknown: Vec::new(),
            unsupported_tail: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, Bytes, parse};

    /// Native payload: `colors` BGRA entries, then `lights` records `(i16, bits, i32)`.
    fn payload(colors: &[[u8; 4]], lights: &[(i16, Vec<u8>, i32)]) -> Vec<u8> {
        let mut b = Bytes::default().c(0); // empty tagged-property block (None terminator)
        b = b.c(colors.len() as i32);
        for c in colors {
            b = b.raw(c);
        }
        b = b.c(lights.len() as i32);
        for (light, bits, unknown) in lights {
            b = b.i16(*light).c(bits.len() as i32).raw(bits).i32(*unknown);
        }
        b.0
    }

    fn build(colors: &[[u8; 4]], lights: &[(i16, Vec<u8>, i32)]) -> (Vec<u8>, usize) {
        let mut b = Builder::new();
        let i = b.export("StaticMeshInstance", "I", payload(colors, lights));
        (b.build(), i)
    }

    #[test]
    fn synthetic_instance_decodes_exactly() {
        let colors = [
            [0x73, 0xa1, 0xc9, 0xff],
            [0x3c, 0x4c, 0x64, 0xff],
            [0, 0, 0, 0],
            [0x08, 0x10, 0x20, 0xff],
        ];
        let (bytes, i) = build(
            &colors,
            &[(0, vec![0b1010_0000], 1), (62, vec![0b0000_0001], 0)],
        );
        let p = parse(&bytes);
        let s = decode_static_mesh_instance(&p, &bytes, i).unwrap();
        assert_eq!(s.colors.len(), 4);
        assert_eq!(s.colors[0], [0x73, 0xa1, 0xc9, 0xff]);
        assert_eq!(s.rgba(0), [0xc9, 0xa1, 0x73, 0xff]);
        assert_eq!(s.lights.len(), 2);
        assert_eq!(s.lights[0].light, 0);
        assert_eq!(s.lights[0].visibility, vec![0b1010_0000]);
        assert_eq!(s.lights[1].unknown, 0);
        assert!(!s.is_unlit());
        assert!(s.report.unsupported_tail.is_none());
    }

    #[test]
    fn all_zero_colors_are_unlit() {
        let (bytes, i) = build(&[[0, 0, 0, 0], [0, 0, 0, 0]], &[]);
        let p = parse(&bytes);
        let s = decode_static_mesh_instance(&p, &bytes, i).unwrap();
        assert!(s.is_unlit());
        assert_eq!(s.alpha_255(), 0);
    }

    #[test]
    fn wrong_visibility_length_fails() {
        let colors = [[1, 2, 3, 255]; 16]; // 2 visibility bytes expected
        let (bytes, i) = build(&colors, &[(0, vec![0xff], 1)]);
        let p = parse(&bytes);
        let e = decode_static_mesh_instance(&p, &bytes, i).unwrap_err();
        assert!(matches!(e.kind, DecodeErrorKind::Invalid(_)), "{e}");
        assert_eq!(e.field, Some("lights.visibility"));
    }

    #[test]
    fn truncation_fails_cleanly() {
        let colors = [[1, 2, 3, 255]; 4];
        let full = payload(&colors, &[(0, vec![0x0f], 1)]);
        for cut in 1..full.len() {
            let mut b = Builder::new();
            let i = b.export("StaticMeshInstance", "I", full[..full.len() - cut].to_vec());
            let bytes = b.build();
            let p = parse(&bytes);
            assert!(
                decode_static_mesh_instance(&p, &bytes, i).is_err(),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn wrong_class_fails() {
        let mut b = Builder::new();
        let i = b.export("StaticMesh", "M", vec![0]);
        let bytes = b.build();
        let p = parse(&bytes);
        let e = decode_static_mesh_instance(&p, &bytes, i).unwrap_err();
        assert!(matches!(e.kind, DecodeErrorKind::WrongClass { .. }), "{e}");
    }
}

/// Opt-in corpus decode (`XIII_GOG_DIR`); prints `SKIPPED` otherwise.
#[cfg(test)]
mod local_tests {
    use super::*;
    use xiii_package::Limits;

    fn gog_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        Some(if path.is_relative() {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(path)
        } else {
            path
        })
    }

    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, out);
            } else if let Ok(mut f) = std::fs::File::open(&p) {
                use std::io::Read as _;
                let mut b = [0u8; 4];
                if f.read_exact(&mut b).is_ok() && xiii_package::has_package_tag(&b) {
                    out.push(p);
                }
            }
        }
    }

    /// Every `Engine.StaticMeshInstance` export of the installation decodes exactly (including
    /// the `VisibilityByteCount == ceil(Colors/8)` cross-check) and the corpus contains
    /// instances with baked colours and with the all-zero unlit array.
    #[test]
    fn corpus_static_mesh_instances_decode() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to run the StaticMeshInstance corpus test");
            return;
        };
        let mut files = Vec::new();
        collect(&root, &mut files);
        files.sort();
        let (mut instances, mut zero, mut lights, mut failures) = (0u64, 0u64, 0u64, 0u64);
        for f in &files {
            let Ok(data) = std::fs::read(f) else { continue };
            let Ok(p) = xiii_package::Package::parse(&data, &Limits::default()) else {
                continue;
            };
            for i in 0..p.exports().len() {
                if p.exports()[i].serial_size == 0 {
                    continue;
                }
                if !p
                    .export_class_path(i)
                    .is_some_and(|c| c.eq_ignore_ascii_case(STATIC_MESH_INSTANCE_CLASS))
                {
                    continue;
                }
                match decode_static_mesh_instance(&p, &data, i) {
                    Ok(s) => {
                        instances += 1;
                        lights += s.lights.len() as u64;
                        if s.is_unlit() {
                            zero += 1;
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("[lighting] decode failure {}: {e}", f.display());
                    }
                }
            }
        }
        assert!(files.len() >= 100, "too few packages: {}", files.len());
        assert_eq!(failures, 0, "decode failures");
        assert!(instances > 0, "no StaticMeshInstance exports found");
        assert!(
            zero > 0,
            "expected all-zero (unlit) instances in the corpus"
        );
        assert!(zero < instances, "expected some lit instances");
        println!(
            "[lighting] packages {} StaticMeshInstance {} (unlit {}) lights {} failures {}",
            files.len(),
            instances,
            zero,
            lights,
            failures
        );
    }
}
