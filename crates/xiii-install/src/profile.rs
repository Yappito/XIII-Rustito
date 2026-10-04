//! Content-profile detection and per-profile search roots.
//!
//! Detection is based only on measured layout facts (directory names compared
//! case-insensitively, package files present, patch credit text), never on the
//! installation folder's display name or a store manifest outside the root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;

use crate::PackageKind;
use crate::walk::Inventory;

/// Which known content layout an installation matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentProfile {
    /// GOG retail layout: platform subdirectories `system/PC` (code packages
    /// named by `SpecificPackage`) and `TexturesPC`, maps directly in `Maps`.
    Gog,
    /// Steam "XIII - Classic" as found locally: flattened `System` and
    /// `Textures`, maps split into `Maps/BaseSP` and `Maps/BaseMP`, plus
    /// unofficial patch 1.4 markers (`*Plus` packages, credits file).
    SteamPatched,
    /// Signals were missing or contradictory; see
    /// [`ProfileDetection::unknown_reasons`].
    Unknown,
}

impl std::fmt::Display for ContentProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Gog => "gog",
            Self::SteamPatched => "steam-patched",
            Self::Unknown => "unknown",
        })
    }
}

/// A measured layout signal used for profile detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// `system/PC` contains `.u` packages (GOG).
    PlatformCodeDir,
    /// `TexturesPC` contains `.utx` packages (GOG).
    PlatformTextureDir,
    /// `Textures` contains `.utx` packages (flattened layout).
    FlatTextureDir,
    /// `Maps/BaseSP` or `Maps/BaseMP` contain `.unr` maps (flattened layout).
    SplitMapDirs,
    /// `Maps` itself contains `.unr` maps (GOG; informational only).
    RootMapDir,
    /// `<Name>Plus` packages next to a `<Name>` base package (patch marker).
    PlusPackages,
    /// `Patch 1.4 Credits.txt` (patch marker).
    PatchCredits,
}

/// One recorded piece of detection evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub signal: Signal,
    /// Human-readable detail with exact relative paths/counts.
    pub detail: String,
}

/// Outcome of profile detection, including everything it was based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDetection {
    pub profile: ContentProfile,
    pub evidence: Vec<Evidence>,
    /// Non-empty exactly when `profile == Unknown`.
    pub unknown_reasons: Vec<String>,
}

impl ProfileDetection {
    pub fn has(&self, signal: Signal) -> bool {
        self.evidence.iter().any(|e| e.signal == signal)
    }
}

/// One directory (relative to the root, lowercase, `/`-separated) whose
/// direct children with the given extension are indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchRootSpec {
    pub logical: &'static str,
    pub kind: PackageKind,
}

const fn r(logical: &'static str, kind: PackageKind) -> SearchRootSpec {
    SearchRootSpec { logical, kind }
}

use PackageKind as K;

/// GOG roots. `system` and `system/pc` are both code roots; the files are
/// disjoint in the measured install (no stem occurs in both), so no precedence
/// rule is applied. `MapsUser` (in `Paths=`) is user content and not indexed.
/// `StaticMeshes` and `TexturesPC` are absent from GOG `Paths=` but are where
/// the shipped `.usx`/`.utx` files live.
pub const GOG_SEARCH_ROOTS: &[SearchRootSpec] = &[
    r("system", K::Code),
    r("system/pc", K::Code),
    r("maps", K::Map),
    r("texturespc", K::Texture),
    r("staticmeshes", K::StaticMesh),
    r("sounds", K::Sound),
    r("animations", K::Animation),
    r("music", K::Music),
];

/// Patched Steam roots, matching its `Paths=` entries except `Skins` (user
/// content, empty locally).
pub const STEAM_PATCHED_SEARCH_ROOTS: &[SearchRootSpec] = &[
    r("system", K::Code),
    r("maps", K::Map),
    r("maps/basesp", K::Map),
    r("maps/basemp", K::Map),
    r("textures", K::Texture),
    r("staticmeshes", K::StaticMesh),
    r("sounds", K::Sound),
    r("animations", K::Animation),
    r("music", K::Music),
];

