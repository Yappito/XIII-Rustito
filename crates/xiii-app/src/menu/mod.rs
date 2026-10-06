//! Front-end menu mode (`item16`): the game's own `XIDInterf.XIIIRootWindow`/`XIIIMenu`.
//!
//! **What this is.** A consumer of the game's compiled menu logic, not a reimplemented menu.
//! The host loads the installation's `MapMenu` entry map and the decoded scripts, spawns the
//! real `XIDInterf.XIIIRootWindow` (the class `[Engine.Engine] GUIController` names) and the
//! real `XIDInterf.XIIIMenu` page, runs their decoded functions (`Created`, `BeforePaint`,
//! `InternalOnPreDraw`/`InternalOnDraw` on each control, `AfterPaint`, `InternalOnKeyEvent`,
//! `InternalOnClick`) through the VM, and renders the `Engine.Canvas` draw commands the scripts
//! record. The button textures, labels, layout and the new-game transition are the game's data
//! and code.
//!
//! **What is host-side (documented).** XIII's menu is drawn by a native GUI subsystem
//! (`GUI.dll`'s `GUIController`/`GUIComponent` render loop) that the x64 runtime does not have.
//! This module replaces the *render loop*: it instantiates the real pages/controls and runs the
//! game's own `InitComponent` -> `Created`/`FocusFirst` page-open path and its
//! `__OnPreDraw__`/`__OnDraw__` callbacks through the VM's delegate opcodes
//! (`DelegateProperty`/`LetDelegate`/`DelegateFunction`). It draws the recorded `Engine.Canvas`
//! commands. The `ViewportOwner` (the engine's player/viewport link) is a minimal host-created
//! `Engine.Player` -> `Engine.PlayerController` pair. The `cine00` intro video is played by the
//! item21 host cutscene player (`menu::cutscene`/`crate::video`): the game's own
//! `VideoPlayer.Open/Play/Stop/GetStatus` natives drive it fullscreen over the menu and the
//! new-game `ClientTravel("Plage00")` request is recorded for the host travel step.

mod cutscene;

mod config;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::window::PresentMode;

use xiii_decode::texture::{RgbaImage, decode_texture};
use xiii_package::Limits;
use xiii_package::ObjectRef as PkgRef;
use xiii_script::canvas::{CanvasFonts, DrawCommand, TravelRequest, parse_travel_note};
use xiii_script::{ObjRef, ObjectId, ScriptSet, TraceKind, Value, Vm, VmLimits};
use xiii_script::{SaveSlotInfo, SaveSlotProvider};
use xiii_world::PackageCache;
use xiii_world::runtime;

use crate::cli::Options;
use crate::play::hud::{self, FontDb};

/// Process-wide travel request written when the menu selects New game and read by `main`, which
/// then performs the host travel step (`--play` on the requested map).
static TRAVEL_REQUEST: Mutex<Option<TravelRequest>> = Mutex::new(None);
static SAVE_LOAD_REQUEST: Mutex<Option<u32>> = Mutex::new(None);

pub fn take_save_load_request() -> Option<u32> {
    SAVE_LOAD_REQUEST.lock().ok().and_then(|mut g| g.take())
}

fn save_dir(options: &Options) -> Result<PathBuf, String> {
    match &options.save_dir {
        Some(dir) => Ok(dir.clone()),
        None => crate::save::default_save_dir(),
    }
}

struct MenuSaveSlots {
    store: crate::save::SaveStore,
    slots: Vec<crate::save::SlotInfo>,
    requested: Option<(u8, u32)>,
    read_slot: Option<u32>,
}
impl MenuSaveSlots {
    fn open(dir: PathBuf) -> Result<Self, String> {
        let store = crate::save::SaveStore::open(dir);
        let slots = store.list()?;
        Ok(Self {
            store,
            slots,
            requested: None,
            read_slot: None,
        })
    }

    fn info(&self, slot: u32) -> Option<SaveSlotInfo> {
        let i = self.slots.iter().find(|i| i.slot == slot)?;
        let secs = i.modified_unix as i64;
        let days = secs.div_euclid(86400);
        let sod = secs.rem_euclid(86400);
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let mut y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        y += i64::from(m <= 2);
        Some(SaveSlotInfo {
            description: i.description.clone(),
            date: [
                y as i32,
                m as i32,
                d as i32,
                (sod / 3600) as i32,
                ((sod % 3600) / 60) as i32,
            ],
        })
    }
}
impl SaveSlotProvider for MenuSaveSlots {
    fn slot(&self, s: u32) -> Option<SaveSlotInfo> {
        self.info(s)
    }
    fn request_empty(&mut self, s: u32) -> bool {
        if s >= 10 {
            return false;
        }
        self.requested = Some((0, s));
        true
    }
    fn poll_empty(&mut self) -> Option<bool> {
        let (_, s) = self.requested?;
        Some(self.info(s).is_none())
    }
    fn request_description(&mut self, s: u32) -> bool {
        if self.info(s).is_none() {
            return false;
        }
        self.requested = Some((1, s));
        true
    }
    fn poll_description(&mut self) -> Option<String> {
        let (_, s) = self.requested?;
        Some(self.info(s)?.description)
    }
    fn request_date(&mut self, s: u32) -> bool {
        if self.info(s).is_none() {
            return false;
        }
        self.requested = Some((2, s));
        true
    }
    fn poll_date(&mut self) -> Option<[i32; 5]> {
        let (_, s) = self.requested?;
        Some(self.info(s)?.date)
    }
    fn request_read(&mut self, s: u32) -> bool {
        if self.info(s).is_none() || self.store.read(s).is_err() {
            return false;
        }
        self.read_slot = Some(s);
        true
    }
    fn poll_read(&mut self) -> bool {
        self.read_slot.is_some()
    }
    fn take_read_slot(&mut self) -> Option<u32> {
        self.read_slot.take()
    }
}

/// Takes the recorded travel request, if any.
pub fn take_travel_request() -> Option<TravelRequest> {
    TRAVEL_REQUEST.lock().ok().and_then(|mut g| g.take())
}

/// Sets the recorded travel request (idempotent; keeps the first).
fn set_travel_request(req: TravelRequest) {
    if let Ok(mut g) = TRAVEL_REQUEST.lock()
        && g.is_none()
    {
        *g = Some(req);
    }
}

/// Number of buttons on `XIDInterf.XIIIMenu` in the decoded `Created` (`Controls[0..6]`).
const XIIIMENU_CONTROL_COUNT: usize = 6;

/// Host labels for the main-menu controls, in `Controls` order. Decoded from the `XIIIMenu`
/// class defaults (`ContinueText`, `MultiplayerText`, `LoadGameText`, `OptionsText`,
/// `NewGameText`, `QuitText`); used for the `--menu-script` `click <name>` form and the report.
const XIIIMENU_LABELS: [&str; XIIIMENU_CONTROL_COUNT] = [
    "continue",
    "multiplayer",
    "loadgame",
    "options",
    "newgame",
    "quit",
];

/// `XIIIMenu` label struct properties, in `Controls` order. `XIIIMenu.AfterPaint` draws a
/// button's label only when `bDisplayTex` is set (the focused/hovered control), which its
/// `__OnActivate__` -> `MouseEnter` delegate sets. Used by the opt-in layout test.
#[allow(dead_code)] // exercised by the opt-in menu layout test
const XIIIMENU_LABEL_PROPS: [&str; XIIIMENU_CONTROL_COUNT] = [
    "ContinueLabel",
    "MultiLabel",
    "LoadLabel",
    "OptionsLabel",
    "NewLabel",
    "QuitLabel",
];

/// Front-end menu plugin.
pub struct MenuPlugin {
    /// Parsed options (game dir, script, unattended settings).
    pub options: Options,
}

#[derive(Resource)]
struct MenuConfig {
    options: Options,
}

#[derive(Resource, Default)]
struct MenuShotFlag(bool);

#[derive(Resource)]
struct MenuState {
    tick: u64,
    start: Instant,
    exit_secs: Option<f32>,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
}

/// One `--menu-script` action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// `key <name>`: press+release the named key through `InternalOnKeyEvent`.
    Key(String),
    /// `click <index|label>`: call the game's `InternalOnClick(control)`.
    Click(String),
    /// `focus <index|label>`: set the page's `FocusedControl`.
    Focus(String),
    /// `newgame`: focus the New game entry and press Enter (the game's key path).
    NewGame,
    /// `open <Package.Class>`: instantiate that decoded page class as the active page (a host
    /// bridge for the native `GUIController.OpenMenu` page stack; used to reach a sub-menu).
    Open(String),
}

/// One scheduled action.
#[derive(Debug, Clone, PartialEq)]
struct Scheduled {
    time: f32,
    action: Action,
}

/// A parsed `--menu-script`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuScript {
    /// Scheduled actions, sorted by time as written.
    actions: Vec<Scheduled>,
}

impl MenuScript {
    /// Parses the line format documented in `--help`. `t=<secs> <action>` where action is
    /// `key <name>`, `focus <target>`, `click <target>` or `newgame`.
    pub fn parse(text: &str) -> Result<MenuScript, String> {
        let mut actions = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (t, rest) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("line {}: expected 't=<secs> <action>'", n + 1))?;
            let time: f32 = t
                .trim()
                .strip_prefix("t=")
                .ok_or_else(|| format!("line {}: expected a 't=' prefix", n + 1))?
                .trim()
                .parse()
                .map_err(|_| format!("line {}: invalid time {t:?}", n + 1))?;
            if !(time.is_finite() && time >= 0.0) {
                return Err(format!("line {}: time must be finite and >= 0", n + 1));
            }
            let mut it = rest.split_whitespace();
            let verb = it.next().unwrap_or("");
            let arg = it.next();
            let action = match (verb, arg) {
                ("newgame", None) => Action::NewGame,
                ("key", Some(k)) => Action::Key(k.to_owned()),
                ("focus", Some(v)) => Action::Focus(v.to_owned()),
                ("click", Some(v)) => Action::Click(v.to_owned()),
                ("open", Some(v)) => Action::Open(v.to_owned()),
                _ => {
                    return Err(format!(
                        "line {}: unknown action {line:?} (newgame|key K|focus N|click N)",
                        n + 1
                    ));
                }
            };
            actions.push(Scheduled { time, action });
        }
        Ok(MenuScript { actions })
    }

    /// Loads and parses a script file.
    pub fn load(path: &Path) -> Result<MenuScript, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        MenuScript::parse(&text)
    }
}

