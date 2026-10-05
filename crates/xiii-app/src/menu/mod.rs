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
//! This module replaces only that render loop: it instantiates the pages/controls, calls the
//! same script callbacks the native loop calls (delegates are not interpreted by the VM, so the
//! callbacks are invoked directly), and draws the recorded Canvas commands. The `ViewportOwner`
//! (the engine's player/viewport link) is left `None`; the menu functions used here tolerate
//! that (they only use it for `GetLevel`/`GetPlayerOwner`). The `cine00` intro video is not
//! decoded, so `VideoPlayer.*` is a labelled stub and the new-game `ClientTravel("Plage00")`
//! request is recorded for the host travel step.

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
use xiii_world::PackageCache;
use xiii_world::runtime;

use crate::cli::Options;
use crate::play::hud::{self, FontDb};

/// Process-wide travel request written when the menu selects New game and read by `main`, which
/// then performs the host travel step (`--play` on the requested map).
static TRAVEL_REQUEST: Mutex<Option<TravelRequest>> = Mutex::new(None);

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

/// `XIIIMenu` label struct properties, in `Controls` order. The decoded `XIIIMenu.AfterPaint`
/// draws a button's label only for the highlighted control; the native GUI renderer draws every
/// component's caption, so the host draws all six through the same decoded
/// `XIIIWindow.DrawLabel` (the captions are `XIIIMenu`'s localised `*Text` defaults).
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
    root: ObjectId,
    page: ObjectId,
    canvas: ObjectId,
    controls: Vec<ObjectId>,
    control_labels: Vec<String>,
    fonts: Option<Arc<FontDb>>,
    packages: PackageCache,
    commands: Vec<DrawCommand>,
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
    fn open(game_dir: &Path, script: Option<MenuScript>) -> Result<MenuSession, String> {
        // `[URL] LocalMap=mapmenu.unr` in Default.ini is the local front-end map. `[URL] Map`
        // names `Index.unr`, which is absent from this corpus; MapMenu is the installed one.
        let (set, map_idx) = runtime::load_with_map(game_dir, "MapMenu")?;
        let set: &'static ScriptSet = Box::leak(Box::new(set));
        let mut vm = Vm::new(set, VmLimits::default());
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
            runtime::resolve_class_path(set, "Engine.PlayerController"),
        ) {
            let viewport = vm
                .spawn(player_cls, "ViewportOwner(menu)")
                .map_err(|e| format!("creating ViewportOwner: {e}"))?;
            vm.set_active(viewport, true);
            let pc = vm
                .spawn(pc_cls, "MenuPlayerController")
                .map_err(|e| format!("creating PlayerController: {e}"))?;
            vm.set_active(pc, true);
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
        vm.set_property(
            page,
            "myRoot",
            0,
            Value::Object(Some(ObjRef::Instance(root))),
        );

        // Run the game's `Created`: loads the button/onomatopoeia textures with
        // `DynamicLoadObject` and builds the controls with `CreateControl` (the `new` opcode).
        // The decoded function ends with a delegate assignment (`delegateprop`), which the VM
        // does not interpret, so a trailing error is expected and the controls are already
        // built. `SendEvent` on a non-actor does not suspend the object.
        let mut created_error = None;
        if let Err(e) = vm.send_event(page, "Created", Vec::new()) {
            created_error = Some(e.to_string());
        }

        // Wire each control to the page (its `MenuOwner`, which the native GUI would set) and
        // the root, then run the control `Created`.
        let mut controls = Vec::new();
        let mut control_labels = Vec::new();
        if let Some(Value::Array(items)) = vm.get_property(page, "Controls").cloned() {
            for (i, item) in items.iter().enumerate() {
                let Value::Object(Some(ObjRef::Instance(id))) = item else {
                    continue;
                };
                let id = *id;
                vm.set_property(
                    id,
                    "MenuOwner",
                    0,
                    Value::Object(Some(ObjRef::Instance(page))),
                );
                vm.set_property(id, "myRoot", 0, Value::Object(Some(ObjRef::Instance(root))));
                if let Err(e) = vm.send_event(id, "Created", Vec::new()) {
                    log.push(format!("[menu] control {i} Created: {e}"));
                }
                controls.push(id);
                control_labels.push(
                    XIIIMENU_LABELS
                        .get(i)
                        .map(|s| (*s).to_owned())
                        .unwrap_or_else(|| format!("control{i}")),
                );
            }
        }
        // Initial focus on Continue (index 0), as the decoded `OpenMenu`/`ResetFocus` flow does
        // for a fresh main menu.
        if let Some(first) = controls.first() {
            vm.set_property(
                page,
                "FocusedControl",
                0,
                Value::Object(Some(ObjRef::Instance(*first))),
            );
        }
        log.push(format!(
            "[menu] XIIIMenu: {} control(s) {:?}",
            controls.len(),
            control_labels
        ));
        if let Some(e) = &created_error {
            log.push(format!(
                "[menu] XIIIMenu.Created returned an error after building the controls \
                 (expected: the trailing delegate assignment): {e}"
            ));
        }

        let packages = PackageCache::open(game_dir)?;
        let trace_cursor = vm.trace.len();
        Ok(MenuSession {
            vm,
            root,
            page,
            canvas,
            controls,
            control_labels,
            fonts: None,
            packages,
            commands: Vec::new(),
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
            if let Err(e) = self.apply_action(&action) {
                self.errors.push(format!("{action:?}: {e}"));
            }
        }
        // The map actors are not begun; only the root/page state machines and timers need time.
        let _ = self.vm.tick(dt);
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
                self.vm
                    .send_event(
                        self.page,
                        "InternalOnKeyEvent",
                        vec![Value::Byte(key), Value::Byte(1), Value::Float(0.0)],
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
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

    /// Instantiates a decoded page class as the active page and wires its controls (the host
    /// bridge for the native `GUIController.OpenMenu` page stack). Used by `--menu-script open`.
    fn open_page(&mut self, class_name: &str) -> Result<(), String> {
        let class = runtime::resolve_class_path(self.vm.set(), class_name)
            .ok_or_else(|| format!("page class {class_name} is not loaded"))?;
        let page = self
            .vm
            .spawn(class, "MenuPage(menu)")
            .map_err(|e| e.to_string())?;
        self.vm.set_active(page, true);
        self.vm.set_property(
            page,
            "myRoot",
            0,
            Value::Object(Some(ObjRef::Instance(self.root))),
        );
        if let Err(e) = self.vm.send_event(page, "Created", Vec::new()) {
            self.errors.push(format!("{class_name}.Created: {e}"));
        }
        let mut controls = Vec::new();
        let mut labels = Vec::new();
        if let Some(Value::Array(items)) = self.vm.get_property(page, "Controls").cloned() {
            for (i, item) in items.iter().enumerate() {
                let Value::Object(Some(ObjRef::Instance(id))) = item else {
                    continue;
                };
                let id = *id;
                self.vm.set_property(
                    id,
                    "MenuOwner",
                    0,
                    Value::Object(Some(ObjRef::Instance(page))),
                );
                self.vm.set_property(
                    id,
                    "myRoot",
                    0,
                    Value::Object(Some(ObjRef::Instance(self.root))),
                );
                if let Err(e) = self.vm.send_event(id, "Created", Vec::new()) {
                    self.errors
                        .push(format!("{class_name} control {i} Created: {e}"));
                }
                controls.push(id);
                labels.push(format!("control{i}"));
            }
        }
        println!(
            "[menu] opened page {class_name}: {} control(s)",
            controls.len()
        );
        self.page = page;
        self.controls = controls;
        self.control_labels = labels;
        Ok(())
    }

    /// Runs the decoded paint callbacks for one frame and collects the Canvas commands.
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
        // Page ratios (`XIIIWindow.BeforePaint`) and the controls' own pre-draw (origin,
        // `fRatioX/Y`, stretch) then draw (Paint + AfterPaint), matching the native render loop.
        self.call(
            self.page,
            "BeforePaint",
            vec![canvas.clone(), zero(), zero()],
        );
        let controls = self.controls.clone();
        for (i, c) in controls.iter().copied().enumerate() {
            self.call(c, "InternalOnPreDraw", vec![canvas.clone()]);
            self.call(c, "InternalOnDraw", vec![canvas.clone()]);
            // The native GUI draws each component's caption; draw the game's label structs
            // through `XIIIWindow.DrawLabel` (host bridge, documented in the module header).
            if let Some(prop) = XIIIMENU_LABEL_PROPS.get(i)
                && let Some(label) = self.vm.get_property(self.page, prop).cloned()
            {
                self.call(self.page, "DrawLabel", vec![canvas.clone(), label]);
            }
        }
        self.call(self.page, "AfterPaint", vec![canvas, zero(), zero()]);
        self.commands = self.vm.drain_canvas();
        self.fps += 1;
    }

    fn call(&mut self, id: ObjectId, func: &str, args: Vec<Value>) {
        if let Err(e) = self.vm.send_event(id, func, args) {
            let msg = format!("{func}: {e}");
            if !self.errors.iter().any(|x| x == &msg) {
                self.errors.push(msg);
            }
        }
    }
}

fn zero() -> Value {
    Value::Float(0.0)
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
        let session = match &self.options.menu_script {
            Some(path) => match MenuScript::load(path) {
                Ok(s) => MenuSession::open(&game_dir, Some(s)),
                Err(e) => {
                    eprintln!("[menu] input script {}: {e}", path.display());
                    Err(e)
                }
            },
            None => MenuSession::open(&game_dir, None),
        };
        println!(
            "[menu] menu session open (MapMenu + XIDInterf.XIIIMenu): {:.2}s",
            t0.elapsed().as_secs_f32()
        );
        app.insert_non_send(session)
            .insert_resource(MenuConfig {
                options: self.options.clone(),
            })
            .insert_resource(ClearColor(Color::srgb(0.02, 0.02, 0.03)))
            .init_resource::<MenuShotFlag>()
            .add_systems(Startup, setup)
            .add_systems(Update, (frame, draw, unattended).chain());
    }
}

fn setup(
    mut commands: Commands,
    cfg: Res<MenuConfig>,
    mut session: NonSendMut<Result<MenuSession, String>>,
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
    // A camera is required for the Bevy UI overlay to render.
    commands.spawn((Camera3d::default(), Transform::default()));
    println!(
        "[menu] ready: {} control(s); input script {} action(s)",
        session.controls.len(),
        session.script.as_ref().map_or(0, |s| s.actions.len())
    );
}

fn frame(
    time: Res<Time>,
    window: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut session: NonSendMut<Result<MenuSession, String>>,
) {
    let Ok(session) = session.as_mut() else {
        return;
    };
    let dt = time.delta_secs();
    session.clip = window
        .single()
        .map(|w| [w.width(), w.height()])
        .unwrap_or([1280.0, 720.0]);
    session.advance(dt);
    session.refresh_commands();
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
    let (package, object) = path.rsplit_once('.')?;
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
    app.add_plugins(MenuPlugin { options });
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
        let mut session =
            MenuSession::open(&game_dir, Some(script)).expect("open the front-end menu");
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
