//! Read-only installation resolution. Never writes into a game installation.
//!
//! One [`Installation`] is exactly one root directory. It is validated by
//! content, inventoried with a bounded walk that never follows links, matched
//! against a measured [`ContentProfile`], and turned into a case-insensitive
//! logical package index that keeps exact filesystem paths. Name collisions are
//! reported as diagnostics and make resolution fail with
//! [`ResolveError::Ambiguous`]; no ordering rule ever picks a winner.

mod discovery;
mod ini;
mod profile;
mod walk;

#[cfg(test)]
mod test_support;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use discovery::{
    Candidate, CandidateSource, DiscoveryRoots, Kv, STEAM_APP_ID, discover_candidates,
    discover_candidates_in, parse_vdf, probe, steam_install_dir, steam_libraries,
};
pub use ini::{
    CoreSystemFields, INI_FILE_NAMES, IniEvidence, IniPathCheck, MAX_INI_BYTES,
    SpecificPackageCheck, parse_core_system, resolve_ini_path,
};
pub use profile::{
    ContentProfile, Evidence, GOG_SEARCH_ROOTS, ProfileDetection, STEAM_PATCHED_SEARCH_ROOTS,
    SearchRootSpec, Signal, UNKNOWN_SEARCH_ROOTS, search_roots_for,
};
pub use walk::{EXCLUDED_DIR_NAMES, EXCLUDED_FILE_EXTENSIONS, Inventory, InventoryFile};

/// First four bytes of every Unreal package (`0x9E2A83C1` little-endian).
pub const PACKAGE_MAGIC: [u8; 4] = [0xC1, 0x83, 0x2A, 0x9E];

/// Returns true if the file at `path` starts with [`PACKAGE_MAGIC`].
pub fn has_package_magic(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok_and(|()| buf == PACKAGE_MAGIC)
}

/// Package class, determined by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackageKind {
    /// `.u` script/code packages (may also hold meshes and textures).
    Code,
    /// `.unr` maps.
    Map,
    /// `.utx` textures.
    Texture,
    /// `.usx` static meshes.
    StaticMesh,
    /// `.uax` sounds.
    Sound,
    /// `.ukx` animations (none shipped; listed in `Paths=`).
    Animation,
    /// `.umx` music (none shipped; listed in `Paths=`).
    Music,
}

impl PackageKind {
    pub const ALL: [Self; 7] = [
        Self::Code,
        Self::Map,
        Self::Texture,
        Self::StaticMesh,
        Self::Sound,
        Self::Animation,
        Self::Music,
    ];

    /// Lowercase extension without the dot.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Code => "u",
            Self::Map => "unr",
            Self::Texture => "utx",
            Self::StaticMesh => "usx",
            Self::Sound => "uax",
            Self::Animation => "ukx",
            Self::Music => "umx",
        }
    }

    /// Case-insensitive lookup by extension (without the dot).
    pub fn from_extension(ext: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.extension().eq_ignore_ascii_case(ext))
    }
}

impl fmt::Display for PackageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ".{}", self.extension())
    }
}

/// Limits and checks applied by [`Installation::open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOptions {
    /// Maximum directory depth below the root that is entered.
    pub max_depth: usize,
    /// Maximum number of directory entries visited before the walk stops.
    pub max_entries: usize,
    /// Read the first four bytes of every candidate package and exclude files
    /// without [`PACKAGE_MAGIC`] (reported as diagnostics).
    pub verify_package_magic: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        // Measured installs: depth 2 (`Maps/BaseSP`, `system/PC`), ~720 files.
        Self {
            max_depth: 6,
            max_entries: 50_000,
            verify_package_magic: true,
        }
    }
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
}

