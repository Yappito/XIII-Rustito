//! Shared runtime setup for the headless script harness (`xiii-tool script run`) and the Bevy
//! `--play` runtime.
//!
//! Both callers must see **one** implementation of: script-set + map loading,
//! `[Engine.Engine] DefaultGame` resolution, `Package.Class` resolution, and construction of the
//! real map providers ([`crate::physics::WorldPhysicsAdapter`],
//! [`crate::animation::MapAnimationProvider`], [`crate::nav_provider::MapNavigationProvider`]).
//! The Bevy application additionally uses [`begin_play_all`] to drive the level start while
//! tolerating unimplemented natives (suspending only the failing actor); the headless harness
//! keeps its own abort-on-error lifecycle for byte-identical diagnostics.
//!
//! Nothing here opens a window or depends on Bevy. Installation files are read only.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use xiii_install::{Installation, OpenOptions, PACKAGE_MAGIC};
use xiii_package::Limits;
use xiii_script::animation::{AnimationData, SeqInfo};
use xiii_script::linker::GlobalRef;
use xiii_script::navigation::NavigationData;
use xiii_script::physics::WorldPhysics;
use xiii_script::vm::LEVEL_START_LIFECYCLE;
use xiii_script::{ObjectId, ScriptLimits, ScriptPackage, ScriptSet, Value, Vm, VmErrorKind};

use crate::animation::MapAnimationProvider;
use crate::nav_provider::MapNavigationProvider;
use crate::physics::WorldPhysicsAdapter;
use crate::{PackageCache, import_map};

/// `Package.Class` (or bare `Class`) in a loaded set.
pub fn resolve_class_path(set: &ScriptSet, path: &str) -> Option<GlobalRef> {
    match path.split_once('.') {
        Some((package, class)) => find_class(set, package, class),
        None => (0..set.packages.len()).find_map(|pi| {
            let export = set.packages[pi].export_by_path(path)?;
            Some(GlobalRef {
                package: pi,
                export,
            })
        }),
    }
}

/// `Package.Class` (or bare `Class` under one specific package) in a loaded set.
pub fn find_class(set: &ScriptSet, package: &str, path: &str) -> Option<GlobalRef> {
    let pi = set.package_index(package)?;
    let export = set.packages[pi].export_by_path(path)?;
    Some(GlobalRef {
        package: pi,
        export,
    })
}

/// Reads `[Engine.Engine] DefaultGame` from the installation's `Default.ini`.
pub fn default_game_from_ini(root: &Path) -> Option<String> {
    for name in [
        "Default.ini",
        "default.ini",
        "System/Default.ini",
        "system/Default.ini",
    ] {
        let path = root.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(v) = ini_value(&text, "Engine.Engine", "DefaultGame") {
            return Some(v);
        }
    }
    None
}

fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = s.eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            let v = v.split(';').next().unwrap_or(v).trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// Recursively lists every regular file under `root`, sorted by relative path (matching the
/// tool's `tagged_files` ordering).
fn all_files(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                walk(base, &path, out)?;
            } else if ty.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

/// True when the file starts with the UE2 package magic (`tagged_files`'s tag test).
fn starts_with_tag(path: &Path) -> bool {
    use std::io::Read as _;
    let mut prefix = [0u8; 4];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut filled = 0;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => return false,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    prefix == PACKAGE_MAGIC
}

/// Every `.u` script package of an installation (`relative path`, full path), sorted.
pub fn script_packages(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    Ok(all_files(root)?
        .into_iter()
        .filter(|(rel, _)| rel.to_ascii_lowercase().ends_with(".u"))
        .filter(|(_, path)| starts_with_tag(path))
        .collect())
}

/// Loads every `.u` package of an installation into a set. Table-level failures are returned as
/// `(relative path, error)`; the same failure policy as the tool's `script_cmd::load_install`.
pub fn load_install(root: &Path) -> std::io::Result<(ScriptSet, Vec<(String, String)>)> {
    let mut set = ScriptSet::new();
    let mut failures = Vec::new();
    for (rel, path) in script_packages(root)? {
        let data = std::fs::read(&path)?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match ScriptPackage::load(&name, data, &ScriptLimits::default(), &Limits::default()) {
            Ok(p) => {
                set.add(p);
            }
            Err(e) => failures.push((rel, e.to_string())),
        }
    }
    Ok((set, failures))
}