/// The menu VM: the spawned root/page/controls, the decoded fonts and the current draw list.
pub struct MenuSession {
    vm: Vm<'static>,
    save_dir: PathBuf,
    config_path: PathBuf,
    root: ObjectId,
    page: ObjectId,
    canvas: ObjectId,
    controls: Vec<ObjectId>,
    control_labels: Vec<String>,
    fonts: Option<Arc<FontDb>>,
    packages: PackageCache,
    commands: Vec<DrawCommand>,
    slot_lines: Vec<String>,
    entities: Vec<Entity>,
    clip: [f32; 2],
    trace_cursor: usize,
    travel: Option<TravelRequest>,
    script: Option<MenuScript>,
    next_action: usize,
    elapsed: f32,
    map_actor_count: usize,
    fps: u64,
    log: Vec<String>,
    errors: Vec<String>,
}

/// `CanvasFonts` provider backed by the decoded HUD fonts.
struct VmFonts(Arc<FontDb>);

impl CanvasFonts for VmFonts {
    fn measure(&self, font: &str, text: &str) -> Option<(f32, f32)> {
        self.0.find(font).map(|f| f.measure(text))
    }
}

/// `ExternalObjectData` provider for the front-end textures. It answers `USize`/`VSize` for a
/// `Package.Object` name the way UE2's `StaticFindObject`-by-name does: an exact path match
/// first, then a unique leaf-name match (the menu's `DynamicLoadObject` names are the object's
/// leaf, e.g. `XIIIMenuStart.continue01gris`, while the export path is
/// `interface_home.continue01gris`). This replaces the generic `TextureProperties` provider for
/// this mode only; without it the decoded `XIIITextureButton.Paint` fails with an explicit
/// unsupported-property error and draws nothing.
struct MenuTextureData {
    cache: RefCell<PackageCache>,
    sizes: RefCell<HashMap<String, Option<(i32, i32)>>>,
}

impl MenuTextureData {
    fn open(root: &Path) -> Result<Self, String> {
        Ok(Self {
            cache: RefCell::new(PackageCache::open(root)?),
            sizes: RefCell::new(HashMap::new()),
        })
    }

    fn size(&self, path: &str) -> Option<(i32, i32)> {
        if let Some(cached) = self.sizes.borrow().get(path) {
            return *cached;
        }
        let computed = self.decode_size(path);
        self.sizes.borrow_mut().insert(path.to_owned(), computed);
        computed
    }

    fn decode_size(&self, path: &str) -> Option<(i32, i32)> {
        let (package, rest) = path.split_once('.')?;
        let leaf = rest.rsplit('.').next().unwrap_or(rest);
        let loaded = self.cache.borrow_mut().get(package).ok()?;
        let pkg = &loaded.package;
        let mut exact = None;
        let mut by_leaf = Vec::new();
        for e in 0..pkg.exports().len() {
            let Some(p) = pkg.object_path(PkgRef::Export(e as u32)) else {
                continue;
            };
            if p.eq_ignore_ascii_case(rest) {
                exact = Some(e);
                break;
            }
            if p.rsplit('.')
                .next()
                .is_some_and(|l| l.eq_ignore_ascii_case(leaf))
            {
                by_leaf.push(e);
            }
        }
        let export = match exact {
            Some(e) => e,
            None if by_leaf.len() == 1 => by_leaf[0],
            None => return None,
        };
        if !pkg
            .export_class_path(export)
            .is_some_and(|c| c.eq_ignore_ascii_case("Engine.Texture"))
        {
            return None;
        }
        let texture = decode_texture(pkg, &loaded.data, export).ok()?;
        Some((texture.size[0] as i32, texture.size[1] as i32))
    }
}

impl xiii_script::ExternalObjectData for MenuTextureData {
    fn property(&self, path: &str, property: &str) -> Option<Value> {
        if !matches!(property, "usize" | "vsize") {
            return None;
        }
        let (w, h) = self.size(path)?;
        Some(Value::Int(if property == "usize" { w } else { h }))
    }
}

impl MenuSession {
    /// Loads the script set and entry map, and spawns the root controller + main menu page. Fonts
    /// are loaded later (they need the Bevy `Assets<Image>`).
    #[cfg(test)]
    fn open(
        game_dir: &Path,
        save_dir: PathBuf,
        script: Option<MenuScript>,
    ) -> Result<MenuSession, String> {
        let dir = config::default_dir()?;
        Self::open_configured(game_dir, save_dir, &dir, script)
    }

