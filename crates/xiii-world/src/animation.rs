//! Decoded-animation provider for the script VM (`xiii_script::animation::AnimationData`).
//!
//! The VM knows an actor's animation comes from its `Mesh` (a `SkeletalMesh`, which carries a
//! default `MeshAnimation` reference) and from any `Actor.LinkSkelAnim(MeshAnimation)` links;
//! it passes each candidate source path (links first, then `Mesh`) to this provider. A
//! `SkeletalMesh` source is resolved through its decoded default animation, a `MeshAnimation`
//! source is decoded directly, and packages are loaded lazily through the shared
//! [`PackageCache`].
//!
//! An export or sequence that does not exist is `Ok(None)` (the VM reports it as an unknown
//! sequence); a **decode or resolution failure** — including the package named by a resolved
//! source not loading — is `Err` and is never reported as a missing sequence. Notifies
//! (time + function) come from the decoded `MeshAnimation`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use xiii_decode::skeletal::{
    MESH_ANIMATION_CLASS, RawMeshAnimation, SKELETAL_MESH_CLASS, decode_mesh_animation,
    decode_skeletal_mesh,
};
use xiii_script::animation::{AnimationData, SeqInfo};

use crate::PackageCache;

/// Maps a decoded `MeshAnimation` sequence to the VM's [`SeqInfo`] (case-insensitive name
/// lookup), or `Ok(None)` when the sequence name is unknown. A negative decoded frame count is
/// an explicit error, never silently clamped.
pub fn sequence_info(anim: &RawMeshAnimation, seq: &str) -> Result<Option<SeqInfo>, String> {
    let Some(s) = anim
        .sequences
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(seq))
    else {
        return Ok(None);
    };
    if s.num_frames < 0 {
        return Err(format!(
            "sequence {} has negative frame count {}",
            s.name, s.num_frames
        ));
    }
    Ok(Some(SeqInfo {
        frames: s.num_frames as u32,
        rate: s.rate,
        notifies: s
            .notifies
            .iter()
            .map(|n| (n.time, n.function.clone()))
            .collect(),
    }))
}

/// `xiii_script::AnimationData` over the real decoded map animation data.
///
/// Decoded animations are cached per source path (a `SkeletalMesh` and a `MeshAnimation` are
/// both keyed by their script object path). A failed lookup is cached too, so a missing source
/// is not retried on every animation native.
pub struct MapAnimationProvider {
    cache: PackageCache,
    anims: HashMap<String, Result<Option<Arc<RawMeshAnimation>>, String>>,
}

impl MapAnimationProvider {
    /// Opens an installation for lazy animation loading.
    pub fn open(game_dir: &Path) -> Result<Self, String> {
        Ok(Self::from_cache(PackageCache::open(game_dir)?))
    }

    /// Uses an already-open package cache (so a harness need not open the installation twice).
    pub fn from_cache(cache: PackageCache) -> Self {
        Self {
            cache,
            anims: HashMap::new(),
        }
    }

    /// Decoded animation behind `source`, resolving a `SkeletalMesh` through its default
    /// `MeshAnimation` reference. `Ok(None)` means the source does not exist in the install;
    /// `Err` means an object that does exist could not be decoded/resolved.
    fn load_animation(&mut self, source: &str) -> Result<Option<Arc<RawMeshAnimation>>, String> {
        let (package, path) = source.split_once('.').unwrap_or((source, ""));
        if path.is_empty() {
            return Ok(None);
        }
        // The source path comes from a resolved object reference, so the package it names should
        // exist and load: a failure here is an explicit resolution/decode error, not an unknown
        // sequence. An export path that is absent inside a loaded package stays `Ok(None)`.
        let pkg = self
            .cache
            .get(package)
            .map_err(|e| format!("{source}: package lookup: {e}"))?;
        let Some(idx) = (0..pkg.package.exports().len()).find(|&i| {
            pkg.package
                .object_path(xiii_package::ObjectRef::Export(i as u32))
                .is_some_and(|p| p.eq_ignore_ascii_case(path))
        }) else {
            return Ok(None);
        };
        let class = pkg.package.export_class_path(idx).unwrap_or("");
        if class.eq_ignore_ascii_case(SKELETAL_MESH_CLASS) {
            let mesh = decode_skeletal_mesh(&pkg.package, &pkg.data, idx)
                .map_err(|e| format!("{source}: skeletal mesh decode: {e}"))?;
            if mesh.animation.is_null() {
                return Ok(None);
            }
            let (target, anim_idx) = self
                .cache
                .resolve(&pkg, mesh.animation)
                .map_err(|e| format!("{source}: default animation reference: {e}"))?;
            let anim = decode_mesh_animation(&target.package, &target.data, anim_idx)
                .map_err(|e| format!("{source} -> {}: animation decode: {e}", target.name))?;
            Ok(Some(Arc::new(anim)))
        } else if class.eq_ignore_ascii_case(MESH_ANIMATION_CLASS) {
            let anim = decode_mesh_animation(&pkg.package, &pkg.data, idx)
                .map_err(|e| format!("{source}: animation decode: {e}"))?;
            Ok(Some(Arc::new(anim)))
        } else {
            // The source is an object but not an animation source.
            Ok(None)
        }
    }
}