/// Machine-readable diagnostic category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCode {
    /// The same logical package name was found at more than one path.
    DuplicateLogicalPackage,
    /// Several directories differ only by case (case-sensitive filesystems).
    CaseVariantDirectories,
    /// A file with a package extension in a search root lacks the magic.
    BadPackageMagic,
    /// A file or directory could not be read.
    Unreadable,
    /// A package-extension file outside the active search roots.
    UnindexedPackageFile,
    /// A symbolic link or junction was not followed.
    LinkNotFollowed,
    /// An entry with a non-UTF-8 name was skipped.
    NonUtf8Name,
    /// The walk stopped at the entry limit; the index may be incomplete.
    WalkTruncated,
    /// Directories beyond the depth limit were not entered.
    DepthLimited,
    /// A `SpecificPackage=` name was not found in the index.
    SpecificPackageMissing,
    /// An ini `Paths=` directory exists but is not an active search root.
    IniPathNotIndexed,
}

/// One diagnostic produced while opening an installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
    /// Relative paths (exact spelling) the diagnostic refers to.
    pub paths: Vec<String>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}: {}", self.severity, self.code, self.message)?;
        if !self.paths.is_empty() {
            write!(f, " [{}]", self.paths.join(", "))?;
        }
        Ok(())
    }
}

/// An active search root: a [`SearchRootSpec`] matched to a real directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRoot {
    /// Lowercase logical directory from the profile table.
    pub logical: &'static str,
    pub kind: PackageKind,
    /// Exact relative directory spelling on disk.
    pub relative: String,
    /// Number of packages indexed from this root.
    pub packages: usize,
}

/// One indexed package file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    /// Lowercase package stem used as the index key.
    pub key: String,
    /// File stem with its exact on-disk spelling.
    pub name: String,
    pub kind: PackageKind,
    /// Relative path with exact spelling, `/`-separated.
    pub relative: String,
    /// Absolute path (root joined with `relative`).
    pub path: PathBuf,
    /// Logical search root the file was indexed from.
    pub search_root: &'static str,
    pub size: u64,
}

/// Successful resolution of a logical package name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    /// The name as requested.
    pub requested: String,
    pub entry: PackageEntry,
    pub profile: ContentProfile,
    /// Root of the installation that produced this result.
    pub root: PathBuf,
}

impl ResolvedPackage {
    pub fn path(&self) -> &Path {
        &self.entry.path
    }
}