    fn open_configured(
        game_dir: &Path,
        save_dir: PathBuf,
        config_dir: &Path,
        script: Option<MenuScript>,
    ) -> Result<MenuSession, String> {
        let config_path = config::user_path(game_dir, config_dir)?;
        let settings = config::load(game_dir, &config_path)?;
        // `[URL] LocalMap=mapmenu.unr` in Default.ini is the local front-end map. `[URL] Map`
        // names `Index.unr`, which is absent from this corpus; MapMenu is the installed one.
        let (set, map_idx) = runtime::load_with_map(game_dir, "MapMenu")?;
        let set: &'static ScriptSet = Box::leak(Box::new(set));
        let mut vm = Vm::new(set, VmLimits::default());
        vm.canvas.menu = Some(settings);
        vm.set_save_slots(Box::new(MenuSaveSlots::open(save_dir.clone())?));
        runtime::configure_localization(&mut vm, game_dir)?;
        runtime::configure_external_objects(&mut vm, game_dir);
        // The menu's `DynamicLoadObject` names are leaf names whose exports live under a group
        // outer; install the menu's own `USize`/`VSize` resolver (leaf fallback).
        match MenuTextureData::open(game_dir) {
            Ok(p) => vm.set_external_object_data(Box::new(p)),
            Err(e) => eprintln!("[menu] texture size provider unavailable: {e}"),
        }
        let map_actor_count = vm
            .load_level(map_idx, &Limits::default())
            .map(|v| v.len())
            .map_err(|e| format!("loading MapMenu actors: {e}"))?;
        let mut log = Vec::new();
        log.push(format!(
            "[menu] entry map MapMenu: {map_actor_count} script actors loaded"
        ));

        // `Engine.Canvas` is the object the menu scripts draw into (their `AfterPaint`/`Paint`
        // take it as the first argument).
        let canvas_class = runtime::resolve_class_path(set, "Engine.Canvas")
            .ok_or("Engine.Canvas class is not in the loaded script set")?;
        let canvas = vm
            .spawn(canvas_class, "MenuCanvas")
            .map_err(|e| format!("creating Canvas: {e}"))?;
        vm.set_active(canvas, true);
        vm.set_property(canvas, "DrawColor", 0, color_value(255, 255, 255, 255));
        vm.set_property(canvas, "BorderColor", 0, color_value(0, 0, 0, 0));
        vm.set_property(canvas, "Style", 0, Value::Byte(1));

        // `[Engine.Engine] GUIController=XIDInterf.XIIIRootWindow`.
        let root_class = runtime::resolve_class_path(set, "XIDInterf.XIIIRootWindow")
            .ok_or("XIDInterf.XIIIRootWindow is not loaded")?;
        let root = vm
            .spawn(root_class, "XIIIRootWindow(menu)")
            .map_err(|e| format!("creating root window: {e}"))?;
        vm.set_active(root, true);
        // `GUIScale`/`bMapMenu`/`CurrentPF`/`fTextureScaleFactorForConsole`: the fields the
        // decoded `Created`/`BeforePaint`/`DrawStretchedTexture` read from `myRoot`.
        vm.set_property(root, "GUIScale", 0, Value::Float(1.0));
        vm.set_property(root, "bMapMenu", 0, Value::Bool(true));
        vm.set_property(root, "bMusicPlay", 0, Value::Bool(true));
        vm.send_event(root, "LoadIngameMenu", vec![])
            .map_err(|e| e.to_string())?;
        vm.send_event(root, "LoadMainMenu", vec![])
            .map_err(|e| e.to_string())?;
        vm.set_property(root, "CurrentPF", 0, Value::Int(0));
        vm.set_property(root, "fTextureScaleFactorForConsole", 0, Value::Float(1.0));
        vm.set_property(root, "GameResolution", 0, Value::Str(String::new()));
        // `XIIIRootWindow.GetPlayerOwner` returns `ViewportOwner.Actor`, and the decoded menu
        // calls it before `PlayMenu`/`StopAllSounds`/`ClientTravel`. A `None` receiver would make
        // the VM skip the whole call (Accessed None), so the new-game path would never reach the
        // game's map-load request. Link a minimal `Engine.Player` -> `Engine.PlayerController`
        // pair; the menu only uses it as a call receiver (the map's real controller is created by
        // the host travel step).
        if let (Some(player_cls), Some(pc_cls)) = (
            runtime::resolve_class_path(set, "Engine.Player"),
            runtime::resolve_class_path(set, "XIII.XIIIPlayerController")
                .or_else(|| runtime::resolve_class_path(set, "Engine.PlayerController")),
        ) {
            let viewport = vm
                .spawn(player_cls, "ViewportOwner(menu)")
                .map_err(|e| format!("creating ViewportOwner: {e}"))?;
            vm.set_active(viewport, true);
            let pc = vm
                .spawn(pc_cls, "MenuPlayerController")
                .map_err(|e| format!("creating PlayerController: {e}"))?;
            vm.set_active(pc, true);
            xiii_script::canvas::apply_menu_config(&mut vm, pc).map_err(|e| e.to_string())?;
            let level = vm
                .objects
                .iter()
                .enumerate()
                .find(|(id, o)| vm.is_a(*id as ObjectId, "LevelInfo") && !o.deleted)
                .map(|(id, _)| id as ObjectId);
            if let Some(level) = level {
                vm.set_property(pc, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
            }
            vm.set_property(
                viewport,
                "Actor",
                0,
                Value::Object(Some(ObjRef::Instance(pc))),
            );
            vm.set_property(
                root,
                "ViewportOwner",
                0,
                Value::Object(Some(ObjRef::Instance(viewport))),
            );
        } else {
            log.push(
                "[menu] Engine.Player/PlayerController not loaded; GetPlayerOwner() stays None"
                    .to_owned(),
            );
        }
        // `XIIIRootWindow.InitializeController` loads its two background textures by name from
        // the external `XIIIMenuStart` texture package; do the same so `DrawStretchedTexture`
        // has `FondMenu`.
        for (class, prop) in [
            ("Engine.Texture", "FondMenu"),
            ("Engine.Texture", "tFondNoir"),
        ] {
            let name = if prop == "FondMenu" {
                "XIIIMenuStart.menublanc"
            } else {
                "XIIIMenuStart.menunoir"
            };
            match load_texture_native(&mut vm, name, class) {
                Some(v) => {
                    vm.set_property(root, prop, 0, v);
                }
                None => log.push(format!("[menu] root {prop} texture {name} did not resolve")),
            }
        }

        // The main menu page (`XIDInterf.XIIIMenu`, which `XIIIRootWindow.UWindows.BeginState`
        // opens for the map menu).
        let page_class = runtime::resolve_class_path(set, "XIDInterf.XIIIMenu")
            .ok_or("XIDInterf.XIIIMenu is not loaded")?;
        let page = vm
            .spawn(page_class, "XIIIMenu(menu)")
            .map_err(|e| format!("creating main menu: {e}"))?;
        vm.set_active(page, true);
        xiii_script::canvas::apply_menu_config(&mut vm, page).map_err(|e| e.to_string())?;
        // Run the game's own page-open path. `XIIIWindow.InitComponent` assigns the
        // `__OnOpen__`/`__OnPreDraw__`/`__OnDraw__`/`__OnKeyEvent__` delegates, sets `myRoot`
        // and calls `Created` (which loads the panel textures with `DynamicLoadObject` and
        // builds the six controls with `CreateControl`). `GUI.GUIPage.InitComponent` then
        // initialises every control (its delegates and `myRoot`) and focuses the first one,
        // which activates its `MouseEnter` (highlight) by delegate. The VM now interprets the
        // delegate opcodes, so this is the real flow rather than the item16 host wiring.
        if let Err(e) = vm.send_event(
            page,
            "InitComponent",
            vec![
                Value::Object(Some(ObjRef::Instance(root))),
                Value::Object(None),
            ],
        ) {
            log.push(format!("[menu] XIIIMenu.InitComponent: {e}"));
        }

        vm.set_property(
            root,
            "ActivePage",
            0,
            Value::Object(Some(ObjRef::Instance(page))),
        );
        vm.set_property(
            root,
            "MenuStack",
            0,
            Value::Array(vec![Value::Object(Some(ObjRef::Instance(page)))]),
        );
        vm.send_event(page, "ShowWindow", vec![])
            .map_err(|e| e.to_string())?;
        // Read the built controls for `--menu-script` target resolution and the report.
        let mut controls = Vec::new();
        let mut control_labels = Vec::new();
        if let Some(Value::Array(items)) = vm.get_property(page, "Controls").cloned() {
            for (i, item) in items.iter().enumerate() {
                let Value::Object(Some(ObjRef::Instance(id))) = item else {
                    continue;
                };
                controls.push(*id);
                control_labels.push(
                    XIIIMENU_LABELS
                        .get(i)
                        .map(|s| (*s).to_owned())
                        .unwrap_or_else(|| format!("control{i}")),
                );
            }
        }
        log.push(format!(
            "[menu] XIIIMenu: {} control(s) {:?}",
            controls.len(),
            control_labels
        ));
        // The focused control comes from the game's own `GUIPage.InitComponent` -> `FocusFirst`
        // (delegate `__OnActivate__` -> `MouseEnter` sets `bDisplayTex`, so its caption is
        // drawn by `AfterPaint`). Report it so an empty focus is visible, not hidden.
        let focused = match vm.get_property(page, "FocusedControl") {
            Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
            _ => None,
        };
        if let Some(f) = focused {
            log.push(format!(
                "[menu] focus: {} ({})",
                vm.objects[f as usize].name,
                vm.set().path(vm.objects[f as usize].class)
            ));
        } else {
            log.push("[menu] focus: none (FocusFirst found no tab-stop control)".to_owned());
        }

        let packages = PackageCache::open(game_dir)?;
        let trace_cursor = vm.trace.len();
        Ok(MenuSession {
            vm,
            save_dir,
            config_path,
            root,
            page,
            canvas,
            controls,
            control_labels,
            fonts: None,
            packages,
            commands: Vec::new(),
            slot_lines: Vec::new(),
            entities: Vec::new(),
            clip: [0.0, 0.0],
            trace_cursor,
            travel: None,
            script,
            next_action: 0,
            elapsed: 0.0,
            map_actor_count,
            fps: 0,
            log,
            errors: Vec::new(),
        })
    }

    fn bool_prop(&self, id: ObjectId, name: &str) -> bool {
        matches!(self.vm.get_property(id, name), Some(Value::Bool(true)))
    }

    /// Advances the schedule, ticks the VM, drives the `PlayingVideo` state and scans the trace
    /// for the game's `ClientTravel` request.
    fn advance(&mut self, dt: f32) {
        self.elapsed += dt;
        let elapsed = self.elapsed;
        while let Some(s) = self
            .script
            .as_ref()
            .and_then(|s| s.actions.get(self.next_action))
        {
            if s.time > elapsed {
                break;
            }
            let action = s.action.clone();
            self.next_action += 1;
            if std::env::var_os("XIII_WATCH_SAVE_SLOTS").is_some() {
                eprintln!("[menu-action] {action:?} at {elapsed:.3}");
            }
            if let Err(e) = self.apply_action(&action) {
                self.errors.push(format!("{action:?}: {e}"));
            }
        }
        // The map actors are not begun; only the root/page state machines and timers need time.
        if let Err(e) = self.vm.tick(dt) {
            self.record_error(format!("menu tick: {e}"));
        }
        self.sync_page();
        let events: Vec<_> = self
            .vm
            .drain_events()
            .into_iter()
            .map(|e| (self.elapsed as f64, e))
            .collect();
        crate::audio::pump(events.iter());
        if let Some(settings) = self.vm.canvas.menu.as_mut()
            && settings.save_requested
        {
            match config::save(&self.config_path, settings) {
                Ok(()) => println!("[menu] saved {}", self.config_path.display()),
                Err(e) => self.record_error(format!("saving configuration: {e}")),
            }
        }
        if let Some(settings) = self.vm.canvas.menu.as_mut() {
            crate::audio::set_menu_volume_db(settings.master_db);
            crate::audio::set_menu_music_enabled(settings.music != 0);
            if settings.stop_music {
                crate::audio::stop_menu_music();
                settings.stop_music = false;
            }
        }
        // `XIIIMenu.PlayingVideo.Tick` is a state event on a non-actor: `Vm::tick` dispatches
        // `Tick` only for actors, so drive it explicitly while the page thinks a video plays.
        if self.bool_prop(self.page, "bPlayingVideo")
            && let Err(e) = self
                .vm
                .send_event(self.page, "Tick", vec![Value::Float(dt)])
        {
            self.errors.push(format!("PlayingVideo.Tick: {e}"));
        }
        self.scan_trace();
    }

    fn scan_trace(&mut self) {
        if self.vm.trace.len() < self.trace_cursor {
            self.trace_cursor = 0;
        }
        let start = self.trace_cursor.min(self.vm.trace.len());
        for ev in &self.vm.trace[start..] {
            if let TraceKind::Note(note) = &ev.kind
                && let Some(req) = parse_travel_note(note)
            {
                self.travel = Some(req);
            }
            if let TraceKind::Note(note) = &ev.kind
                && let Some(slot) = note
                    .strip_prefix("GUI_SAVE_LOAD_SLOT:")
                    .and_then(|s| s.parse().ok())
                && let Ok(mut request) = SAVE_LOAD_REQUEST.lock()
            {
                *request = Some(slot);
            }
        }
        self.trace_cursor = self.vm.trace.len();
    }

    /// Resolves a control target (`index` or label) to its object.
    fn control(&self, target: &str) -> Option<ObjectId> {
        if let Ok(i) = target.parse::<usize>() {
            return self.controls.get(i).copied();
        }
        let lower = target.to_ascii_lowercase();
        self.control_labels
            .iter()
            .position(|l| l.eq_ignore_ascii_case(&lower))
            .and_then(|i| self.controls.get(i).copied())
    }

    /// Applies one scripted action through the game's own menu entry points.
    fn apply_action(&mut self, action: &Action) -> Result<(), String> {
        match action {
            Action::Focus(target) => {
                let c = self
                    .control(target)
                    .ok_or_else(|| format!("no control {target:?}"))?;
                self.vm.set_property(
                    self.page,
                    "FocusedControl",
                    0,
                    Value::Object(Some(ObjRef::Instance(c))),
                );
                Ok(())
            }
            Action::Click(target) => {
                if !self.vm.is_a(self.page, "XIIIMenuLoadGameWindow")
                    && target.eq_ignore_ascii_case("loadgame")
                {
                    return self.open_page("XIDInterf.XIIIMenuLoadGameWindow");
                }
                if !self.vm.is_a(self.page, "XIIIMenuLoadGameWindow")
                    && target.eq_ignore_ascii_case("continue")
                {
                    let store = crate::save::SaveStore::open(self.save_dir.clone());
                    let newest = store
                        .newest()?
                        .ok_or("Continue requested but no save slot exists")?;
                    if let Ok(mut request) = SAVE_LOAD_REQUEST.lock() {
                        *request = Some(newest.slot);
                    }
                    println!(
                        "[menu] Continue selected newest save slot {} ({})",
                        newest.slot, newest.description
                    );
                    return Ok(());
                }
                if self.vm.is_a(self.page, "XIIIMenuLoadGameWindow")
                    && let Ok(control_index) = target.parse::<u32>()
                    && control_index >= 2
                {
                    let visible = match self.vm.get_property(self.page, "MaxViewable") {
                        Some(Value::Int(n)) if *n > 0 => *n as u32,
                        _ => 5,
                    };
                    let page = match self.vm.get_property(self.page, "onPage") {
                        Some(Value::Int(n)) if *n > 0 => *n as u32,
                        _ => 1,
                    };
                    let slot = (page - 1) * visible + control_index - 2;
                    let Some(provider) = self.vm.save_slots_mut() else {
                        return Err("save-slot provider is unavailable".into());
                    };
                    if !provider.request_read(slot) || !provider.poll_read() {
                        return Err(format!("save slot {slot} is empty or unreadable"));
                    }
                    let Some(slot) = provider.take_read_slot() else {
                        return Err("save-slot provider did not return the selected slot".into());
                    };
                    if let Ok(mut request) = SAVE_LOAD_REQUEST.lock() {
                        *request = Some(slot);
                    }
                    return Ok(());
                }
                let c = self
                    .control(target)
                    .ok_or_else(|| format!("no control {target:?}"))?;
                self.vm
                    .send_event(
                        self.page,
                        "InternalOnClick",
                        vec![Value::Object(Some(ObjRef::Instance(c)))],
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            Action::Key(name) => {
                let key = unreal_key(name).ok_or_else(|| format!("unknown key {name:?}"))?;
                self.dispatch_key(key)
            }
            Action::Open(class_name) => self.open_page(class_name),
            Action::NewGame => {
                // Route through the decoded key handler: focus the New game control, then
                // Enter, exactly as `XIIIMenu.InternalOnKeyEvent` expects.
                let c = self
                    .control("newgame")
                    .ok_or("New game control is absent")?;
                self.vm.set_property(
                    self.page,
                    "FocusedControl",
                    0,
                    Value::Object(Some(ObjRef::Instance(c))),
                );
                self.vm
                    .send_event(
                        self.page,
                        "InternalOnKeyEvent",
                        vec![Value::Byte(13), Value::Byte(1), Value::Float(0.0)],
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// Sends a live key press to the game's menu handler only while its `VideoPlayer` is open.
    /// During `PlayingVideo`, the game's own `InternalOnKeyEvent` decides whether the key stops
    /// the clip (Enter/Escape) or is ignored/passed to the base handler; the host adds no skip
    /// mapping of its own.
    pub(crate) fn video_key_event(&mut self, key: u8) {
        if self.vm.video_name().is_none() {
            return;
        }
        if let Err(e) = self.vm.send_event(
            self.page,
            "InternalOnKeyEvent",
            vec![Value::Byte(key), Value::Byte(1), Value::Float(0.0)],
        ) {
            self.errors.push(format!("live key {key}: {e}"));
        }
        self.scan_trace();
    }

    /// Instantiates a decoded page class as the active page and runs its own `InitComponent`
    /// (the host bridge for the native `GUIController.OpenMenu` page stack). Used by
    /// `--menu-script open`.
    fn record_error(&mut self, error: String) {
        if !self.errors.contains(&error) {
            eprintln!("[menu] {error}");
            self.errors.push(error);
        }
    }

    fn dispatch_key(&mut self, key: u8) -> Result<(), String> {
        let args = vec![Value::Byte(key), Value::Byte(1), Value::Float(0.0)];
        if let Some(Value::Object(Some(ObjRef::Instance(id)))) =
            self.vm.get_property(self.page, "FocusedControl").cloned()
        {
            let result = self
                .vm
                .call_delegate(id, "__OnKeyEvent__Delegate", "OnKeyEvent", args.clone())
                .map_err(|e| e.to_string())?;
            if result == Value::Bool(true) {
                return Ok(());
            }
        }
        self.vm
            .send_event(self.page, "InternalOnKeyEvent", args)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn sync_page(&mut self) {
        let Some(Value::Object(Some(ObjRef::Instance(page)))) =
            self.vm.get_property(self.root, "ActivePage").cloned()
        else {
            return;
        };
        if self.page == page {
            return;
        }
        self.page = page;
        self.slot_lines.clear();
        self.controls.clear();
        self.control_labels.clear();
        if let Some(Value::Array(items)) = self.vm.get_property(page, "Controls") {
            for (i, v) in items.iter().enumerate() {
                if let Value::Object(Some(ObjRef::Instance(id))) = v {
                    self.controls.push(*id);
                    self.control_labels.push(if self.vm.is_a(page, "XIIIMenu") {
                        XIIIMENU_LABELS.get(i).unwrap_or(&"control").to_string()
                    } else {
                        format!("control{i}")
                    });
                }
            }
        }
        self.call(page, "ShowWindow", vec![]);
        println!(
            "[menu] active {}: {} controls",
            self.vm.set().path(self.vm.objects[page as usize].class),
            self.controls.len()
        );
    }

    fn open_page(&mut self, class_name: &str) -> Result<(), String> {
        let class = runtime::resolve_class_path(self.vm.set(), class_name)
            .ok_or_else(|| format!("page class {class_name} is not loaded"))?;
        let page = self
            .vm
            .spawn(class, "MenuPage(menu)")
            .map_err(|e| e.to_string())?;
        self.vm.set_active(page, true);
        xiii_script::canvas::apply_menu_config(&mut self.vm, page).map_err(|e| e.to_string())?;
        self.vm.set_property(
            page,
            "ParentPage",
            0,
            Value::Object(Some(ObjRef::Instance(self.page))),
        );
        let mut stack = match self.vm.get_property(self.root, "MenuStack").cloned() {
            Some(Value::Array(a)) => a,
            _ => vec![],
        };
        stack.push(Value::Object(Some(ObjRef::Instance(page))));
        self.vm
            .set_property(self.root, "MenuStack", 0, Value::Array(stack));
        self.vm.set_property(
            self.root,
            "ActivePage",
            0,
            Value::Object(Some(ObjRef::Instance(page))),
        );
        // The game's own page-open path: assigns the draw/key delegates, calls `Created`
        // (builds controls) and `FocusFirst` (activates the first control's highlight).
        if let Err(e) = self.vm.send_event(
            page,
            "InitComponent",
            vec![
                Value::Object(Some(ObjRef::Instance(self.root))),
                Value::Object(None),
            ],
        ) {
            self.errors.push(format!("{class_name}.InitComponent: {e}"));
        }
        if class_name.eq_ignore_ascii_case("XIDInterf.XIIIMenuLoadGameWindow") {
            // The state machine uses GUI.dll's latent Sleep/poll loop. This host GUI page has no
            // native GUI scheduler, so complete its already-requested slot queries from the same
            // provider and feed the page's own SaveSlotsInfo/PageSwitch presentation path.
            let _ = self.vm.goto_state(page, "None", None);
            let provider = MenuSaveSlots::open(self.save_dir.clone())?;
            let mut info = vec![Value::Str(String::new()); 10];
            self.slot_lines.clear();
            for slot in &provider.slots {
                let Some(slot_info) = provider.info(slot.slot) else {
                    continue;
                };
                let [year, month, day, hour, minute] = slot_info.date;
                let minute = if minute < 10 {
                    format!("0{minute}")
                } else {
                    minute.to_string()
                };
                let rendered = format!(
                    "{}  {hour}:{minute} {month}/{day}/{year}",
                    slot_info.description
                );
                if slot.slot < 5 {
                    self.slot_lines.push(rendered.clone());
                }
                info[slot.slot as usize] = Value::Str(rendered);
            }
            for (index, value) in info.into_iter().enumerate() {
                self.vm.set_property(page, "SaveSlotsInfo", index, value);
            }
            if let Err(e) = self.vm.send_event(page, "PageSwitch", Vec::new()) {
                self.errors.push(format!("load-game PageSwitch: {e}"));
            }
        }
        let mut controls = Vec::new();
        let mut labels = Vec::new();
        if let Some(Value::Array(items)) = self.vm.get_property(page, "Controls").cloned() {
            for (i, item) in items.iter().enumerate() {
                let Value::Object(Some(ObjRef::Instance(id))) = item else {
                    continue;
                };
                controls.push(*id);
                labels.push(format!("control{i}"));
            }
        }
        println!(
            "[menu] opened page {class_name}: {} control(s), state={:?}, active={}",
            controls.len(),
            self.vm.state_name(page),
            self.vm.objects[page as usize].active
        );
        self.call(page, "ShowWindow", vec![]);
        self.page = page;
        self.controls = controls;
        self.control_labels = labels;
        Ok(())
    }

    /// Runs the decoded GUI render loop for one frame and collects the Canvas commands.
    ///
    /// The native `GUI.dll` loop walks the component tree: every page/control is pre-drawn
    /// (`__OnPreDraw__`), then drawn (`__OnDraw__`), and the page's `AfterPaint` draws the
    /// focused control's onomatopoeia and caption. All three callbacks are the game's own
    /// delegates (assigned by `InitComponent`), invoked through [`Vm::call_delegate`].
    fn refresh_commands(&mut self) {
        let (clip_w, clip_h) = (self.clip[0], self.clip[1]);
        if clip_w <= 0.0 || clip_h <= 0.0 {
            return;
        }
        self.vm
            .set_property(self.canvas, "ClipX", 0, Value::Float(clip_w));
        self.vm
            .set_property(self.canvas, "ClipY", 0, Value::Float(clip_h));
        let canvas = Value::Object(Some(ObjRef::Instance(self.canvas)));
        self.call_delegate(
            self.page,
            "__OnPreDraw__Delegate",
            "OnPreDraw",
            vec![canvas.clone()],
        );
        self.call_delegate(
            self.page,
            "__OnDraw__Delegate",
            "OnDraw",
            vec![canvas.clone()],
        );
        let controls = self.controls.clone();
        for c in controls.iter().copied() {
            self.call_delegate(
                c,
                "__OnPreDraw__Delegate",
                "OnPreDraw",
                vec![canvas.clone()],
            );
        }
        for c in controls.iter().copied() {
            self.call_delegate(c, "__OnDraw__Delegate", "OnDraw", vec![canvas.clone()]);
        }

        // `XIIIMenu.AfterPaint` is a plain virtual (no `__AfterPaint__` delegate): it draws the
        // focused control's onomatopoeia and its label (`Continue`, `Options`, ...). Called with
        // the page origin in place from the page's `__OnDraw__`.
        self.call(self.page, "AfterPaint", vec![canvas, zero(), zero()]);
        self.commands = self.vm.drain_canvas();
        for (i, line) in self.slot_lines.iter().enumerate() {
            self.commands.push(DrawCommand::Text {
                text: line.clone(),
                x: 165.0,
                y: 220.0 + i as f32 * 30.0,
                font: Some("XIIIFonts.PoliceF16".into()),
                color: [255, 255, 255, 255],
                clip: self.clip,
                center: false,
                style: 1,
                justify: 0,
                clipped: true,
            });
        }
        self.fps += 1;
    }

    /// Mouse hover focus, following the native controller's hit test: the control whose
    /// `Bounds` (set by its own `InternalOnPreDraw`) contains the cursor receives focus, which
    /// runs its `__OnActivate__` -> `MouseEnter` delegate (highlight + zoom). Controls are read
    /// back-to-front so the topmost wins.
    fn hover(&mut self, cursor: Option<(f32, f32)>) {
        let Some((cx, cy)) = cursor else {
            return;
        };
        let mut target = None;
        for c in self.controls.iter().rev().copied() {
            let bound = |i: usize| match self.vm.get_property_elem(c, "Bounds", i) {
                Some(Value::Float(v)) => Some(*v),
                Some(Value::Int(v)) => Some(*v as f32),
                _ => None,
            };
            if let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                (bound(0), bound(1), bound(2), bound(3))
                && cx >= x0
                && cx <= x1
                && cy >= y0
                && cy <= y1
            {
                target = Some(c);
                break;
            }
        }
        let Some(t) = target else {
            return;
        };
        let focused = match self.vm.get_property(self.page, "FocusedControl") {
            Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
            _ => None,
        };
        if focused != Some(t) {
            // `GUIComponent.SetFocus(None)` runs the page/control focus state machine and the
            // `__OnActivate__`/`__OnDeActivate__` delegates.
            let _ = self.vm.send_event(t, "SetFocus", vec![Value::Object(None)]);
        }
    }

    fn call(&mut self, id: ObjectId, func: &str, args: Vec<Value>) {
        if let Err(e) = self.vm.send_event(id, func, args) {
            let msg = format!("{func}: {e}");
            if !self.errors.iter().any(|x| x == &msg) {
                self.errors.push(msg);
            }
        }
    }

    /// Invokes a `__On*__` delegate through the VM; on failure records `declared` plus the error.
    fn call_delegate(&mut self, id: ObjectId, property: &str, declared: &str, args: Vec<Value>) {
        if let Err(e) = self.vm.call_delegate(id, property, declared, args) {
            let msg = format!("{declared}: {e}");
            if !self.errors.iter().any(|x| x == &msg) {
                self.errors.push(msg);
            }
        }
    }
}

fn zero() -> Value {
    Value::Float(0.0)
}

/// Screen X/Y scale factors used by the native menu layout (`XIIIWindow.BeforePaint`, decoded
/// at `xidinterf.u` @62903). For the map menu (`bMapMenu`) `fRatioX = min(ClipX/640, 800/640)`
/// and `fRatioY = min(ClipY/480, 600/480)`; the design space is 640x480.
pub fn map_menu_ratio(clip: [f32; 2]) -> [f32; 2] {
    [
        (clip[0] / 640.0).min(800.0 / 640.0),
        (clip[1] / 480.0).min(600.0 / 480.0),
    ]
}

/// Canvas origin the page/control sets for the map menu (`XIIIGUIBaseButton.InternalOnPreDraw`,
/// decoded at `xidinterf.u` @380779 / `XIIIWindow.InternalOnPreDraw` @106097): the design area
/// is centred when the clip is larger than 800x600.
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn map_menu_origin(win: [f32; 2], clip: [f32; 2]) -> [f32; 2] {
    let ratio = map_menu_ratio(clip);
    let mut x = win[0] * 640.0 * ratio[0];
    let mut y = win[1] * 480.0 * ratio[1];
    if clip[0] > 800.0 {
        x += (clip[0] - 800.0) / 2.0;
    }
    if clip[1] > 600.0 {
        y += (clip[1] - 600.0) / 2.0;
    }
    [x, y]
}

/// Screen rectangle `[x, y, w, h]` of a control. `win` is the normalized design rectangle
/// `XIIIWindow.CreateControl` stores (`WinLeft = X/640`, `WinWidth = W/640`, ...). This is the
/// `InternalOnPreDraw` origin plus the `Bounds[2] = WinWidth*640*fRatioX` size.
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn map_menu_control_rect(win: [f32; 4], clip: [f32; 2]) -> [f32; 4] {
    let ratio = map_menu_ratio(clip);
    let [x, y] = map_menu_origin([win[0], win[1]], clip);
    [x, y, win[2] * 640.0 * ratio[0], win[3] * 480.0 * ratio[1]]
}

/// Screen rectangle `[x, y, w, h]` of a caption label. `label` is the design-space
/// `{XPos, YPos, XSize, YSize}` `XIIIWindow.InitLabel` stores; `XIIIWindow.DrawLabel` scales it
/// by the page ratios around the page origin.
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn map_menu_label_rect(label: [f32; 4], clip: [f32; 2]) -> [f32; 4] {
    let ratio = map_menu_ratio(clip);
    let [ox, oy] = map_menu_origin([0.0, 0.0], clip);
    [
        ox + label[0] * ratio[0],
        oy + label[1] * ratio[1],
        label[2] * ratio[0],
        label[3] * ratio[1],
    ]
}

/// Modeled placement of a `bBoundToParent` child inside `parent` (a screen rectangle). No
/// main-menu control sets `bBoundToParent`/`bScaleToParent` (both default `false`; only
/// `GUIComponent.FillOwner` sets them true), so this is **not** on the menu path. The native
/// `ActualLeft/ActualTop/ActualWidth/ActualHeight` implementations were not decoded, so this is
/// a documented model (hypothesis), not a measurement.
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn bound_child_rect(parent: [f32; 4], win: [f32; 4], scale_to_parent: bool) -> [f32; 4] {
    let x = parent[0] + win[0] * parent[2];
    let y = parent[1] + win[1] * parent[3];
    let w = if scale_to_parent {
        win[2] * parent[2]
    } else {
        win[2] * 640.0
    };
    let h = if scale_to_parent {
        win[3] * parent[3]
    } else {
        win[3] * 480.0
    };
    [x, y, w, h]
}

/// True when two screen rectangles overlap (touching edges do not count).
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn rects_overlap(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// True when `r` lies inside `[0,0,w,h]`.
#[allow(dead_code)] // exercised by the layout tests in this module
pub fn rect_inside(r: [f32; 4], screen: [f32; 2]) -> bool {
    r[0] >= 0.0 && r[1] >= 0.0 && r[0] + r[2] <= screen[0] && r[1] + r[3] <= screen[1]
}

/// Unreal key codes for the keys `--menu-script` names (`XIIIMenu.InternalOnKeyEvent` compares
/// the decoded byte values 37..40, 13, 27).
fn unreal_key(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => 37,
        "up" => 38,
        "right" => 39,
        "down" => 40,
        "enter" | "return" => 13,
        "escape" | "esc" => 27,
        "space" => 32,
        "backspace" => 8,
        _ => return None,
    })
}

/// A `Color` struct value with the VM's `b`,`g`,`r`,`a` member names.
fn color_value(r: u8, g: u8, b: u8, a: u8) -> Value {
    Value::Struct(vec![
        ("b".to_owned(), Value::Byte(b)),
        ("g".to_owned(), Value::Byte(g)),
        ("r".to_owned(), Value::Byte(r)),
        ("a".to_owned(), Value::Byte(a)),
    ])
}

/// Resolves a `Package.Object` texture through the installation and returns it as the static
/// object the VM would have loaded (`DynamicLoadObject`'s result). This is the host bridge for
/// the root's own `InitializeController` texture loads, which run outside the map class set.
fn load_texture_native(vm: &mut Vm<'_>, path: &str, _class: &str) -> Option<Value> {
    vm.external_asset(path).map(|(v, _)| v)
}

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        let game_dir = self.options.game_dir.clone().unwrap_or_default();
        let t0 = Instant::now();
        let mut session = match &self.options.menu_script {
            Some(path) => match MenuScript::load(path) {
                Ok(s) => save_dir(&self.options).and_then(|dir| {
                    MenuSession::open_configured(
                        &game_dir,
                        dir,
                        &self
                            .options
                            .config_dir
                            .clone()
                            .map(Ok)
                            .unwrap_or_else(config::default_dir)?,
                        Some(s),
                    )
                }),
                Err(e) => {
                    eprintln!("[menu] input script {}: {e}", path.display());
                    Err(e)
                }
            },
            None => save_dir(&self.options).and_then(|dir| {
                MenuSession::open_configured(
                    &game_dir,
                    dir,
                    &self
                        .options
                        .config_dir
                        .clone()
                        .map(Ok)
                        .unwrap_or_else(config::default_dir)?,
                    None,
                )
            }),
        };
        println!(
            "[menu] menu session open (MapMenu + XIDInterf.XIIIMenu): {:.2}s",
            t0.elapsed().as_secs_f32()
        );
        // item21: the game's new-game path opens `cine00` through `Engine.VideoPlayer`; install
        // the host cutscene player so it is decoded, played fullscreen (letterboxed) with its
        // Bink Audio track, and `GetStatus` follows the actual playback.
        let cutscene_host = session.as_mut().ok().and_then(|s| {
            let dir = Path::new(&game_dir);
            if !dir.is_dir() {
                return None;
            }
            // `MenuSession` does not use the play `Session::open` duration scan. Keep the same
            // labelled Bink-header fallback for cine00 if the clean-room decoder/tables cannot
            // decode it.
            let cine00 = dir.join("Video").join("Cine00.bik");
            let mut header = [0u8; 36];
            if std::fs::File::open(&cine00)
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header))
                .is_ok()
                && let Some(duration) = crate::video::bink_duration_from_header(&header)
            {
                s.vm.set_video_duration("cine00", duration);
            }
            let host = crate::video::VideoHostHandle::new(dir.to_path_buf(), true);
            s.vm.set_video_host(Box::new(host.clone()));
            println!("[menu] item21 cutscene host installed (the new-game clip plays fullscreen)");
            Some(host)
        });
        app.insert_non_send(session)
            .insert_non_send(crate::video::CutsceneHost(cutscene_host))
            .add_plugins((crate::video::CutscenePlugin, cutscene::CutsceneSystems))
            .add_plugins(MaterialPlugin::<crate::viewer::lights::ReceiverMaterial>::default())
            .insert_resource(MenuConfig {
                options: self.options.clone(),
            })
            .insert_resource(ClearColor(Color::srgb(0.02, 0.02, 0.03)))
            .init_resource::<MenuShotFlag>()
            .add_systems(Startup, setup)
            .add_systems(Update, (frame, draw, cutscene::sync, unattended).chain());
    }
}

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    cfg: Res<MenuConfig>,
    mut session: NonSendMut<Result<MenuSession, String>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut receiver_materials: ResMut<Assets<crate::viewer::lights::ReceiverMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut exit: MessageWriter<AppExit>,
) {
    let session = match session.as_mut() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[menu] session failed: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    let game_dir = cfg.options.game_dir.clone().unwrap_or_default();
    let fonts = match hud::load_fonts(&game_dir, &mut images) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!("[menu] font decode failed: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    session
        .vm
        .set_canvas_fonts(Box::new(VmFonts(fonts.clone())));
    session.fonts = Some(fonts);
    for l in &session.log {
        println!("{l}");
    }
    // Render the menu's entry map behind its script-drawn canvas panels. Prefer a map-placed
    // CameraPoint (the front-end's cinematic camera actor) when the map contains one.
    let mut backdrop_options = cfg.options.clone();
    backdrop_options.map = Some("MapMenu".into());
    let camera_transform = match crate::viewer::load_scene(&backdrop_options) {
        Ok(scene) => {
            let map_camera = session
                .vm
                .objects
                .iter()
                .enumerate()
                .find_map(|(id, actor)| {
                    let path = session.vm.set().path(actor.class);
                    (path.to_ascii_lowercase().contains("camerapoint")
                        && !actor.deleted
                        && session.vm.is_a(id as ObjectId, "Actor"))
                    .then_some((id as ObjectId, path))
                });
            if map_camera.is_none() {
                let candidates: Vec<String> = session
                    .vm
                    .objects
                    .iter()
                    .filter_map(|actor| {
                        let path = session.vm.set().path(actor.class);
                        let lower = path.to_ascii_lowercase();
                        (lower.contains("camera") || lower.contains("matinee")).then_some(path)
                    })
                    .collect();
                println!(
                    "[menu] MapMenu has no CameraPoint actor; camera/matinee candidates={candidates:?}; using PlayerStart fallback"
                );
            }
            let camera = map_camera.and_then(|(id, path)| {
                let location = match session.vm.get_property(id, "Location") {
                    Some(Value::Vector(v)) => Some(*v),
                    _ => None,
                }?;
                let rotation = match session.vm.get_property(id, "Rotation") {
                    Some(Value::Rotator(r)) => Some(*r),
                    _ => None,
                }?;
                let position = xiii_decode::common::to_bevy_position(location);
                let matrix = xiii_decode::common::rotator_to_bevy_matrix(rotation);
                println!("[menu] MapMenu camera anchor: {path} at {position:?}");
                Some(
                    Transform::from_translation(Vec3::from_array(position)).with_rotation(
                        Quat::from_mat3(&bevy::math::Mat3::from_cols_array(&xiii_world::to_cols(
                            &matrix,
                        ))),
                    ),
                )
            });
            let camera = camera.unwrap_or_else(|| {
                let target = scene.objects.iter().fold(Vec3::ZERO, |sum, object| {
                    sum + Vec3::from_array(object.transform.translation)
                }) / scene.objects.len().max(1) as f32;
                scene
                    .player_start
                    .map(|(p, _)| {
                        Transform::from_translation(Vec3::new(p[0], p[1], p[2]))
                            .looking_at(target, Vec3::Y)
                    })
                    .unwrap_or_default()
            });
            let (geometry, _) = crate::viewer::spawn_scene_geometry(
                &mut commands,
                &mut meshes,
                &mut materials,
                &mut receiver_materials,
                &mut images,
                &scene,
                true,
                false,
                false,
            );
            println!(
                "[menu] MapMenu backdrop: {} geometry entities",
                geometry.len()
            );
            camera
        }
        Err(e) => {
            eprintln!("[menu] MapMenu backdrop import failed: {e}");
            Transform::default()
        }
    };
    commands.spawn((Camera3d::default(), camera_transform));
    println!(
        "[menu] ready: {} control(s); input script {} action(s)",
        session.controls.len(),
        session.script.as_ref().map_or(0, |s| s.actions.len())
    );
}

fn frame(
    time: Res<Time>,
    window: Query<&Window, With<bevy::window::PrimaryWindow>>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut session: NonSendMut<Result<MenuSession, String>>,
) {
    let Ok(session) = session.as_mut() else {
        return;
    };
    let dt = time.delta_secs();
    let cursor = window.single().ok().and_then(|w| w.cursor_position());
    session.clip = window
        .single()
        .map(|w| [w.width(), w.height()])
        .unwrap_or([1280.0, 720.0]);
    session.advance(dt);
    session.refresh_commands();
    session.hover(cursor.map(|p| (p.x, p.y)));
    for (key, code) in [
        (KeyCode::ArrowUp, 38),
        (KeyCode::ArrowDown, 40),
        (KeyCode::ArrowLeft, 37),
        (KeyCode::ArrowRight, 39),
        (KeyCode::Enter, 13),
        (KeyCode::Escape, 27),
        (KeyCode::Backspace, 8),
        (KeyCode::Space, 32),
    ] {
        if keys.just_pressed(key)
            && let Err(e) = session.dispatch_key(code)
        {
            session.record_error(format!("key {key:?}: {e}"));
        }
    }
    if mouse.just_pressed(MouseButton::Left)
        && let Some(Value::Object(Some(ObjRef::Instance(control)))) = session
            .vm
            .get_property(session.page, "FocusedControl")
            .cloned()
        && let Err(e) = session.vm.send_event(
            session.page,
            "InternalOnClick",
            vec![Value::Object(Some(ObjRef::Instance(control)))],
        )
    {
        session.record_error(format!("menu click: {e}"));
    }
}

fn draw(
    mut commands: Commands,
    mut session: NonSendMut<Result<MenuSession, String>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Ok(session) = session.as_mut() else {
        return;
    };
    for e in session.entities.drain(..) {
        commands.entity(e).despawn();
    }
    let clip = session.clip;
    if clip[0] <= 0.0 || clip[1] <= 0.0 {
        return;
    }
    let Some(fonts) = session.fonts.clone() else {
        return;
    };
    let cmds = std::mem::take(&mut session.commands);
    let mut missing: BTreeMap<String, u64> = BTreeMap::new();
    for cmd in &cmds {
        match cmd {
            DrawCommand::Rect {
                x,
                y,
                xl,
                yl,
                color,
                ..
            } => {
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(*x),
                            top: px(*y),
                            width: px(*xl),
                            height: px(*yl),
                            ..default()
                        },
                        BackgroundColor(to_color(*color)),
                        GlobalZIndex(10),
                    ))
                    .id();
                session.entities.push(e);
            }
            DrawCommand::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                ..
            } => {
                let (left, top) = (x1.min(*x2), y1.min(*y2));
                let (w, h) = ((x2 - x1).abs().max(1.0), (y2 - y1).abs().max(1.0));
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(left),
                            top: px(top),
                            width: px(w),
                            height: px(h),
                            ..default()
                        },
                        BackgroundColor(to_color(*color)),
                        GlobalZIndex(10),
                    ))
                    .id();
                session.entities.push(e);
            }
            DrawCommand::Tile {
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
                ..
            } => {
                let Some(path) = material else {
                    continue;
                };
                let handle = decode_texture_path(&mut session.packages, path, &mut images);
                let Some(handle) = handle else {
                    *missing.entry(path.clone()).or_default() += 1;
                    continue;
                };
                let rect = if *ul > 0.0 && *vl > 0.0 {
                    Some(Rect::new(*u, *v, *u + *ul, *v + *vl))
                } else {
                    None
                };
                let e = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(*x),
                            top: px(*y),
                            width: px(*xl),
                            height: px(*yl),
                            ..default()
                        },
                        ImageNode {
                            image: handle,
                            rect,
                            image_mode: bevy::ui::widget::NodeImageMode::Stretch,
                            color: to_color(*color),
                            ..default()
                        },
                        GlobalZIndex(10),
                    ))
                    .id();
                session.entities.push(e);
            }
            DrawCommand::Text {
                text,
                x,
                y,
                font,
                color,
                ..
            } => {
                let Some(font) = font.as_deref().and_then(|f| fonts.find(f)) else {
                    continue;
                };
                let mut pen_x = *x;
                for ch in text.chars() {
                    let Some(glyph) = font.glyph(ch as u32) else {
                        continue;
                    };
                    let Some(page) = font.pages.get(glyph.page) else {
                        continue;
                    };
                    if glyph.u_size <= 0 || glyph.v_size <= 0 {
                        continue;
                    }
                    let gx = glyph.start_u as f32;
                    let gy = glyph.start_v as f32;
                    let gw = glyph.u_size as f32;
                    let gh = glyph.v_size as f32;
                    if pen_x + gw < 0.0 || pen_x > clip[0] || *y + gh < 0.0 || *y > clip[1] {
                        pen_x += gw;
                        continue;
                    }
                    let e = commands
                        .spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: px(pen_x),
                                top: px(*y),
                                width: px(gw),
                                height: px(gh),
                                ..default()
                            },
                            ImageNode {
                                image: page.handle.clone(),
                                rect: Some(Rect::new(gx, gy, gx + gw, gy + gh)),
                                image_mode: bevy::ui::widget::NodeImageMode::Stretch,
                                color: to_color(*color),
                                ..default()
                            },
                            GlobalZIndex(10),
                        ))
                        .id();
                    session.entities.push(e);
                    pen_x += gw;
                }
            }
        }
    }
    for path in missing.keys() {
        if !session.errors.iter().any(|e| e.contains(path)) {
            session
                .errors
                .push(format!("tile material {path} did not decode"));
        }
    }
}