/// Finds a map file by stem under the installation (case-insensitive).
pub fn find_map(root: &Path, map: &str) -> Result<Option<PathBuf>, String> {
    let install = Installation::open(root, &OpenOptions::default()).map_err(|e| e.to_string())?;
    match install.resolve_map(map) {
        Ok(resolved) => Ok(Some(resolved.entry.path)),
        Err(_) => Ok(None),
    }
}

/// Loads the installation's `.u` packages plus one map; returns the set and the map index.
pub fn load_with_map(root: &Path, map: &str) -> Result<(ScriptSet, usize), String> {
    let (mut set, failures) = load_install(root).map_err(|e| e.to_string())?;
    if let Some((rel, e)) = failures.first() {
        return Err(format!("{rel}: {e}"));
    }
    let path = find_map(root, map)?
        .ok_or_else(|| format!("map {map} not found under {}", root.display()))?;
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pkg = ScriptPackage::load(map, data, &ScriptLimits::default(), &Limits::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let idx = set.add(pkg);
    Ok((set, idx))
}

/// One animation lookup recorded for the report.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimQuery {
    /// Animation source path queried (`Package.Object`).
    pub source: String,
    /// Sequence name requested.
    pub sequence: String,
    /// `found`, `not found` or `error: ...`.
    pub outcome: String,
}

/// Wraps an [`AnimationData`] provider and records every lookup, so a caller can report which
/// sequences were requested and whether they were found.
pub struct LoggingAnim {
    inner: Box<dyn AnimationData>,
    log: Rc<RefCell<Vec<AnimQuery>>>,
}

impl AnimationData for LoggingAnim {
    fn sequence(&mut self, source: &str, seq: &str) -> Result<Option<SeqInfo>, String> {
        let result = self.inner.sequence(source, seq);
        let outcome = match &result {
            Ok(Some(info)) => format!(
                "found ({} frames, {} fps, {} notifies)",
                info.frames,
                info.rate,
                info.notifies.len()
            ),
            Ok(None) => "not found".to_owned(),
            Err(e) => format!("error: {e}"),
        };
        self.log.borrow_mut().push(AnimQuery {
            source: source.to_owned(),
            sequence: seq.to_owned(),
            outcome,
        });
        result
    }
}

/// Which real-map providers to build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderSpec {
    /// Build the decoded map collision (`--physics map`).
    pub physics: bool,
    /// Build the decoded `MeshAnimation` provider (`--anim map`).
    pub animation: bool,
    /// Build the decoded `ReachSpec` navigation graph (`--nav map`).
    pub navigation: bool,
}

/// The optional real-map providers.
pub struct MapProviders {
    /// Decoded map collision adapter.
    pub physics: Option<Box<dyn WorldPhysics>>,
    /// Decoded `MeshAnimation` provider (possibly wrapped in [`LoggingAnim`]).
    pub animation: Option<Box<dyn AnimationData>>,
    /// Decoded `ReachSpec` navigation provider.
    pub navigation: Option<Box<dyn NavigationData>>,
}

/// Builds the real-map providers for `spec`. One [`PackageCache`] is opened and reused. The
/// `anim_log` receives every animation lookup (the provider is wrapped in [`LoggingAnim`]).
pub fn build_map_providers(
    root: &Path,
    map: &str,
    spec: &ProviderSpec,
    anim_log: &Rc<RefCell<Vec<AnimQuery>>>,
) -> Result<MapProviders, String> {
    if !spec.physics && !spec.animation && !spec.navigation {
        return Ok(MapProviders {
            physics: None,
            animation: None,
            navigation: None,
        });
    }
    let mut cache = PackageCache::open(root)?;
    let physics = if spec.physics {
        let scene = import_map(&mut cache, map)?;
        Some(Box::new(WorldPhysicsAdapter::from_scene(&scene)) as Box<dyn WorldPhysics>)
    } else {
        None
    };
    // Navigation is decoded before the animation provider consumes the cache.
    let navigation = if spec.navigation {
        let provider = MapNavigationProvider::from_cache(&mut cache, map)?;
        Some(Box::new(provider) as Box<dyn NavigationData>)
    } else {
        None
    };
    let animation = if spec.animation {
        let provider = MapAnimationProvider::from_cache(cache);
        Some(Box::new(LoggingAnim {
            inner: Box::new(provider),
            log: anim_log.clone(),
        }) as Box<dyn AnimationData>)
    } else {
        None
    };
    Ok(MapProviders {
        physics,
        animation,
        navigation,
    })
}