/// Why a name did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Empty, path-like, or with an unsupported extension/extra dots.
    InvalidName { name: String, reason: &'static str },
    /// No indexed package has this logical name.
    NotFound {
        name: String,
        kind: Option<PackageKind>,
    },
    /// The name exists, but only as other package kinds.
    WrongKind {
        name: String,
        expected: PackageKind,
        found: Vec<(PackageKind, String)>,
    },
    /// More than one file carries this logical name (see diagnostics).
    Ambiguous {
        name: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, reason } => {
                write!(f, "invalid package name {name:?}: {reason}")
            }
            Self::NotFound { name, kind: None } => write!(f, "package {name:?} not found"),
            Self::NotFound {
                name,
                kind: Some(k),
            } => write!(f, "{k} package {name:?} not found"),
            Self::WrongKind {
                name,
                expected,
                found,
            } => {
                let found: Vec<_> = found.iter().map(|(_, p)| p.as_str()).collect();
                write!(
                    f,
                    "{name:?} is not a {expected} package; found {}",
                    found.join(", ")
                )
            }
            Self::Ambiguous { name, candidates } => write!(
                f,
                "package {name:?} is ambiguous; candidates: {}",
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Why a root could not be opened.
#[derive(Debug)]
pub enum OpenError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    NotADirectory(PathBuf),
    /// Content checks failed; reasons list each missing requirement.
    NotAnInstallation {
        root: PathBuf,
        reasons: Vec<String>,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::NotADirectory(p) => write!(f, "{} is not a directory", p.display()),
            Self::NotAnInstallation { root, reasons } => write!(
                f,
                "{} is not a XIII installation: {}",
                root.display(),
                reasons.join("; ")
            ),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One validated, read-only XIII installation.
#[derive(Debug, Clone)]
pub struct Installation {
    root: PathBuf,
    detection: ProfileDetection,
    inventory: Inventory,
    search_roots: Vec<SearchRoot>,
    index: BTreeMap<String, Vec<PackageEntry>>,
    diagnostics: Vec<Diagnostic>,
    ini: Vec<IniEvidence>,
    specific_packages: Vec<SpecificPackageCheck>,
}

/// Code packages whose presence (with package magic) directly in a `system`
/// directory identifies an Unreal Engine 2 installation root. Both measured
/// layouts keep them there (`system/core.u` on GOG, `System/Core.u` on Steam).
pub const REQUIRED_CODE_PACKAGES: &[&str] = &["core.u", "engine.u"];

fn lower_ext(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

impl Installation {
    /// Opens `root` as one installation. Nothing is written anywhere.
    pub fn open(root: impl AsRef<Path>, options: &OpenOptions) -> Result<Self, OpenError> {
        let given = root.as_ref();
        let root = std::path::absolute(given).map_err(|source| OpenError::Io {
            path: given.to_path_buf(),
            source,
        })?;
        let meta = fs::metadata(&root).map_err(|source| OpenError::Io {
            path: root.clone(),
            source,
        })?;
        if !meta.is_dir() {
            return Err(OpenError::NotADirectory(root));
        }
        validate_code_root(&root)?;

        let inventory = walk::walk(
            &root,
            &walk::WalkLimits {
                max_depth: options.max_depth,
                max_entries: options.max_entries,
            },
        );
        let detection = profile::detect(&root, &inventory);
        let mut inst = Self {
            root,
            detection,
            inventory,
            search_roots: Vec::new(),
            index: BTreeMap::new(),
            diagnostics: Vec::new(),
            ini: Vec::new(),
            specific_packages: Vec::new(),
        };
        inst.walk_diagnostics();
        inst.build_index(options.verify_package_magic);
        if !inst
            .index
            .values()
            .flatten()
            .any(|e| e.kind == PackageKind::Map)
        {
            return Err(OpenError::NotAnInstallation {
                root: inst.root,
                reasons: vec![format!(
                    "no .unr map package in any {} map search root",
                    inst.detection.profile
                )],
            });
        }
        inst.read_ini();
        Ok(inst)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profile(&self) -> ContentProfile {
        self.detection.profile
    }

    pub fn detection(&self) -> &ProfileDetection {
        &self.detection
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    /// Active search roots in profile-table order.
    pub fn search_roots(&self) -> &[SearchRoot] {
        &self.search_roots
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Ini evidence (`Default.ini`, `XIII.ini`) found in code roots.
    pub fn ini_evidence(&self) -> &[IniEvidence] {
        &self.ini
    }

    /// Location of every `SpecificPackage=` name across the ini files.
    pub fn specific_packages(&self) -> &[SpecificPackageCheck] {
        &self.specific_packages
    }

    /// All indexed entries, ordered by key.
    pub fn packages(&self) -> impl Iterator<Item = &PackageEntry> {
        self.index.values().flatten()
    }

    /// Number of distinct logical names.
    pub fn logical_name_count(&self) -> usize {
        self.index.len()
    }

    /// Logical names with more than one candidate file.
    pub fn conflicts(&self) -> impl Iterator<Item = (&str, &[PackageEntry])> {
        self.index
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Resolves a logical package name, case-insensitively. A known package
    /// extension (`Plage00.unr`) restricts the kind.
    pub fn resolve_package(&self, name: &str) -> Result<ResolvedPackage, ResolveError> {
        self.resolve_inner(name, None)
    }

    /// Resolves a map name (`Plage00` or `Plage00.unr`).
    pub fn resolve_map(&self, name: &str) -> Result<ResolvedPackage, ResolveError> {
        self.resolve_inner(name, Some(PackageKind::Map))
    }

    /// Resolves a name that must be of `kind`.
    pub fn resolve_package_of_kind(
        &self,
        name: &str,
        kind: PackageKind,
    ) -> Result<ResolvedPackage, ResolveError> {
        self.resolve_inner(name, Some(kind))
    }

    fn resolve_inner(
        &self,
        name: &str,
        kind: Option<PackageKind>,
    ) -> Result<ResolvedPackage, ResolveError> {
        let invalid = |reason| ResolveError::InvalidName {
            name: name.to_owned(),
            reason,
        };
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(invalid("empty"));
        }
        if trimmed.contains(['/', '\\', ':', '\0']) {
            return Err(invalid("path separators are not allowed"));
        }
        let (stem_part, ext_kind) = match trimmed.rsplit_once('.') {
            Some((s, ext)) => match PackageKind::from_extension(ext) {
                Some(k) => (s, Some(k)),
                None => return Err(invalid("unknown package extension or dotted object path")),
            },
            None => (trimmed, None),
        };
        if stem_part.is_empty() || stem_part.contains('.') {
            return Err(invalid("package names contain no dots"));
        }
        let kind = match (kind, ext_kind) {
            (Some(a), Some(b)) if a != b => {
                return Err(invalid("extension contradicts requested kind"));
            }
            (a, b) => a.or(b),
        };
        let key = stem_part.to_ascii_lowercase();
        let Some(entries) = self.index.get(&key) else {
            return Err(ResolveError::NotFound {
                name: name.to_owned(),
                kind,
            });
        };
        // UE2 package names share one namespace regardless of extension, so a
        // collision blocks every lookup of that name, kind filter or not.
        if entries.len() > 1 {
            return Err(ResolveError::Ambiguous {
                name: name.to_owned(),
                candidates: entries.iter().map(|e| e.relative.clone()).collect(),
            });
        }
        let entry = &entries[0];
        if let Some(k) = kind
            && entry.kind != k
        {
            return Err(ResolveError::WrongKind {
                name: name.to_owned(),
                expected: k,
                found: vec![(entry.kind, entry.relative.clone())],
            });
        }
        Ok(ResolvedPackage {
            requested: name.to_owned(),
            entry: entry.clone(),
            profile: self.detection.profile,
            root: self.root.clone(),
        })
    }

    fn diag(
        &mut self,
        severity: Severity,
        code: DiagnosticCode,
        message: String,
        paths: Vec<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            severity,
            code,
            message,
            paths,
        });
    }

    fn walk_diagnostics(&mut self) {
        let inv = &self.inventory;
        let mut pending = Vec::new();
        if inv.truncated {
            pending.push((
                Severity::Warning,
                DiagnosticCode::WalkTruncated,
                "entry limit reached; inventory and index are incomplete".to_owned(),
                Vec::new(),
            ));
        }
        if !inv.depth_limited.is_empty() {
            pending.push((
                Severity::Info,
                DiagnosticCode::DepthLimited,
                "directories beyond the depth limit were not entered".to_owned(),
                inv.depth_limited.clone(),
            ));
        }
        if !inv.skipped_links.is_empty() {
            pending.push((
                Severity::Warning,
                DiagnosticCode::LinkNotFollowed,
                "symbolic links / junctions are not followed".to_owned(),
                inv.skipped_links.clone(),
            ));
        }
        if !inv.skipped_non_utf8.is_empty() {
            pending.push((
                Severity::Warning,
                DiagnosticCode::NonUtf8Name,
                "entries with non-UTF-8 names were skipped".to_owned(),
                inv.skipped_non_utf8
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            ));
        }
        for (path, err) in &inv.unreadable {
            pending.push((
                Severity::Warning,
                DiagnosticCode::Unreadable,
                err.clone(),
                vec![path.clone()],
            ));
        }
        for (s, c, m, p) in pending {
            self.diag(s, c, m, p);
        }
    }

    fn build_index(&mut self, verify_magic: bool) {
        // lowercase dir -> exact spellings
        let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for d in &self.inventory.directories {
            dirs.entry(d.to_ascii_lowercase())
                .or_default()
                .push(d.clone());
        }
        let mut case_variants = Vec::new();
        for spellings in dirs.values().filter(|v| v.len() > 1) {
            case_variants.push(spellings.clone());
        }
        for v in case_variants {
            self.diag(
                Severity::Warning,
                DiagnosticCode::CaseVariantDirectories,
                "directories differ only by case; all are indexed".to_owned(),
                v,
            );
        }

        let specs = search_roots_for(self.detection.profile);
        let mut root_of_dir: BTreeMap<String, (usize, &'static SearchRootSpec)> = BTreeMap::new();
        for spec in specs {
            for actual in dirs.get(spec.logical).into_iter().flatten() {
                root_of_dir.insert(actual.clone(), (self.search_roots.len(), spec));
                self.search_roots.push(SearchRoot {
                    logical: spec.logical,
                    kind: spec.kind,
                    relative: actual.clone(),
                    packages: 0,
                });
            }
        }

        let mut unindexed = Vec::new();
        let mut bad_magic = Vec::new();
        let files = std::mem::take(&mut self.inventory.files);
        for f in &files {
            let name = f.file_name();
            let Some(kind) = lower_ext(name).and_then(|e| PackageKind::from_extension(&e)) else {
                continue;
            };
            let parent = match f.relative.rfind('/') {
                Some(i) => &f.relative[..i],
                None => "",
            };
            let Some(&(root_idx, spec)) = root_of_dir.get(parent).filter(|(_, s)| s.kind == kind)
            else {
                unindexed.push(f.relative.clone());
                continue;
            };
            let path = self.root.join(&f.relative);
            if verify_magic && !has_package_magic(&path) {
                bad_magic.push(f.relative.clone());
                continue;
            }
            self.search_roots[root_idx].packages += 1;
            let name = stem(name).to_owned();
            let key = name.to_ascii_lowercase();
            self.index
                .entry(key.clone())
                .or_default()
                .push(PackageEntry {
                    key,
                    name,
                    kind,
                    relative: f.relative.clone(),
                    path,
                    search_root: spec.logical,
                    size: f.size,
                });
        }
        self.inventory.files = files;

        if !unindexed.is_empty() {
            self.diag(
                Severity::Info,
                DiagnosticCode::UnindexedPackageFile,
                format!(
                    "package-extension files outside the {} search roots are not indexed",
                    self.detection.profile
                ),
                unindexed,
            );
        }
        if !bad_magic.is_empty() {
            self.diag(
                Severity::Warning,
                DiagnosticCode::BadPackageMagic,
                "files in search roots without the package magic were excluded".to_owned(),
                bad_magic,
            );
        }
        let dups: Vec<(String, Vec<String>)> = self
            .conflicts()
            .map(|(k, v)| (k.to_owned(), v.iter().map(|e| e.relative.clone()).collect()))
            .collect();
        for (key, paths) in dups {
            self.diag(
                Severity::Warning,
                DiagnosticCode::DuplicateLogicalPackage,
                format!(
                    "logical package {key:?} has {} candidates; resolution refuses it",
                    paths.len()
                ),
                paths,
            );
        }
    }

    fn read_ini(&mut self) {
        let dir_keys: BTreeSet<String> = self
            .inventory
            .directories
            .iter()
            .map(|d| d.to_ascii_lowercase())
            .collect();
        let root_keys: BTreeSet<&'static str> =
            self.search_roots.iter().map(|r| r.logical).collect();
        let code_dirs: Vec<String> = self
            .search_roots
            .iter()
            .filter(|r| r.logical == "system")
            .map(|r| r.relative.clone())
            .collect();

        for code_dir in code_dirs {
            let code_key = code_dir.to_ascii_lowercase();
            for f in &self.inventory.files {
                if f.parent_key() != code_key
                    || !INI_FILE_NAMES
                        .iter()
                        .any(|n| f.file_name().eq_ignore_ascii_case(n))
                {
                    continue;
                }
                let Ok(text) = ini::read_bounded(&self.root.join(&f.relative)) else {
                    continue;
                };
                let fields = parse_core_system(&text);
                let path_checks: Vec<IniPathCheck> = fields
                    .paths
                    .iter()
                    .map(|raw| {
                        let (resolved_dir, extension) = resolve_ini_path(&code_key, raw);
                        let dir_exists =
                            resolved_dir.as_ref().is_some_and(|d| dir_keys.contains(d));
                        let is_search_root = resolved_dir
                            .as_deref()
                            .is_some_and(|d| root_keys.contains(d));
                        IniPathCheck {
                            raw: raw.clone(),
                            resolved_dir,
                            extension,
                            dir_exists,
                            is_search_root,
                        }
                    })
                    .collect();
                let covered: BTreeSet<&str> = path_checks
                    .iter()
                    .filter_map(|c| c.resolved_dir.as_deref())
                    .collect();
                let search_roots_not_in_paths = root_keys
                    .iter()
                    .filter(|r| !covered.contains(*r))
                    .map(|r| (*r).to_owned())
                    .collect();
                self.ini.push(IniEvidence {
                    file: f.relative.clone(),
                    fields,
                    path_checks,
                    search_roots_not_in_paths,
                });
            }
        }

        let mut not_indexed = Vec::new();
        for ev in &self.ini {
            for c in &ev.path_checks {
                if c.dir_exists && !c.is_search_root {
                    not_indexed.push(format!("{}: {}", ev.file, c.raw));
                }
            }
        }
        if !not_indexed.is_empty() {
            self.diag(
                Severity::Info,
                DiagnosticCode::IniPathNotIndexed,
                "ini Paths= directories that exist but are not search roots (user content)"
                    .to_owned(),
                not_indexed,
            );
        }

        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for ev in &self.ini {
            for n in &ev.fields.specific_packages {
                names
                    .entry(stem(n).to_ascii_lowercase())
                    .or_insert_with(|| n.clone());
            }
        }
        let mut missing = Vec::new();
        for (key, name) in names {
            let found: Vec<String> = self
                .index
                .get(&key)
                .into_iter()
                .flatten()
                .map(|e| e.relative.clone())
                .collect();
            if found.is_empty() {
                missing.push(name.clone());
            }
            self.specific_packages
                .push(SpecificPackageCheck { name, found });
        }
        if !missing.is_empty() {
            self.diag(
                Severity::Warning,
                DiagnosticCode::SpecificPackageMissing,
                "SpecificPackage= names not present in the index".to_owned(),
                missing,
            );
        }
    }
}

/// Content check before walking: some `system` directory directly holds every
/// [`REQUIRED_CODE_PACKAGES`] entry with the package magic.
fn validate_code_root(root: &Path) -> Result<(), OpenError> {
    let io = |source| OpenError::Io {
        path: root.to_path_buf(),
        source,
    };
    let mut systems = Vec::new();
    for e in fs::read_dir(root).map_err(io)?.flatten() {
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if is_dir
            && e.file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("system")
        {
            systems.push(e.path());
        }
    }
    if systems.is_empty() {
        return Err(OpenError::NotAnInstallation {
            root: root.to_path_buf(),
            reasons: vec!["no `system` directory (any case) in the root".to_owned()],
        });
    }
    let mut reasons = Vec::new();
    for sys in &systems {
        let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
        if let Ok(rd) = fs::read_dir(sys) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_file()) {
                    found.insert(
                        e.file_name().to_string_lossy().to_ascii_lowercase(),
                        e.path(),
                    );
                }
            }
        }
        let mut ok = true;
        for req in REQUIRED_CODE_PACKAGES {
            match found.get(*req) {
                None => {
                    ok = false;
                    reasons.push(format!("{} lacks {req}", sys.display()));
                }
                Some(p) if !has_package_magic(p) => {
                    ok = false;
                    reasons.push(format!("{} is not an Unreal package", p.display()));
                }
                Some(_) => {}
            }
        }
        if ok {
            return Ok(());
        }
    }
    Err(OpenError::NotAnInstallation {
        root: root.to_path_buf(),
        reasons,
    })
}

#[cfg(test)]
mod tests;