impl AnimationData for MapAnimationProvider {
    fn sequence(&mut self, source: &str, seq: &str) -> Result<Option<SeqInfo>, String> {
        if source.is_empty() {
            return Ok(None);
        }
        let key = source.to_ascii_lowercase();
        if !self.anims.contains_key(&key) {
            let loaded = self.load_animation(source);
            self.anims.insert(key.clone(), loaded);
        }
        let anim = match self.anims.get(&key) {
            Some(Ok(anim)) => anim.clone(),
            Some(Err(e)) => return Err(e.clone()),
            None => unreachable!("inserted above"),
        };
        match anim {
            Some(anim) => sequence_info(&anim, seq),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_decode::skeletal::anim::{AnimNotify, AnimSeq, NamedBone};

    fn synthetic_anim() -> RawMeshAnimation {
        let seq = |name: &str, frames: i32, rate: f32, notifies: Vec<AnimNotify>| AnimSeq {
            name: name.to_owned(),
            groups: Vec::new(),
            start_frame: 0,
            num_frames: frames,
            notifies,
            rate,
        };
        RawMeshAnimation {
            version: 0,
            ref_bones: vec![NamedBone {
                name: "Root".into(),
                flags: 0,
                parent: 0,
            }],
            moves: Vec::new(),
            sequences: vec![
                seq(
                    "WaitNeutre",
                    40,
                    30.0,
                    vec![AnimNotify {
                        time: 0.5,
                        function: "FootStep".into(),
                    }],
                ),
                seq("Run", 10, 15.0, Vec::new()),
                seq("Empty", 0, 1.0, Vec::new()),
            ],
        }
    }

    #[test]
    fn sequence_info_maps_frames_rate_and_notifies() {
        let anim = synthetic_anim();
        let info = sequence_info(&anim, "WaitNeutre").unwrap().unwrap();
        assert_eq!(info.frames, 40);
        assert_eq!(info.rate, 30.0);
        assert_eq!(info.notifies, vec![(0.5, "FootStep".to_owned())]);
    }

    #[test]
    fn sequence_info_is_case_insensitive_and_missing_is_none() {
        let anim = synthetic_anim();
        assert_eq!(
            sequence_info(&anim, "waitneutre").unwrap().unwrap().frames,
            40
        );
        assert!(sequence_info(&anim, "NoSuchSequence").unwrap().is_none());
    }

    #[test]
    fn zero_frame_sequence_is_reported() {
        let anim = synthetic_anim();
        let info = sequence_info(&anim, "Empty").unwrap().unwrap();
        assert_eq!(info.frames, 0);
    }

    #[test]
    fn negative_frame_count_is_an_explicit_error() {
        let mut anim = synthetic_anim();
        anim.sequences[1].num_frames = -3;
        let e = sequence_info(&anim, "Run").unwrap_err();
        assert!(e.contains("negative"), "{e}");
    }

    /// Opt-in corpus check: the provider's answer for `XIIIM` (the soldier mesh) is byte-for-byte
    /// the decoded `MeshAnimation` values (frames, rate, notifies), not the diagnostic provider.
    #[test]
    fn gog_xiiim_sequences_match_the_decoded_mesh_animation() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let mesh_pkg = cache.get("xiiipersos").expect("xiiipersos");
        let mesh_idx = (0..mesh_pkg.package.exports().len())
            .find(|&i| {
                mesh_pkg
                    .package
                    .object_path(xiii_package::ObjectRef::Export(i as u32))
                    .is_some_and(|p| p.eq_ignore_ascii_case("XIIIM"))
            })
            .expect("XIIIM export");
        let mesh = xiii_decode::skeletal::decode_skeletal_mesh(
            &mesh_pkg.package,
            &mesh_pkg.data,
            mesh_idx,
        )
        .expect("decode XIIIM");
        assert!(!mesh.animation.is_null(), "XIIIM has a default animation");
        let (anim_pkg, anim_idx) = cache
            .resolve(&mesh_pkg, mesh.animation)
            .expect("resolve XIIIM default animation");
        let decoded = xiii_decode::skeletal::decode_mesh_animation(
            &anim_pkg.package,
            &anim_pkg.data,
            anim_idx,
        )
        .expect("decode default animation");
        let mut provider = MapAnimationProvider::from_cache(cache);
        let mut checked = 0;
        for seq in &decoded.sequences {
            let info = provider
                .sequence("xiiipersos.XIIIM", &seq.name)
                .expect("lookup")
                .unwrap_or_else(|| panic!("sequence {} not found through the mesh", seq.name));
            assert_eq!(info.frames as i32, seq.num_frames, "{} frames", seq.name);
            assert_eq!(info.rate, seq.rate, "{} rate", seq.name);
            let expected: Vec<(f32, String)> = seq
                .notifies
                .iter()
                .map(|n| (n.time, n.function.clone()))
                .collect();
            assert_eq!(info.notifies, expected, "{} notifies", seq.name);
            checked += 1;
            if checked >= 8 {
                break;
            }
        }
        assert!(checked > 0, "the decoded animation has sequences");
        println!(
            "XIIIM default animation {}: {checked} sequences match the decoded values",
            anim_pkg
                .package
                .object_path(xiii_package::ObjectRef::Export(anim_idx as u32))
                .unwrap_or("?")
        );
    }
}