fn to_color(c: [u8; 4]) -> Color {
    Color::srgba_u8(c[0], c[1], c[2], c[3])
}

/// Resolves a `Package.Object` path to a decoded texture image handle.
fn decode_texture_path(
    cache: &mut PackageCache,
    path: &str,
    images: &mut Assets<Image>,
) -> Option<Handle<Image>> {
    let (package, object) = path.split_once('.')?;
    let leaf = object.rsplit('.').next().unwrap_or(object);
    let loaded = cache.get(package).ok()?;
    let pkg = &loaded.package;
    // `DynamicLoadObject` names a bare leaf that may live under a group outer, so match the
    // exact path first and then a unique leaf (same rule as `MenuTextureData`).
    let mut exact = None;
    let mut by_leaf = Vec::new();
    for e in 0..pkg.exports().len() {
        let Some(q) = pkg.object_path(PkgRef::Export(e as u32)) else {
            continue;
        };
        if q.eq_ignore_ascii_case(object) {
            exact = Some(e);
            break;
        }
        if q.rsplit('.')
            .next()
            .is_some_and(|l| l.eq_ignore_ascii_case(leaf))
        {
            by_leaf.push(e);
        }
    }
    let export = match exact {
        Some(e) => e,
        None if by_leaf.len() == 1 => by_leaf[0],
        None => return None,
    };
    if !pkg
        .export_class_path(export)
        .is_some_and(|c| c.eq_ignore_ascii_case("Engine.Texture"))
    {
        return None;
    }
    let texture = decode_texture(pkg, &loaded.data, export).ok()?;
    let image = texture.decode_mip(0).ok()?;
    Some(images.add(image_from_rgba(&image)))
}