/// Unknown profile: the union of both, so every candidate is visible and any
/// cross-layout name collision surfaces as a duplicate diagnostic.
pub const UNKNOWN_SEARCH_ROOTS: &[SearchRootSpec] = &[
    r("system", K::Code),
    r("system/pc", K::Code),
    r("maps", K::Map),
    r("maps/basesp", K::Map),
    r("maps/basemp", K::Map),
    r("textures", K::Texture),
    r("texturespc", K::Texture),
    r("staticmeshes", K::StaticMesh),
    r("sounds", K::Sound),
    r("animations", K::Animation),
    r("music", K::Music),
];

/// Search roots used for a profile.
pub fn search_roots_for(profile: ContentProfile) -> &'static [SearchRootSpec] {
    match profile {
        ContentProfile::Gog => GOG_SEARCH_ROOTS,
        ContentProfile::SteamPatched => STEAM_PATCHED_SEARCH_ROOTS,
        ContentProfile::Unknown => UNKNOWN_SEARCH_ROOTS,
    }
}

const CREDITS_FILE: &str = "patch 1.4 credits.txt";
const CREDITS_MARKER: &str = "unofficial patch 1.4";

fn ext_of(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn stem_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_owned()
}

/// Detects the profile from the inventory. `root` is only used to read the
/// (small, bounded) patch credits text.
pub(crate) fn detect(root: &Path, inv: &Inventory) -> ProfileDetection {
    // (lowercase parent dir, lowercase ext) -> exact relative paths
    let mut by_dir: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    // lowercase package stem -> exact file names (all package extensions)
    let mut pkg_stems: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut credits: Vec<&str> = Vec::new();

    for f in &inv.files {
        let name = f.file_name();
        if name.eq_ignore_ascii_case(CREDITS_FILE) {
            credits.push(&f.relative);
        }
        let Some(ext) = ext_of(name) else { continue };
        if PackageKind::from_extension(&ext).is_some() {
            pkg_stems
                .entry(stem_of(name).to_ascii_lowercase())
                .or_default()
                .push(name);
            by_dir
                .entry((f.parent_key(), ext))
                .or_default()
                .push(&f.relative);
        }
    }

    let count = |dir: &str, ext: &str| {
        by_dir
            .get(&(dir.to_owned(), ext.to_owned()))
            .map_or(0, Vec::len)
    };
    let mut evidence = Vec::new();
    let mut push = |signal, detail: String| evidence.push(Evidence { signal, detail });

    let n = count("system/pc", "u");
    if n > 0 {
        push(
            Signal::PlatformCodeDir,
            format!("system/PC holds {n} .u packages"),
        );
    }
    let n = count("texturespc", "utx");
    if n > 0 {
        push(
            Signal::PlatformTextureDir,
            format!("TexturesPC holds {n} .utx packages"),
        );
    }
    let n = count("textures", "utx");
    if n > 0 {
        push(
            Signal::FlatTextureDir,
            format!("Textures holds {n} .utx packages"),
        );
    }
    let (sp, mp) = (count("maps/basesp", "unr"), count("maps/basemp", "unr"));
    if sp + mp > 0 {
        push(
            Signal::SplitMapDirs,
            format!("Maps/BaseSP holds {sp} and Maps/BaseMP holds {mp} .unr maps"),
        );
    }
    let n = count("maps", "unr");
    if n > 0 {
        push(
            Signal::RootMapDir,
            format!("Maps holds {n} .unr maps directly"),
        );
    }

    let mut plus: BTreeSet<String> = BTreeSet::new();
    for (stem, names) in &pkg_stems {
        if let Some(base) = stem.strip_suffix("plus")
            && !base.is_empty()
            && pkg_stems.contains_key(base)
        {
            for n in names {
                plus.insert((*n).to_owned());
            }
        }
    }
    if !plus.is_empty() {
        let list: Vec<_> = plus.into_iter().collect();
        push(
            Signal::PlusPackages,
            format!(
                "{} *Plus packages with base package: {}",
                list.len(),
                list.join(", ")
            ),
        );
    }

    for rel in &credits {
        let mut text = String::new();
        let marker = fs::File::open(root.join(rel))
            .and_then(|f| f.take(4096).read_to_string(&mut text))
            .map(|_| text.to_ascii_lowercase().contains(CREDITS_MARKER))
            .unwrap_or(false);
        let detail = if marker {
            format!("{rel} mentions \"{CREDITS_MARKER}\"")
        } else {
            format!("{rel} present (marker text not found)")
        };
        push(Signal::PatchCredits, detail);
    }

    classify(evidence)
}