impl MapProviders {
    /// Installs every built provider into `vm`, emitting one note per provider (the same notes
    /// the headless harness prints).
    pub fn install(self, vm: &mut Vm, note: impl FnMut(xiii_script::TraceKind)) {
        let mut note = note;
        if let Some(provider) = self.physics {
            vm.set_physics(provider);
            note(xiii_script::TraceKind::Note(
                "map physics (decoded map collision), not the diagnostic flat floor".into(),
            ));
        }
        if let Some(provider) = self.navigation {
            let text = format!(
                "map navigation (decoded ReachSpec graph): {} points, {} edges",
                provider.points().len(),
                provider.edges().len()
            );
            vm.set_navigation(provider);
            note(xiii_script::TraceKind::Note(text));
        }
        if let Some(provider) = self.animation {
            vm.set_animation_data(provider);
            note(xiii_script::TraceKind::Note(
                "map animation (decoded MeshAnimation sequences), not the diagnostic fixed provider"
                    .into(),
            ));
        }
    }
}

/// Outcome of [`begin_play_all`].
pub struct BeginPlay {
    /// Spawned GameInfo id, when `spawn_level_actor`/`InitGame` succeeded.
    pub game_info: Option<ObjectId>,
    /// Actors suspended because their lifecycle code failed, with the first error per actor.
    pub suspended: Vec<(String, String)>,
}

/// Level start for the Bevy host: spawns `game_class` and runs `InitGame`, then runs the
/// [`LEVEL_START_LIFECYCLE`] for every actor **tolerantly** — an actor whose code fails is
/// suspended (`active = false`) and the remaining actors still run.
///
/// The headless harness keeps its own abort-on-error lifecycle; this variant exists so a play
/// window is not taken down by one unimplemented native. The GameInfo/level properties
/// (`Level`, `bStartup`, `bBegunPlay`) are the same as `Vm::begin_play_with_game_info`.
pub fn begin_play_all(vm: &mut Vm, ids: &[ObjectId], game_class: GlobalRef) -> BeginPlay {
    let mut suspended = Vec::new();
    let level_info = vm.find_level_info();
    // `begin_play_with_game_info(&[])` spawns the GameInfo, points LevelInfo.Game at it and runs
    // InitGame; passing no map ids keeps it from aborting on a map actor here.
    let game_info = match vm.begin_play_with_game_info(&[], game_class) {
        Ok(info) => Some(info),
        Err(e) => {
            suspended.push(("<GameInfo>".to_owned(), e.to_string()));
            None
        }
    };
    // bStartup is the gate `Pawn.PostBeginPlay` reads before spawning its `ControllerClass`;
    // `begin_play_with_game_info` clears it after its own (GameInfo-only) lifecycle, so set it
    // again for the map actors, then clear it when they are done (upstream `UGameEngine::LoadMap`).
    if let Some(li) = level_info {
        vm.set_property(li, "bStartup", 0, Value::Bool(true));
    }
    let mut all: Vec<ObjectId> = ids.to_vec();
    if let Some(info) = game_info {
        all.push(info);
    }
    for ev in LEVEL_START_LIFECYCLE {
        for &id in &all {
            let Some(obj) = vm.objects.get(id as usize) else {
                continue;
            };
            if obj.deleted || !obj.active {
                continue;
            }
            if let Err(e) = vm.send_event(id, ev, Vec::new()) {
                let name = vm.objects[id as usize].name.clone();
                vm.set_active(id, false);
                suspended.push((name, e.to_string()));
            }
        }
    }
    if let Some(li) = level_info {
        vm.set_property(li, "bStartup", 0, Value::Bool(false));
        vm.set_property(li, "bBegunPlay", 0, Value::Bool(true));
    }
    BeginPlay {
        game_info,
        suspended,
    }
}

/// Extracts the actor name a [`xiii_script::VmError`] was raised in: the innermost stack entry's
/// object, else the outermost. Used by the host to name the actor it suspended.
pub fn error_actor(error: &xiii_script::VmError) -> Option<String> {
    error
        .stack
        .last()
        .or_else(|| error.stack.first())
        .map(|s| s.object.clone())
}

/// True when a [`xiii_script::VmErrorKind`] is a missing-native / unsupported-operation failure
/// (as opposed to a data/decode error). The host reports both, but labels them differently.
pub fn is_unimplemented(kind: &xiii_script::VmErrorKind) -> bool {
    matches!(
        kind,
        VmErrorKind::UnimplementedNative { .. }
            | VmErrorKind::UnregisteredNative { .. }
            | VmErrorKind::UnsupportedToken { .. }
            | VmErrorKind::NoPhysicsProvider { .. }
            | VmErrorKind::NoAnimationProvider { .. }
            | VmErrorKind::NoNavProvider { .. }
    )
}