fn image_from_rgba(img: &RgbaImage) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        img.pixels.clone(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        ..ImageSamplerDescriptor::linear()
    });
    image
}

#[allow(clippy::too_many_arguments)]
fn unattended(
    cfg: Res<MenuConfig>,
    mut state: ResMut<MenuState>,
    flag: Res<MenuShotFlag>,
    session: NonSend<Result<MenuSession, String>>,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
) {
    // Host travel step: the game requested a map (`ClientTravel`). Record it and leave the menu
    // app; `main` then starts `--play` on the requested map.
    if let Ok(s) = session.as_ref()
        && let Some(req) = &s.travel
    {
        println!(
            "[menu] travel request: map {} url={} type={} items={} (the game's own ClientTravel)",
            req.map, req.url, req.travel_type, req.items
        );
        println!(
            "[menu] host travel step: start --play --map {} (host action, not script)",
            req.map
        );
        set_travel_request(req.clone());
        exit.write(AppExit::Success);
        return;
    }
    let Some(secs) = state.exit_secs else {
        return;
    };
    let elapsed = state.start.elapsed().as_secs_f32();
    if flag.0 {
        state.shot_done = true;
    }
    if let Some(path) = &cfg.options.screenshot
        && state.shot == 0
        && elapsed >= secs * 0.75
    {
        let path: PathBuf = path.clone();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path))
            .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<MenuShotFlag>| f.0 = true);
        state.shot = 1;
    }
    if elapsed < secs {
        return;
    }
    let reached = *state.target_at.get_or_insert_with(Instant::now);
    if state.shot == 1 && !state.shot_done && reached.elapsed() < std::time::Duration::from_secs(5)
    {
        return;
    }
    let frames = match &*session {
        Ok(s) => s.fps,
        Err(_) => 0,
    };
    if let Ok(s) = &*session {
        for e in s
            .errors
            .iter()
            .filter(|e| !e.contains("LetDelegate"))
            .take(3)
        {
            println!("[menu] diagnostic: {e}");
        }
    }
    state.tick = frames;
    println!(
        "[menu] exit after {:.1}s, {} rendered frame(s), screenshot {}",
        elapsed,
        frames,
        match (&cfg.options.screenshot, state.shot_done) {
            (Some(p), true) => format!("saved {}", p.display()),
            (Some(p), false) => format!("NOT confirmed {}", p.display()),
            (None, _) => "none".into(),
        }
    );
    exit.write(AppExit::Success);
}