fn classify(evidence: Vec<Evidence>) -> ProfileDetection {
    let has = |s| evidence.iter().any(|e: &Evidence| e.signal == s);
    let gog_code = has(Signal::PlatformCodeDir);
    let gog_tex = has(Signal::PlatformTextureDir);
    let any_gog = gog_code || gog_tex;
    let flat = has(Signal::FlatTextureDir) || has(Signal::SplitMapDirs);
    let patch = has(Signal::PlusPackages) || has(Signal::PatchCredits);

    let mut reasons = Vec::new();
    let profile = if gog_code && gog_tex && !flat && !patch {
        ContentProfile::Gog
    } else if flat && patch && !any_gog {
        ContentProfile::SteamPatched
    } else {
        if any_gog && flat {
            reasons.push(
                "mixed layout: platform subdirectories (system/PC or TexturesPC) and \
                 flattened directories (Textures or Maps/BaseSP/BaseMP) are both populated"
                    .to_owned(),
            );
        }
        if any_gog && !(gog_code && gog_tex) {
            reasons.push(
                "partial platform-subdirectory layout: only one of system/PC and TexturesPC \
                 holds packages"
                    .to_owned(),
            );
        }
        if any_gog && patch {
            reasons.push(
                "platform-subdirectory layout carries patch markers (*Plus packages or \
                 patch credits); no profile covers this combination"
                    .to_owned(),
            );
        }
        if flat && !patch && !any_gog {
            reasons.push(
                "flattened layout without unofficial-patch markers; not a measured profile"
                    .to_owned(),
            );
        }
        if !any_gog && !flat {
            reasons.push(
                "no layout signal recognised (neither system/PC + TexturesPC nor \
                 Textures / Maps/BaseSP / Maps/BaseMP hold packages)"
                    .to_owned(),
            );
        }
        ContentProfile::Unknown
    };
    ProfileDetection {
        profile,
        evidence,
        unknown_reasons: reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(signals: &[Signal]) -> Vec<Evidence> {
        signals
            .iter()
            .map(|&signal| Evidence {
                signal,
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn classification_table() {
        use ContentProfile::*;
        use Signal::*;
        let cases: &[(&[Signal], ContentProfile)] = &[
            (&[PlatformCodeDir, PlatformTextureDir, RootMapDir], Gog),
            (
                &[FlatTextureDir, SplitMapDirs, PlusPackages, PatchCredits],
                SteamPatched,
            ),
            (&[SplitMapDirs, PatchCredits], SteamPatched),
            (&[FlatTextureDir, SplitMapDirs], Unknown),
            (
                &[PlatformCodeDir, PlatformTextureDir, FlatTextureDir],
                Unknown,
            ),
            (
                &[PlatformCodeDir, PlatformTextureDir, PlusPackages],
                Unknown,
            ),
            (&[PlatformCodeDir, RootMapDir], Unknown),
            (&[RootMapDir], Unknown),
        ];
        for (signals, expected) in cases {
            let d = classify(ev(signals));
            assert_eq!(d.profile, *expected, "{signals:?}");
            assert_eq!(
                d.profile == Unknown,
                !d.unknown_reasons.is_empty(),
                "{signals:?}"
            );
        }
    }

    #[test]
    fn unknown_roots_are_union_of_profiles() {
        for spec in GOG_SEARCH_ROOTS.iter().chain(STEAM_PATCHED_SEARCH_ROOTS) {
            assert!(UNKNOWN_SEARCH_ROOTS.contains(spec), "{spec:?}");
        }
        for spec in UNKNOWN_SEARCH_ROOTS {
            assert!(
                GOG_SEARCH_ROOTS.contains(spec) || STEAM_PATCHED_SEARCH_ROOTS.contains(spec),
                "{spec:?}"
            );
            assert_eq!(spec.logical, spec.logical.to_ascii_lowercase());
        }
    }
}