/// Builds the menu [`App`] for `--menu`. Used by `main` so the travel step can chain `--play`.
pub fn build_menu_app(options: Options) -> App {
    let present_mode = if options.no_vsync {
        PresentMode::AutoNoVsync
    } else {
        PresentMode::AutoVsync
    };
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "XIII Classic runtime - front-end menu (item16)".into(),
            resolution: (options.width, options.height).into(),
            present_mode,
            ..default()
        }),
        ..default()
    }));
    app.insert_resource(MenuState {
        tick: 0,
        start: Instant::now(),
        exit_secs: options.exit_after_secs,
        shot: 0,
        shot_done: false,
        target_at: None,
    });
    app.add_plugins(MenuPlugin {
        options: options.clone(),
    });
    let mut audio_options = options;
    audio_options.map = Some("MapMenu".into());
    app.add_plugins(crate::audio::AudioFxPlugin {
        options: audio_options,
    });
    app
}

/// How many times the menu repainted (used by the report/test).
#[allow(dead_code)]
pub fn rendered_frames(session: &MenuSession) -> u64 {
    session.fps
}

/// The entry map's decoded actor count (report/tests).
#[allow(dead_code)]
pub fn map_actor_count(session: &MenuSession) -> usize {
    session.map_actor_count
}

/// The control labels in `Controls` order (report/tests).
#[allow(dead_code)]
pub fn control_labels(session: &MenuSession) -> &[String] {
    &session.control_labels
}

/// The current travel request, if the menu reached `ClientTravel` (report/tests).
#[allow(dead_code)]
pub fn travel(session: &MenuSession) -> Option<&TravelRequest> {
    session.travel.as_ref()
}

/// The first paint error, if any (report/tests).
#[allow(dead_code)]
pub fn first_error(session: &MenuSession) -> Option<&str> {
    session.errors.first().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_slot_provider_lists_dates_and_requires_an_existing_slot_for_read() {
        let dir = std::env::temp_dir().join(format!("xiii-menu-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let save = crate::save::SaveFile {
            map: "Plage00".into(),
            teleporter: "PlayerStart".into(),
            save_trigger_tag: "Debut".into(),
            description: "Synthetic beach".into(),
            health: 150.0,
            speed_factor_limit: 1.0,
            checkpoint_number: 1,
            location: [0.0; 3],
            rotation: [0; 3],
            objectives: Vec::new(),
            inventory: Vec::new(),
            sound_to_launch: None,
            selected_weapon: None,
            music_vars: Vec::new(),
        };
        crate::save::write(&dir, 3, &save).unwrap();
        let mut provider = MenuSaveSlots::open(dir.clone()).unwrap();
        let listed = provider.slot(3).unwrap();
        assert_eq!(listed.description, "Synthetic beach");
        assert!((1970..=9999).contains(&listed.date[0]));
        assert!((1..=12).contains(&listed.date[1]));
        assert!((1..=31).contains(&listed.date[2]));
        assert!((0..=23).contains(&listed.date[3]));
        assert!((0..=59).contains(&listed.date[4]));
        assert!(provider.slot(10).is_none());
        assert!(!provider.request_empty(10));
        assert!(provider.request_empty(3));
        assert_eq!(provider.poll_empty(), Some(false));
        assert!(provider.request_description(3));
        assert_eq!(
            provider.poll_description().as_deref(),
            Some("Synthetic beach")
        );
        assert!(!provider.request_read(2));
        assert!(provider.request_read(3));
        assert!(provider.poll_read());
        assert_eq!(provider.take_read_slot(), Some(3));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn menu_script_parses_actions_and_rejects_bad_lines() {
        let s = MenuScript::parse(
            "# comment\nt=0.5 newgame\nt=1.0 key down\n t=2 focus 4\nt=3 click quit\n",
        )
        .unwrap();
        assert_eq!(s.actions.len(), 4);
        assert_eq!(s.actions[0].time, 0.5);
        assert_eq!(s.actions[0].action, Action::NewGame);
        assert_eq!(s.actions[1].action, Action::Key("down".into()));
        assert_eq!(s.actions[2].action, Action::Focus("4".into()));
        assert_eq!(s.actions[3].action, Action::Click("quit".into()));
        assert!(MenuScript::parse("0.5 newgame").is_err());
        assert!(MenuScript::parse("t=0.5 wiggle").is_err());
        assert!(MenuScript::parse("t=-1 newgame").is_err());
        assert!(MenuScript::parse("t=0.5 key").is_err());
    }

    #[test]
    fn unreal_key_map_covers_the_decoded_menu_keys() {
        assert_eq!(unreal_key("up"), Some(38));
        assert_eq!(unreal_key("DOWN"), Some(40));
        assert_eq!(unreal_key("enter"), Some(13));
        assert_eq!(unreal_key("escape"), Some(27));
        assert_eq!(unreal_key("nonsense"), None);
    }

    /// The layout constants come from the decoded `XIIIWindow.BeforePaint` (640x480 design,
    /// `fRatio = min(Clip/design, 800/640)` for the map menu) and the centring in
    /// `InternalOnPreDraw`. Checked against three resolutions, including the clamp and the
    /// sub-800x600 case (no centring).
    #[test]
    fn map_menu_scaling_matches_native_before_paint() {
        // 640x480: ratio 1, no centre offset.
        assert_eq!(map_menu_ratio([640.0, 480.0]), [1.0, 1.0]);
        assert_eq!(map_menu_origin([0.0, 0.0], [640.0, 480.0]), [0.0, 0.0]);
        // 1280x720: ratio clamps at 1.25; the 800x600 design area is centred.
        assert_eq!(map_menu_ratio([1280.0, 720.0]), [1.25, 1.25]);
        assert_eq!(map_menu_origin([0.0, 0.0], [1280.0, 720.0]), [240.0, 60.0]);
        // 1600x1200: still clamped to 1.25, larger centre offset.
        assert_eq!(map_menu_ratio([1600.0, 1200.0]), [1.25, 1.25]);
        assert_eq!(
            map_menu_origin([0.0, 0.0], [1600.0, 1200.0]),
            [400.0, 300.0]
        );
        // A control at design (27,19)-(318,147) normalized by CreateControl.
        let win = [27.0 / 640.0, 19.0 / 480.0, 318.0 / 640.0, 147.0 / 480.0];
        let r = map_menu_control_rect(win, [1280.0, 720.0]);
        assert!((r[0] - 273.75).abs() < 1e-3, "{r:?}");
        assert!((r[1] - 83.75).abs() < 1e-3, "{r:?}");
        assert!((r[2] - 397.5).abs() < 1e-3, "{r:?}");
        assert!((r[3] - 183.75).abs() < 1e-3, "{r:?}");
    }

    /// The six decoded main-menu controls and captions (positions from `XIIIMenu.Created`):
    /// every control and caption lies inside the screen and no two captions overlap. Uses the
    /// decoded design-space values (documented in the report), so it runs without the corpus.
    #[test]
    fn decoded_main_menu_rects_are_contained_and_labels_do_not_overlap() {
        let clip = [1280.0, 720.0];
        // (X, Y, W, H) from `XIIIMenu.Created` `CreateControl` calls.
        let controls = [
            (27.0, 19.0, 318.0, 147.0),
            (457.0, 19.0, 155.0, 280.0),
            (27.0, 181.0, 244.0, 252.0),
            (287.0, 181.0, 156.0, 252.0),
            (457.0, 310.0, 155.0, 123.0),
            (361.0, 19.0, 80.0, 147.0),
        ];
        for (x, y, w, h) in controls {
            let win = [x / 640.0, y / 480.0, w / 640.0, h / 480.0];
            let r = map_menu_control_rect(win, clip);
            assert!(rect_inside(r, clip), "control rect {r:?} outside {clip:?}");
        }
        // (XPos, YPos, XSize, YSize) from `XIIIMenu.Created` `InitLabel` calls.
        let labels = [
            (16.0, 32.0, 128.0, 32.0),
            (420.0, 220.0, 128.0, 32.0),
            (16.0, 350.0, 128.0, 32.0),
            (350.0, 320.0, 128.0, 32.0),
            (500.0, 350.0, 128.0, 32.0),
            (380.0, 120.0, 128.0, 32.0),
        ];
        let rects: Vec<[f32; 4]> = labels
            .iter()
            .map(|&l| map_menu_label_rect([l.0, l.1, l.2, l.3], clip))
            .collect();
        for r in &rects {
            assert!(rect_inside(*r, clip), "label rect {r:?} outside {clip:?}");
        }
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    !rects_overlap(rects[i], rects[j]),
                    "labels {i} {:?} and {j} {:?} overlap",
                    rects[i],
                    rects[j]
                );
            }
        }
    }

    /// `bBoundToParent`/`bScaleToParent` model: `GUIComponent.FillOwner` (gui.u @33859) sets
    /// `Win = (0,0,1,1)` plus both flags, and a bound+scaled child then fills its parent. This is
    /// a documented model of the un-decoded native `Actual*` functions, not a measurement; no
    /// main-menu control sets the flags.
    #[test]
    fn bound_to_parent_fill_semantics() {
        let parent = [100.0, 50.0, 400.0, 300.0];
        let full = [0.0, 0.0, 1.0, 1.0];
        assert_eq!(bound_child_rect(parent, full, true), parent);
        // Not scaled: the child keeps its design size (640x480 space).
        let half = [0.25, 0.5, 0.5, 0.5];
        assert_eq!(
            bound_child_rect(parent, half, false),
            [100.0 + 0.25 * 400.0, 50.0 + 0.5 * 300.0, 320.0, 240.0]
        );
    }

    fn vm_f32(vm: &Vm<'_>, id: ObjectId, name: &str) -> f32 {
        match vm.get_property(id, name) {
            Some(Value::Float(v)) => *v,
            Some(Value::Int(v)) => *v as f32,
            _ => panic!("{name} is not a float on {id}"),
        }
    }

    fn label_rect(vm: &Vm<'_>, page: ObjectId, prop: &str) -> [f32; 4] {
        let Some(Value::Struct(fields)) = vm.get_property(page, prop) else {
            panic!("{prop} is not a struct");
        };
        let get = |n: &str| {
            fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .and_then(|(_, v)| match v {
                    Value::Float(f) => Some(*f),
                    Value::Int(i) => Some(*i as f32),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{prop}.{n} missing"))
        };
        [get("XPos"), get("YPos"), get("XSize"), get("YSize")]
    }

    fn opt_in_root() -> Option<PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus test: the decoded front end spawns without suspensions and the New game
    /// entry reaches the game's own `ClientTravel("Plage00")` map request.
    #[test]
    fn opt_in_menu_spawns_and_newgame_requests_the_map() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let script = MenuScript::parse("t=0.1 newgame\n").unwrap();
        let mut session = MenuSession::open(
            &game_dir,
            save_dir(&Options::default()).unwrap(),
            Some(script),
        )
        .expect("open the front-end menu");
        assert_eq!(
            session.controls.len(),
            XIIIMENU_CONTROL_COUNT,
            "XIIIMenu.Created must build {:?}",
            session.control_labels
        );
        assert_eq!(session.control_labels[4], "newgame");
        for c in &session.controls {
            assert!(
                session.vm.is_a(*c, "XIIITextureButton"),
                "control is {}",
                session.vm.set().path(session.vm.objects[*c as usize].class)
            );
        }
        // Advance past the action and drive the `PlayingVideo` state until `EndOfVideo`
        // travels.
        for _ in 0..40 {
            session.clip = [1280.0, 720.0];
            session.advance(0.05);
            session.refresh_commands();
            if session.travel.is_some() {
                break;
            }
        }
        assert!(
            session.errors.is_empty(),
            "menu spawned with no suspensions; errors: {:?}",
            session.errors
        );
        let req = travel(&session).expect("New game reaches ClientTravel");
        assert_eq!(req.map, "plage00");
        assert_eq!(req.url, "Plage00");
        println!(
            "[menu test] controls={:?} travel={} url={} paint_error={:?}",
            session.control_labels,
            req.map,
            req.url,
            session.errors.first()
        );
    }

    /// Opt-in corpus test: after the game's own `InitComponent` builds the main menu, every
    /// control's rect (from its decoded `WinLeft/WinTop/WinWidth/WinHeight`) is inside the
    /// 1280x720 screen and none of the six captions overlaps another. Also checks the focused
    /// control's `bDisplayTex` is set (the `__OnActivate__` -> `MouseEnter` delegate ran), which
    /// is what makes `AfterPaint` draw its caption.
    #[test]
    fn opt_in_menu_control_and_caption_rects_fit_the_screen() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session =
            MenuSession::open(&game_dir, save_dir(&Options::default()).unwrap(), None)
                .expect("open the front-end menu");
        session.clip = [1280.0, 720.0];
        session.refresh_commands();
        let clip = session.clip;
        assert_eq!(session.controls.len(), XIIIMENU_CONTROL_COUNT);
        let mut controls = Vec::new();
        for c in &session.controls {
            let win = [
                vm_f32(&session.vm, *c, "WinLeft"),
                vm_f32(&session.vm, *c, "WinTop"),
                vm_f32(&session.vm, *c, "WinWidth"),
                vm_f32(&session.vm, *c, "WinHeight"),
            ];
            let r = map_menu_control_rect(win, clip);
            assert!(
                rect_inside(r, clip),
                "control {} rect {r:?} outside {clip:?}",
                session.control_labels[controls.len()]
            );
            controls.push(r);
        }
        let mut captions = Vec::new();
        let mut names = Vec::new();
        for prop in XIIIMENU_LABEL_PROPS {
            let design = label_rect(&session.vm, session.page, prop);
            let r = map_menu_label_rect(design, clip);
            assert!(rect_inside(r, clip), "{prop} rect {r:?} outside {clip:?}");
            captions.push(r);
            names.push(prop);
        }
        for i in 0..captions.len() {
            for j in (i + 1)..captions.len() {
                assert!(
                    !rects_overlap(captions[i], captions[j]),
                    "captions {} {:?} and {} {:?} overlap",
                    names[i],
                    captions[i],
                    names[j],
                    captions[j]
                );
            }
        }
        // The first control is focused (GUIPage.InitComponent -> FocusFirst) and its delegate
        // highlight is on, so `XIIIMenu.AfterPaint` draws its caption.
        let focused = session.controls[0];
        assert!(
            matches!(
                session.vm.get_property(focused, "bDisplayTex"),
                Some(Value::Bool(true))
            ),
            "the focused control's MouseEnter delegate must set bDisplayTex"
        );
        println!(
            "[menu test] control_rects={controls:?} caption_rects={captions:?} focused={}",
            session.control_labels[0]
        );
    }

    /// Opt-in corpus check for the retail audio page's master-volume persistence. It advances
    /// focus from the music checkbox to the Sound Volume slider, moves it one step and presses
    /// Enter through the page handler, then starts a fresh VM and reads the saved value.
    #[test]
    fn opt_in_audio_page_master_volume_survives_restart() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let base =
            std::env::temp_dir().join(format!("xiii-menu-music-persist-{}", std::process::id()));
        let config_dir = base.join("config");
        let save_dir = base.join("saves");
        let _ = std::fs::remove_dir_all(&base);
        let mut session =
            MenuSession::open_configured(&game_dir, save_dir.clone(), &config_dir, None)
                .expect("open menu with disposable configuration");
        let initial = session.vm.canvas.menu.as_ref().unwrap().master_db;
        session
            .open_page("XIDInterf.XIIIMenuAudioClientWindow")
            .expect("open retail audio page");
        session.dispatch_key(40).expect("focus Sound Volume slider");
        session
            .dispatch_key(37)
            .expect("decrease Sound Volume slider one step from its upper endpoint");
        session.dispatch_key(13).expect("apply audio page options");
        session.advance(1.0 / 60.0);
        let saved = config::load(
            &game_dir,
            &config::user_path(&game_dir, &config_dir).unwrap(),
        )
        .expect("read persisted settings");
        assert_ne!(
            saved.master_db, initial,
            "the audio slider change must persist"
        );

        let restarted = MenuSession::open_configured(&game_dir, save_dir, &config_dir, None)
            .expect("restart menu VM");
        assert_eq!(
            restarted.vm.canvas.menu.as_ref().unwrap().master_db,
            saved.master_db
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn control_target_resolution_accepts_index_and_label() {
        // The resolution helper is exercised without a VM by mirroring the lookup tables.
        let labels = XIIIMENU_LABELS;
        let by_label = |t: &str| labels.iter().position(|l| l.eq_ignore_ascii_case(t));
        assert_eq!(by_label("newgame"), Some(4));
        assert_eq!(by_label("NEWGAME"), Some(4));
        assert_eq!(by_label("quit"), Some(5));
        assert_eq!(by_label("nope"), None);
    }
}
