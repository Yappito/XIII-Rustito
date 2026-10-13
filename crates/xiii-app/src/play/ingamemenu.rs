//! item72: the retail in-game menu (Esc → `ShowTheMenu` → `XIIIMenuInGame`) inside a running
//! level.
//!
//! **Design (from the decode).** Retail's root window `XIDInterf.XIIIRootWindow` is an
//! Interaction on the running level's own `Player` (`XIIIConsole.ShowTheMenu` reads
//! `myC.Player.LocalInteractions[0]`), and every page callback reads live level state
//! (`Level.Game.MapInfo.Objectif[]`, `myHUD`, `Level.Title`) through the same object graph. The
//! page instance therefore runs **in the same VM as the level**: this module installs the
//! retail object graph (`Engine.Player` viewport, `XIDInterf.XIIIConsole`,
//! `XIDInterf.XIIIRootWindow`, an `Engine.InteractionMaster` linking them) and then drives the
//! game's own decoded code — `ShowTheMenu` (`xidinterf.u` @274801..275508), the root's
//! `UWindows` state (`BeginState` pauses through `PlayerController.SetPause` →
//! `GameInfo.SetPause` → `Level.Pauser`, `EndState` unpauses), and the decoded `gui.u` page
//! stack (`OpenMenu`/`InternalMenuInit`/`CloseAll`/`CloseMenu`). The level clock already
//! freezes when `Level.Pauser` is set (item68b).
//!
//! **Host-side (documented).** The native `GUI.dll` render loop is replaced: each rendered
//! frame the page/controls `__OnPreDraw__`/`__OnDraw__` delegates and the page's `AfterPaint`
//! run into the level VM's existing HUD `Canvas` (the same draw-command pipeline the HUD uses).
//! The console save-device pieces the paused VM cannot schedule — `XIIIMenuSave`'s
//! `GetSlotDescription`/`SaveInSlot` state code only advances in `process_state`, which the
//! pause skips — are completed host-side through the item20 save-slot provider, mirroring the
//! front-end host's load-window completion. Quit to main menu is the game's own
//! `ClientTravel("MapMenu")`, surfaced to `main` as a host relaunch of the `--menu` mode (one
//! event loop per process, the same arrangement as the menu → play travel step).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use xiii_script::canvas::{DrawCommand, TravelRequest};
use xiii_script::{HostRequest, ObjRef, ObjectId, TraceKind, Value, Vm};

use super::session::Session;

/// Front-end map the retail `QuitGame` travels to (`ClientTravel("MapMenu", ...)`; the same
/// map `[URL] LocalMap=mapmenu.unr` names in `Default.ini`).
pub const FRONT_END_MAP: &str = "MapMenu";

/// Shared cell between the host and the VM-installed save-slot provider: the page's write
/// request lands here and the host fulfils it with a real `Session::menu_snapshot` write.
#[derive(Default)]
struct PlaySaveShared {
    write_request: Option<u32>,
    write_result: Option<bool>,
    /// Slot handed to the game's `LoadAtCheckpoint` (the Retry path): the newest save.
    read_slot: Option<u32>,
    last_written: Option<u32>,
}

/// The item20 provider over the host save store. Listing is synchronous from the store; the
/// write is parked in the shared cell for the host frame (the snapshot needs `&Session`, which
/// the provider cannot reach from inside the VM).
struct PlaySaveSlots {
    slots: Vec<crate::save::SlotInfo>,
    shared: Rc<RefCell<PlaySaveShared>>,
    /// Slot of the request currently in flight (`Request*` → `Poll*` pairing).
    last_query: Option<u32>,
}

impl PlaySaveSlots {
    fn open(dir: PathBuf, shared: Rc<RefCell<PlaySaveShared>>) -> Result<Self, String> {
        let store = crate::save::SaveStore::open(dir);
        let slots = store.list()?;
        Ok(Self {
            slots,
            shared,
            last_query: None,
        })
    }

    fn info(&self, slot: u32) -> Option<&crate::save::SlotInfo> {
        self.slots.iter().find(|i| i.slot == slot)
    }

    /// The UTC calendar date the save-store mtime encodes (`MenuSaveSlots` format).
    fn info_date(i: &crate::save::SlotInfo) -> [i32; 5] {
        let secs = i.modified_unix as i64;
        let days = secs.div_euclid(86400);
        let sod = secs.rem_euclid(86400);
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let mut y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100 + yoe / 400);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        y += i64::from(m <= 2);
        [
            y as i32,
            m as i32,
            d as i32,
            (sod / 3600) as i32,
            ((sod % 3600) / 60) as i32,
        ]
    }
}

impl xiii_script::SaveSlotProvider for PlaySaveSlots {
    fn slot(&self, s: u32) -> Option<xiii_script::SaveSlotInfo> {
        let i = self.info(s)?;
        Some(xiii_script::SaveSlotInfo {
            description: i.description.clone(),
            date: Self::info_date(i),
        })
    }
    fn request_empty(&mut self, s: u32) -> bool {
        // The answer is synchronous: the store listing already holds it.
        self.last_query = Some(s);
        true
    }
    fn poll_empty(&mut self) -> Option<bool> {
        let s = self.last_query.take()?;
        Some(self.info(s).is_none())
    }
    fn request_description(&mut self, s: u32) -> bool {
        self.last_query = Some(s);
        self.info(s).is_some()
    }
    fn poll_description(&mut self) -> Option<String> {
        let s = self.last_query.take()?;
        Some(self.info(s)?.description.clone())
    }
    fn request_date(&mut self, s: u32) -> bool {
        self.last_query = Some(s);
        self.info(s).is_some()
    }
    fn poll_date(&mut self) -> Option<[i32; 5]> {
        let s = self.last_query.take()?;
        Some(Self::info_date(self.info(s)?))
    }
    fn request_read(&mut self, s: u32) -> bool {
        if self.info(s).is_none() {
            return false;
        }
        self.shared.borrow_mut().read_slot = Some(s);
        true
    }
    fn poll_read(&mut self) -> bool {
        self.shared.borrow().read_slot.is_some()
    }
    fn take_read_slot(&mut self) -> Option<u32> {
        let mut shared = self.shared.borrow_mut();
        if shared.read_slot.is_none() {
            // Retail `LoadAtCheckpoint` reloads the last checkpoint of the current game; the
            // newest host save is that checkpoint.
            shared.read_slot = shared.last_written.or_else(|| {
                self.slots
                    .iter()
                    .max_by_key(|s| (s.modified_unix, s.slot))
                    .map(|s| s.slot)
            });
        }
        shared.read_slot.take()
    }
    fn request_write(&mut self, slot: u32) -> bool {
        if slot >= 10 {
            return false;
        }
        let mut shared = self.shared.borrow_mut();
        if shared.write_request.is_some() {
            return false;
        }
        shared.write_request = Some(slot);
        shared.write_result = None;
        true
    }
    fn poll_write(&mut self) -> Option<bool> {
        self.shared.borrow_mut().write_result
    }
    fn has_any_checkpoint(&self) -> bool {
        !self.slots.is_empty()
    }
}

/// Host state of the in-game menu. Lives in a non-send resource; the VM references are only
/// meaningful while the session is open (reset on a travel rebuild).
#[derive(Default)]
pub struct InGameMenu {
    config_path: Option<PathBuf>,
    save_dir: Option<PathBuf>,
    /// The campaign map of the open session (the save description and Quit interception).
    map: Option<String>,
    installed: bool,
    root: Option<ObjectId>,
    console: Option<ObjectId>,
    /// Current active page (re-read from the root each frame; cached for input routing).
    page: Option<ObjectId>,
    /// First errors from the install/open/dispatch path (visible in the log and the exit
    /// report; never silently dropped).
    pub errors: Vec<String>,
    shared: Rc<RefCell<PlaySaveShared>>,
    /// `ClientTravel` observed while the menu was open (`QuitGame` → `MapMenu`).
    pub travel: Option<TravelRequest>,
    /// `LoadAtCheckpoint` observed while the menu was open (Retry): the slot to resume.
    pub load_slot: Option<u32>,
    /// Set when a menu save failed (e.g. a map with no resumable save-trigger identity).
    pub save_error: Option<String>,
}

impl InGameMenu {
    pub fn install_settings(
        &mut self,
        session: &mut Session,
        game: &std::path::Path,
        config: &std::path::Path,
    ) -> Result<(), String> {
        let path = crate::menu::config::user_path(game, config)?;
        session.vm_mut().canvas.menu = Some(crate::menu::config::load(game, &path)?);
        self.config_path = Some(path);
        Ok(())
    }
    /// Configures the host directories/map for the session about to run.
    pub fn configure(&mut self, save_dir: Option<PathBuf>, map: Option<String>) {
        if self.installed {
            // A travel rebuild reuses the resource; drop the stale VM references first.
            self.reset(save_dir, map);
        } else {
            self.save_dir = save_dir;
            self.map = map;
        }
    }

    /// Whether the retail page is currently open (root in `UWindows` with an active page).
    pub fn is_open(&self) -> bool {
        self.root.is_some() && self.page.is_some()
    }

    /// Recomputes `page` from the root's `ActivePage` and returns whether the menu is open.
    fn poll(&mut self, vm: &mut Vm<'static>) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        self.page = match vm.get_property(root, "ActivePage").cloned() {
            Some(Value::Object(Some(ObjRef::Instance(id))))
                if vm.objects.get(id as usize).is_some_and(|o| !o.deleted) =>
            {
                Some(id)
            }
            _ => None,
        };
        let open = vm.is_in_state(root, "UWindows") && self.page.is_some();
        if !open {
            self.page = None;
        }
        open
    }

    fn record_error(&mut self, error: String) {
        if !self.errors.contains(&error) {
            eprintln!("[menu] {error}");
            self.errors.push(error);
        }
    }

    /// Installs the retail GUI object graph into the play session's VM (once per session) and
    /// wires the save-slot provider. Idempotent; failures are recorded, never silently skipped.
    pub fn ensure_installed(&mut self, session: &mut Session) -> Result<(), String> {
        if self.installed {
            return Ok(());
        }
        let controller = session.controller;
        let vm = session.vm_mut();
        vm.canvas
            .menu
            .get_or_insert_with(xiii_script::canvas::MenuSettings::default);
        let set = vm.set();
        let resolve = |path: &str| {
            xiii_world::runtime::resolve_class_path(set, path)
                .ok_or_else(|| format!("{path} is not loaded"))
        };
        let viewport_class = resolve("Engine.Player")?;
        let root_class = resolve("XIDInterf.XIIIRootWindow")?;
        let console_class = resolve("XIDInterf.XIIIConsole")?;
        let master_class = resolve("Engine.InteractionMaster")?;

        // The save-device provider (`XIIIMenuSave` slot listing + write requests).
        if let Some(dir) = self.save_dir.clone() {
            match PlaySaveSlots::open(dir, self.shared.clone()) {
                Ok(provider) => vm.set_save_slots(Box::new(provider)),
                Err(e) => self.record_error(format!("save-slot provider: {e}")),
            }
        }

        // The `Engine.Player` viewport: `PC.Player` in retail, `ViewportOwner.Actor` for the
        // root's `GetPlayerOwner`, `LocalInteractions[0]` for `ShowTheMenu`'s root lookup.
        let viewport = vm
            .spawn(viewport_class, "ViewportOwner(ingame)")
            .map_err(|e| e.to_string())?;
        vm.set_active(viewport, true);
        let root = vm
            .spawn(root_class, "XIIIRootWindow(ingame)")
            .map_err(|e| e.to_string())?;
        vm.set_active(root, true);
        let console = vm
            .spawn(console_class, "XIIIConsole(ingame)")
            .map_err(|e| e.to_string())?;
        vm.set_active(console, true);
        let master = vm
            .spawn(master_class, "InteractionMaster(ingame)")
            .map_err(|e| e.to_string())?;
        vm.set_active(master, true);

        // Root window fields the decoded `Created`/`BeforePaint`/draw path reads (the values
        // `UWindows.BeginState` sets, set up front so the first paint has them).
        for (prop, value) in [
            ("GUIScale", Value::Float(1.0)),
            ("bMapMenu", Value::Bool(false)),
            ("bIamInGame", Value::Bool(false)),
            ("bIamInMulti", Value::Bool(false)),
            ("bProfileMenu", Value::Bool(false)),
            ("bMusicPlay", Value::Bool(false)),
            ("CurrentPF", Value::Int(0)),
            ("fTextureScaleFactorForConsole", Value::Float(1.0)),
            ("GameResolution", Value::Str(String::new())),
            // The in-game save device is available: the host owns the save store. The page's
            // own `BeforePaint` still greys Save while `IsThereACheckpoint()` is false.
            ("bSavingPossible", Value::Bool(true)),
        ] {
            if !vm.set_property(root, prop, 0, value) {
                self.record_error(format!("XIIIRootWindow has no writable {prop}"));
            }
        }
        // The front-end background textures the pages draw through `myRoot` (retail
        // `XIIIRootWindow.InitializeController` loads these two by name).
        for (prop, name) in [
            ("FondMenu", "XIIIMenuStart.menublanc"),
            ("tFondNoir", "XIIIMenuStart.menunoir"),
        ] {
            match vm.external_asset(name) {
                Some((value, _)) => {
                    vm.set_property(root, prop, 0, value);
                }
                None => self.record_error(format!("root {prop} texture {name} did not resolve")),
            }
        }

        let level = vm.find_level_info();
        if let Some(pc) = controller {
            vm.set_property(
                pc,
                "Player",
                0,
                Value::Object(Some(ObjRef::Instance(viewport))),
            );
        } else {
            self.record_error("play session has no player controller; menu cannot open".into());
        }
        if let Some(level) = level {
            vm.set_property(
                viewport,
                "Level",
                0,
                Value::Object(Some(ObjRef::Instance(level))),
            );
        }
        vm.set_property(
            viewport,
            "Actor",
            0,
            controller
                .map(|pc| Value::Object(Some(ObjRef::Instance(pc))))
                .unwrap_or(Value::Object(None)),
        );
        vm.set_property(
            viewport,
            "InteractionMaster",
            0,
            Value::Object(Some(ObjRef::Instance(master))),
        );
        vm.set_property(
            master,
            "GlobalInteractions",
            0,
            Value::Array(vec![Value::Object(Some(ObjRef::Instance(console)))]),
        );
        for id in [console, root] {
            vm.set_property(
                id,
                "ViewportOwner",
                0,
                Value::Object(Some(ObjRef::Instance(viewport))),
            );
            vm.set_property(
                id,
                "Master",
                0,
                Value::Object(Some(ObjRef::Instance(master))),
            );
        }
        vm.set_property(
            viewport,
            "LocalInteractions",
            0,
            Value::Array(vec![Value::Object(Some(ObjRef::Instance(root)))]),
        );

        // The decoded menu-data load: fills `myMenuData` page stacks from the `XIIIMenuData`
        // class defaults via `DynamicLoadObject` (`gui.u` @46520/@46566).
        if let Err(e) = vm.send_event(root, "LoadIngameMenu", vec![]) {
            self.record_error(format!("LoadIngameMenu: {e}"));
        }
        // item89b baseline: requests queued before the menu existed are gameplay travel, not
        // menu requests (the old trace cursor skipped them the same way).
        let _ = vm.drain_host_requests();
        self.root = Some(root);
        self.console = Some(console);
        self.installed = true;
        println!(
            "[menu] in-game GUI installed: root={}, console={}, ViewportOwner linked to the player controller",
            vm.objects[root as usize].name, vm.objects[console as usize].name
        );
        Ok(())
    }

    /// Opens the menu through the game's own `XIIIConsole.ShowTheMenu(controller)` (the exact
    /// decoded `Escape=Showmenu` path). Completes the save page's host-side pieces when the
    /// opened page needs them.
    pub fn open(&mut self, session: &mut Session) {
        if let Err(e) = self.ensure_installed(session) {
            self.record_error(format!("in-game menu install: {e}"));
            return;
        }
        let (Some(console), Some(pc)) = (self.console, session.controller) else {
            self.record_error("menu open without console/controller".into());
            return;
        };
        if self.poll(session.vm_mut()) {
            return; // already open; the page handles its own Esc close
        }
        let vm = session.vm_mut();
        if let Err(e) = vm.send_event(
            console,
            "ShowTheMenu",
            vec![Value::Object(Some(ObjRef::Instance(pc)))],
        ) {
            self.record_error(format!("ShowTheMenu: {e}"));
        }
        // In retail GUI.dll calls the root's menu-opening delegate during its UWindows
        // BeginState. That native GUI lifecycle is absent in the VM host, so drive the same
        // decoded GUIController.OpenMenu entry point when the state transition did not create
        // its page itself.
        if self.root.is_some_and(|root| {
            !matches!(
                vm.get_property(root, "ActivePage"),
                Some(Value::Object(Some(ObjRef::Instance(_))))
            )
        }) && let Some(root) = self.root
            && let Err(e) = vm.send_event(
                root,
                "OpenMenu",
                vec![Value::Str("XIDInterf.XIIIMenuInGame".to_owned())],
            )
        {
            self.record_error(format!("GUIController.OpenMenu: {e}"));
        }
        if self.root.is_some_and(|root| {
            !matches!(
                vm.get_property(root, "ActivePage"),
                Some(Value::Object(Some(ObjRef::Instance(_))))
            )
        }) {
            // The page stack native initialization is also part of GUI.dll's host lifecycle.
            // Reproduce its decoded `OpenMenuWithClass` operations (new page, InitComponent,
            // stack/ActivePage assignment, ShowWindow) while retaining the same page class and
            // script delegates.
            let root = self.root.expect("installed root");
            let page_class =
                xiii_world::runtime::resolve_class_path(vm.set(), "XIDInterf.XIIIMenuInGame")
                    .ok_or_else(|| "XIDInterf.XIIIMenuInGame is not loaded".to_owned());
            match page_class.and_then(|class| {
                vm.spawn(class, "XIIIMenuInGame(ingame)")
                    .map_err(|e| e.to_string())
            }) {
                Ok(page) => {
                    vm.set_active(page, true);
                    vm.canvas
                        .menu
                        .get_or_insert_with(xiii_script::canvas::MenuSettings::default);
                    if let Err(e) = xiii_script::canvas::apply_menu_config(vm, page) {
                        self.record_error(format!("menu page configuration: {e}"));
                    }
                    if let Err(e) = vm.send_event(
                        page,
                        "InitComponent",
                        vec![
                            Value::Object(Some(ObjRef::Instance(root))),
                            Value::Object(None),
                        ],
                    ) {
                        self.record_error(format!("XIIIMenuInGame.InitComponent: {e}"));
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
                    if let Err(e) = vm.send_event(page, "ShowWindow", vec![]) {
                        self.record_error(format!("XIIIMenuInGame.ShowWindow: {e}"));
                    }
                }
                Err(e) => self.record_error(e),
            }
        }
        let open = self.poll(vm);
        let page_desc = self
            .page
            .map(|p| vm.set().path(vm.objects[p as usize].class))
            .unwrap_or_else(|| "<none>".to_owned());
        let (paused, root_state) = {
            let level_pauser = vm
                .find_level_info()
                .and_then(|level| vm.get_property(level, "Pauser").cloned())
                .map(|p| !matches!(p, Value::Object(None)))
                .unwrap_or(false);
            (level_pauser, self.root.and_then(|r| vm.state_name(r)))
        };
        println!(
            "[menu] ShowTheMenu: open={open} page={page_desc} Level.Pauser set={paused} root state={root_state:?} ActivePage={:?} MenuStack={:?}",
            self.root
                .and_then(|r| vm.get_property(r, "ActivePage").cloned()),
            self.root
                .and_then(|r| vm.get_property(r, "MenuStack").cloned())
        );
        if !open {
            self.record_error("ShowTheMenu did not open the in-game page".into());
            return;
        }
        self.complete_page(session);
        self.take_host_requests(session);
    }

    /// Closes the menu through the page's own Esc handler (the retail resume path:
    /// `ResetInputs` + `CloseAll` + `GotoState('None')` → `EndState` → `SetPause(false)`).
    pub fn request_close(&mut self, session: &mut Session) {
        if self.poll(session.vm_mut()) {
            let _ = self.key(session, 27);
            // Some VM package builds do not bind the page's GUI input delegate until the native
            // controller has run its first render initialization. Preserve the retail Esc close
            // sequence explicitly if the page handler left it open.
            if self.poll(session.vm_mut())
                && let Some(root) = self.root
            {
                let pc = session.controller;
                let vm = session.vm_mut();
                if let Some(pc) = pc {
                    let _ = vm.send_event(pc, "ResetInputs", vec![]);
                }
                let _ = vm.send_event(root, "CloseAll", vec![Value::Bool(true)]);
                let _ = vm.goto_state(root, "None", None);
                vm.set_property(root, "bIamInGame", 0, Value::Bool(false));
                vm.set_property(root, "bIamInMulti", 0, Value::Bool(false));
                let _ = self.poll(vm);
            }
        }
    }

    /// Dispatches one key press (state 1) to the focused control's `OnKeyEvent` delegate, then
    /// the page's `InternalOnKeyEvent` — the decoded dispatch order the front-end host uses.
    pub fn key(&mut self, session: &mut Session, key: u8) -> Result<bool, String> {
        if !self.poll(session.vm_mut()) {
            return Ok(false);
        }
        let args = vec![Value::Byte(key), Value::Byte(1), Value::Float(0.0)];
        let vm = session.vm_mut();
        let page = self.page.expect("poll opened");
        // Each retail page owns Enter/click semantics (Save handles slots; Skills closes;
        // InGame calls InternalOnClick). Generic button delegates can consume Enter without
        // executing that page behavior, so dispatch the page's decoded key handler first.
        if matches!(key, 13 | 1)
            && let Some(Value::Object(Some(ObjRef::Instance(_)))) =
                vm.get_property(page, "FocusedControl").cloned()
        {
            let start = vm.trace.len();
            vm.send_event(page, "InternalOnKeyEvent", args.clone())
                .map_err(|e| e.to_string())?;
            for event in &vm.trace[start..] {
                match &event.kind {
                    TraceKind::Event { function, .. }
                        if function.contains("Menu") || function.contains("MsgBox") =>
                    {
                        println!("[menu dispatch] {:?}", event.kind)
                    }
                    TraceKind::Native { path, .. } if !path.starts_with("Object.") => {
                        println!("[menu native] {:?}", event.kind)
                    }
                    _ => {}
                }
            }
            self.after_dispatch(session);
            return Ok(true);
        }
        if let Some(Value::Object(Some(ObjRef::Instance(id)))) =
            vm.get_property(page, "FocusedControl").cloned()
        {
            let result = vm
                .call_delegate(id, "__OnKeyEvent__Delegate", "OnKeyEvent", args.clone())
                .map_err(|e| e.to_string())?;
            if result == Value::Bool(true) {
                self.after_dispatch(session);
                return Ok(true);
            }
        }
        vm.send_event(page, "InternalOnKeyEvent", args)
            .map_err(|e| e.to_string())?;
        self.after_dispatch(session);
        Ok(true)
    }

    /// Mouse path while open: focus the control under the cursor (`Bounds` hit test,
    /// back-to-front, the native controller's rule) and dispatch the click key `1` — retail
    /// routes mouse clicks through `InternalOnKeyEvent(Key=1, State=1)`.
    pub fn click(&mut self, session: &mut Session, cursor: Option<(f32, f32)>) -> bool {
        if !self.poll(session.vm_mut()) {
            return false;
        }
        if let Some((cx, cy)) = cursor {
            let vm = session.vm_mut();
            let page = self.page.expect("poll opened");
            let controls = vm.get_property(page, "Controls").cloned();
            let mut target = None;
            if let Some(Value::Array(items)) = controls {
                for item in items.iter().rev() {
                    let Value::Object(Some(ObjRef::Instance(c))) = item else {
                        continue;
                    };
                    let bound = |i: usize| match vm.get_property_elem(*c, "Bounds", i) {
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
                        target = Some(*c);
                        break;
                    }
                }
            }
            if let Some(t) = target {
                let focused = match vm.get_property(page, "FocusedControl") {
                    Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
                    _ => None,
                };
                if focused != Some(t) {
                    let _ = vm.send_event(t, "SetFocus", vec![Value::Object(None)]);
                }
            }
        }
        matches!(self.key(session, 1), Ok(true))
    }

    /// Per-rendered-frame upkeep run by the play app's Update chain: page-state completion
    /// (the save page's latent polling, which the paused VM cannot schedule), request
    /// fulfilment and host-request draining. The page draw runs in [`Self::page_commands`]
    /// from the HUD refresh.
    pub fn frame(&mut self, session: &mut Session, delta: f32) {
        if self.root.is_none() {
            return;
        }
        if self.poll(session.vm_mut()) {
            self.complete_page(session);
            self.fulfil_save(session);
            self.drive_page_tick(session);
            if let Some(page) = self.page
                && !session
                    .vm()
                    .set()
                    .path(session.vm().objects[page as usize].class)
                    .eq_ignore_ascii_case("xidinterf.XIIIMenuSave")
                && let Err(error) = session.vm_mut().tick_gui_state(page, delta)
            {
                self.record_error(format!("GUI page state: {error}"));
            }
            self.take_host_requests(session);
        }
        if let Some(path) = &self.config_path
            && let Some(settings) = session.vm_mut().canvas.menu.as_mut()
            && settings.save_requested
            && let Err(error) = crate::menu::config::save(path, settings)
        {
            self.record_error(format!("menu settings save: {error}"));
        }
    }

    /// The page draw commands for this frame (empty while closed). Called from the HUD refresh
    /// after the HUD's own `PostRender`, so the page draws over the frozen game view.
    pub fn page_commands(
        &mut self,
        session: &mut Session,
        canvas: ObjectId,
        clip: [f32; 2],
    ) -> Vec<DrawCommand> {
        if !self.poll(session.vm_mut()) {
            return Vec::new();
        }
        self.draw_commands(session, canvas, clip)
    }

    /// Runs one non-actor page `Tick` when the page has one (`XIIIMenuSave.Tick` closes the
    /// page once `bSavingPossible` drops; the VM only ticks actors).
    fn drive_page_tick(&mut self, session: &mut Session) {
        let Some(page) = self.page else {
            return;
        };
        let vm = session.vm_mut();
        if !vm
            .set()
            .path(vm.objects[page as usize].class)
            .eq_ignore_ascii_case("xidinterf.xiiimenusave")
        {
            return;
        }
        if let Err(e) = vm.send_event(page, "Tick", vec![Value::Float(0.0)]) {
            self.record_error(format!("XIIIMenuSave.Tick: {e}"));
        }
        if self.shared.borrow().write_result == Some(true)
            && let Some(root) = self.root
            && vm.get_property(root, "ActivePage")
                != Some(&Value::Object(Some(ObjRef::Instance(page))))
        {
            vm.set_property(root, "bSavingPossible", 0, Value::Bool(true));
        }
    }

    /// Completes the host-side pieces of `XIIIMenuSave`: its `GetSlotDescription`/
    /// `SaveInSlot` state code only advances in `process_state`, which the pause skips, so the
    /// host finishes the provider queries, leaves the state, and lets the page's own
    /// `PageSwitch`/`Tick` run. Mirrors the front-end host's load-window completion.
    fn complete_page(&mut self, session: &mut Session) {
        let Some(page) = self.page else {
            return;
        };
        let vm = session.vm_mut();
        if !vm
            .set()
            .path(vm.objects[page as usize].class)
            .eq_ignore_ascii_case("xidinterf.xiiimenusave")
        {
            return;
        }
        let state = vm.state_name(page).unwrap_or_default();
        match state.as_str() {
            "GetSlotDescription" => {
                let max_slots = match vm.get_property(page, "MaxSlots") {
                    Some(Value::Int(n)) if *n > 0 => *n as usize,
                    _ => 0,
                };
                let quick_prefix = match vm.get_property(page, "QuickSavePrefix").cloned() {
                    Some(Value::Str(s)) => s,
                    _ => String::new(),
                };
                let mut empty = vec![Value::Byte(1); max_slots];
                let buttons = match vm.get_property(page, "SaveSlots").cloned() {
                    Some(Value::Array(items)) => items,
                    _ => Vec::new(),
                };
                // Fill the real slot texts/emptiness from the store, then leave the latent
                // state (its remaining code after the loop is nothing).
                for (slot, is_empty) in empty.iter_mut().enumerate() {
                    let info = vm.save_slots_mut().and_then(|p| p.slot(slot as u32));
                    if let Some(info) = info {
                        let [year, month, day, hour, minute] = info.date;
                        let minute = if minute < 10 {
                            format!("0{minute}")
                        } else {
                            minute.to_string()
                        };
                        let mut text =
                            format!("{}  {hour}:{minute} {month}/{day}/{year}", info.description);
                        if slot == 0 {
                            text = format!("{quick_prefix}{text}");
                        }
                        *is_empty = Value::Byte(0);
                        if let Some(Value::Object(Some(ObjRef::Instance(btn)))) =
                            buttons.get(slot).cloned()
                        {
                            vm.set_property(btn, "Text", 0, Value::Str(text));
                        }
                    }
                }
                vm.set_property(page, "bSaveSlotEmpty", 0, Value::Array(empty));
                let _ = vm.goto_state(page, "None", None);
                println!("[menu] XIIIMenuSave slots filled from the host save store");
            }
            "SaveInSlot" => {
                let slot = vm
                    .get_property(page, "SlotNumberToSaveIn")
                    .cloned()
                    .unwrap_or(Value::Int(-1));
                if let Some(root) = self.root {
                    match vm.send_event(root, "RequestWriteSlot", vec![slot]) {
                        Ok(Some(Value::Bool(true))) => {}
                        Ok(result) => {
                            self.record_error(format!("RequestWriteSlot rejected: {result:?}"))
                        }
                        Err(error) => self.record_error(format!("RequestWriteSlot: {error}")),
                    }
                }
                // The write request is in flight (or queued); leaving the state is the host
                // completion — `fulfil_save` settles the request and the page's own `Tick`
                // runs `CloseMenu` once `bSavingPossible` drops.
                let _ = vm.goto_state(page, "None", None);
            }
            _ => {}
        }
    }

    /// Fulfils a pending `RequestWriteSlot` with a real menu snapshot written to the host save
    /// store, then clears `bSavingPossible` so the page's own `Tick` closes the page (the
    /// retail save-device lifecycle).
    fn fulfil_save(&mut self, session: &mut Session) {
        let Some(slot) = self.shared.borrow_mut().write_request.take() else {
            return;
        };
        let map = self.map.clone().unwrap_or_default();
        let rot = session.player_rotation().unwrap_or([0; 3]);
        let location = session.player_location().unwrap_or([0.0; 3]);
        let snapshot = session.menu_snapshot(&map, location, rot).map(|mut data| {
            data.description = format!("Menu save ({map})");
            data
        });
        let result = snapshot.and_then(|data| {
            let dir = self
                .save_dir
                .clone()
                .ok_or_else(|| "no --save-dir configured for the in-game save".to_owned())?;
            crate::save::write(&dir, slot, &data).map(|()| (dir, data))
        });
        match result {
            Ok((dir, data)) => {
                self.shared.borrow_mut().write_result = Some(true);
                self.shared.borrow_mut().last_written = Some(slot);
                match PlaySaveSlots::open(dir.clone(), self.shared.clone()) {
                    Ok(provider) => session.vm_mut().set_save_slots(Box::new(provider)),
                    Err(error) => self.record_error(format!("refresh save slots: {error}")),
                }
                println!(
                    "[menu] in-game save wrote slot {slot} ({}) to {}",
                    data.description,
                    dir.display()
                );
            }
            Err(e) => {
                self.shared.borrow_mut().write_result = Some(false);
                self.save_error = Some(e.clone());
                self.record_error(format!("in-menu save slot {slot}: {e}"));
            }
        }
        if self.shared.borrow().write_result == Some(true)
            && let Some(root) = self.root
        {
            // Retail clears this when the save device settles; the page's own `Tick` then runs
            // `CloseMenu`.
            session
                .vm_mut()
                .set_property(root, "bSavingPossible", 0, Value::Bool(false));
        }
    }

    /// Post-dispatch upkeep (key/click paths): page completion, save fulfilment,
    /// host-request drain.
    fn after_dispatch(&mut self, session: &mut Session) {
        self.poll(session.vm_mut());
        if let Some(page) = self.page {
            let vm = session.vm_mut();
            println!(
                "[menu] active page={} state={:?} focus={:?}",
                vm.set().path(vm.objects[page as usize].class),
                vm.state_name(page),
                vm.get_property(page, "FocusedControl")
            );
        }
        self.complete_page(session);
        self.fulfil_save(session);
        self.take_host_requests(session);
    }

    /// Takes the VM's queued host-request notes and converts them into host relaunch
    /// requests: the `QuitGame` travel (`Main menu` → `MapMenu`) and the checkpoint-load
    /// slot (Retry). item89b: these arrive on the bounded always-on request queue
    /// ([`Vm::drain_host_requests`]), not the diagnostic trace — the windowed session runs
    /// with `trace_enabled = false`.
    fn take_host_requests(&mut self, session: &mut Session) {
        for request in session.vm_mut().drain_host_requests() {
            match request {
                HostRequest::Travel(req) => self.travel = Some(req),
                HostRequest::LoadSlot(slot) => self.load_slot = Some(slot),
            }
        }
    }

    /// Runs the decoded GUI render loop for one frame into the session canvas and returns the
    /// recorded commands (the HUD caller appends them after its own, so the page draws over
    /// the frozen game view). The order is the native GUI.dll loop's: page pre-draw/draw, then
    /// each control, then the page's `AfterPaint`.
    fn draw_commands(
        &mut self,
        session: &mut Session,
        canvas: ObjectId,
        clip: [f32; 2],
    ) -> Vec<DrawCommand> {
        let vm = session.vm_mut();
        if clip[0] <= 0.0 || clip[1] <= 0.0 {
            return Vec::new();
        }
        vm.set_property(canvas, "ClipX", 0, Value::Float(clip[0]));
        vm.set_property(canvas, "ClipY", 0, Value::Float(clip[1]));
        let page = self.page.expect("caller polled");
        let arg = Value::Object(Some(ObjRef::Instance(canvas)));
        let mut commands = Vec::new();
        let mut errors = Vec::new();
        let run = |vm: &mut Vm<'static>, id: ObjectId, prop: &str, declared: &str| {
            vm.call_delegate(id, prop, declared, vec![arg.clone()])
                .map_err(|error| format!("{}.{declared}: {error:?}", vm.objects[id as usize].name))
        };
        if let Err(error) = run(vm, page, "__OnPreDraw__Delegate", "OnPreDraw") {
            errors.push(error);
        }
        if let Err(error) = run(vm, page, "__OnDraw__Delegate", "OnDraw") {
            errors.push(error);
        }
        let controls = vm
            .get_property(page, "Controls")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        if let Value::Array(items) = controls {
            let ids: Vec<ObjectId> = items
                .iter()
                .filter_map(|v| match v {
                    Value::Object(Some(ObjRef::Instance(id))) => Some(*id),
                    _ => None,
                })
                .collect();
            for c in &ids {
                if let Err(error) = run(vm, *c, "__OnPreDraw__Delegate", "OnPreDraw") {
                    errors.push(error);
                }
            }
            for c in &ids {
                if let Err(error) = run(vm, *c, "__OnDraw__Delegate", "OnDraw") {
                    errors.push(error);
                }
            }
        }
        if let Err(error) = vm.send_event(
            page,
            "AfterPaint",
            vec![arg, Value::Float(0.0), Value::Float(0.0)],
        ) {
            errors.push(format!("menu AfterPaint: {error}"));
        }
        commands.extend(vm.drain_canvas());
        for error in errors {
            self.record_error(error);
        }
        commands
    }

    /// Drops VM references after a travel rebuild (the old VM is gone).
    fn reset(&mut self, save_dir: Option<PathBuf>, map: Option<String>) {
        self.save_dir = save_dir;
        self.map = map;
        self.installed = false;
        self.root = None;
        self.console = None;
        self.page = None;
        self.travel = None;
        self.load_slot = None;
        let mut shared = self.shared.borrow_mut();
        shared.write_request = None;
        shared.write_result = None;
        shared.read_slot = None;
        shared.last_written = None;
    }

    /// The front-end map name, for the travel interception.
    pub fn is_front_end_map(map: &str) -> bool {
        map.eq_ignore_ascii_case(FRONT_END_MAP)
    }
}

/// Host handoff requested by the in-game menu (read by `main` after the app exits; one Bevy
/// event loop per process, so the menu/reload step is a child process — the same arrangement
/// as the front-end's menu → play travel step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRelaunch {
    /// `QuitGame`: the game's own `ClientTravel("MapMenu")` — quit to the main menu.
    FrontEnd,
    /// Retry: `LoadAtCheckpoint` — resume the newest save slot.
    Load(u32),
}

static RELAUNCH: Mutex<Option<MenuRelaunch>> = Mutex::new(None);

/// Takes the pending relaunch request (called once by `main` after the play app exits).
pub fn take_relaunch() -> Option<MenuRelaunch> {
    RELAUNCH.lock().ok().and_then(|mut g| g.take())
}

/// Records the host handoff (used by the travel system's front-end interception).
pub fn request_relaunch(req: MenuRelaunch) {
    set_relaunch(req);
}

fn set_relaunch(req: MenuRelaunch) {
    if let Ok(mut g) = RELAUNCH.lock() {
        // The first request wins; a Retry travel and a Quit travel cannot both be pending.
        if g.is_none() {
            *g = Some(req);
        }
    }
}

/// The play app's per-frame menu upkeep system (Update, right after `controls`): runs the
/// page upkeep, applies the scripted `menu`/`menu_close` commands, and surfaces the Quit/Retry
/// host handoffs.
pub(crate) fn frame(
    mut session: NonSendMut<Result<Session, String>>,
    mut menu: NonSendMut<InGameMenu>,
    mut script: ResMut<super::ScriptRes>,
    mut state: ResMut<super::TraceState>,
    mut exit: MessageWriter<AppExit>,
    time: Res<Time>,
) {
    let Ok(sess) = session.as_mut() else {
        return;
    };
    // item72 scripted menu commands: the same decoded open/close paths the Esc binding and the
    // page's own Esc handler run.
    if let Some(drive) = script.drive.as_mut() {
        if drive.take_menu_open() {
            menu.open(sess);
        }
        while drive.take_menu_close() {
            menu.request_close(sess);
        }
        for key in drive.take_menu_keys() {
            if let Err(error) = menu.key(sess, key) {
                menu.record_error(format!("scripted menu key {key}: {error}"));
            }
        }
    }
    menu.frame(sess, time.delta_secs());
    state.menu_open = menu.is_open();
    if let Some(req) = menu.travel.take() {
        if InGameMenu::is_front_end_map(&req.map) {
            println!(
                "[menu] QuitGame travels to {} (url {:?}); the host hands off to the --menu mode",
                req.map, req.url
            );
            set_relaunch(MenuRelaunch::FrontEnd);
            exit.write(AppExit::Success);
        } else {
            eprintln!(
                "[menu] unexpected in-menu travel to {} (only {FRONT_END_MAP} is a host handoff)",
                req.map
            );
        }
    }
    if let Some(slot) = menu.load_slot.take() {
        println!("[menu] LoadAtCheckpoint slot {slot}; the host hands off to --play --load {slot}");
        set_relaunch(MenuRelaunch::Load(slot));
        exit.write(AppExit::Success);
    }
}

/// Spawns the scripted `shot <path>` screenshots (Update; the capture lands at the end of the
/// frame and `save_to_disk` writes it asynchronously).
pub(crate) fn scripted_shots(
    mut commands: Commands,
    mut script: ResMut<super::ScriptRes>,
    windows: Query<(), With<bevy::window::PrimaryWindow>>,
) {
    let Some(drive) = script.drive.as_mut() else {
        return;
    };
    for path in drive.take_shots() {
        if windows.is_empty() {
            eprintln!("[play] shot {path}: no primary window (headless mode cannot screenshot)");
            continue;
        }
        let path = std::path::PathBuf::from(&path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path));
        println!("[play] scripted screenshot requested");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_script::SaveSlotProvider;

    fn paint_test_page(menu: &mut InGameMenu, session: &mut Session) {
        let class =
            xiii_world::runtime::resolve_class_path(session.vm().set(), "Engine.Canvas").unwrap();
        let canvas = session.vm_mut().spawn(class, "MenuTestCanvas").unwrap();
        for (field, value) in [
            ("SizeX", Value::Int(1280)),
            ("SizeY", Value::Int(720)),
            ("ClipX", Value::Float(1280.0)),
            ("ClipY", Value::Float(720.0)),
        ] {
            session.vm_mut().set_property(canvas, field, 0, value);
        }
        for _ in 0..2 {
            menu.page_commands(session, canvas, [1280.0, 720.0]);
        }
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
    }

    #[test]
    fn front_end_map_detection_is_case_insensitive_and_exact() {
        assert!(InGameMenu::is_front_end_map("MapMenu"));
        assert!(InGameMenu::is_front_end_map("mapmenu.unr").eq(&false));
        assert!(InGameMenu::is_front_end_map("mapmenu"));
        assert!(!InGameMenu::is_front_end_map("Plage00"));
        assert!(!InGameMenu::is_front_end_map("MapMenuUnr"));
        assert!(!InGameMenu::is_front_end_map(""));
    }

    #[test]
    fn relaunch_request_keeps_the_first_and_take_clears_it() {
        // A global cell: use distinct requests to prove first-wins.
        request_relaunch(MenuRelaunch::Load(3));
        request_relaunch(MenuRelaunch::FrontEnd);
        assert_eq!(take_relaunch(), Some(MenuRelaunch::Load(3)));
        assert_eq!(take_relaunch(), None, "the second request was dropped");
        // Leave the global clear for other tests.
        assert_eq!(take_relaunch(), None);
    }

    #[test]
    fn configure_resets_a_stale_install() {
        let mut menu = InGameMenu {
            save_dir: Some(std::path::PathBuf::from("a")),
            installed: true,
            root: Some(7),
            console: Some(8),
            page: Some(9),
            travel: Some(TravelRequest {
                url: "MapMenu".into(),
                map: "MapMenu".into(),
                travel_type: 0,
                items: false,
            }),
            ..Default::default()
        };
        menu.configure(Some(std::path::PathBuf::from("b")), Some("Plage00".into()));
        assert!(!menu.installed);
        assert!(menu.root.is_none() && menu.console.is_none() && menu.page.is_none());
        assert!(!menu.is_open());
        assert_eq!(menu.save_dir.as_deref(), Some(std::path::Path::new("b")));
        assert!(menu.travel.is_none());
    }

    #[test]
    fn save_slot_write_bridge_reports_codes() {
        // The shared-cell contract the fulfil path relies on: a request is taken exactly once
        // and the result is readable by the provider.
        let shared = Rc::new(RefCell::new(PlaySaveShared::default()));
        let mut provider =
            PlaySaveSlots::open(std::path::PathBuf::from("nonexistent"), shared.clone()).unwrap();
        assert!(provider.request_write(4));
        assert!(!provider.request_write(5), "one request at a time");
        assert_eq!(shared.borrow_mut().write_request.take(), Some(4));
        assert!(provider.poll_write().is_none(), "unsettled write");
        shared.borrow_mut().write_result = Some(true);
        assert_eq!(provider.poll_write(), Some(true));
        shared.borrow_mut().write_result = None;
        assert!(
            provider.request_write(5),
            "the settled request frees the slot"
        );
        // An unreadable store lists no slots and the checkpoint query is false.
        assert!(!provider.has_any_checkpoint());
        assert!(!provider.request_read(8));
        assert!(!provider.poll_read());
        assert!(!provider.request_write(10));
        shared.borrow_mut().last_written = Some(4);
        assert_eq!(provider.take_read_slot(), Some(4));
    }

    #[test]
    fn opt_in_menu_save_survives_destroyed_checkpoint_trigger() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to run retail menu save acceptance");
            return;
        };
        let dir = std::env::temp_dir().join(format!("xiii-item82-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Session::open(&game_dir, "Plage00").unwrap();
        let mut menu = InGameMenu::default();
        menu.configure(Some(dir.clone()), Some("Plage00".into()));
        menu.open(&mut session);
        menu.key(&mut session, 13).unwrap();
        let page = menu.page.unwrap();
        assert!(
            session
                .vm()
                .set()
                .path(session.vm().objects[page as usize].class)
                .eq_ignore_ascii_case("xidinterf.XIIIMenuSave")
        );
        assert!(matches!(
            session.vm().get_property(page, "bSaveSlotEmpty"),
            Some(Value::Array(_))
        ));
        paint_test_page(&mut menu, &mut session);
        let slot = match session.vm().get_property(page, "SaveSlots") {
            Some(Value::Array(slots)) => slots[1].clone(),
            other => panic!("SaveSlots: {other:?}"),
        };
        session
            .vm_mut()
            .set_property(page, "FocusedControl", 0, slot);
        menu.key(&mut session, 13).unwrap();
        menu.frame(&mut session, 1.0 / 60.0);
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
        let store = crate::save::SaveStore::open(dir.clone());
        let saved = store.read(1).unwrap();
        assert_eq!(saved.description, "Menu save (Plage00)");
        assert_eq!(saved.teleporter, "PlayerStart");
        assert!(
            session.vm_mut().save_slots_mut().unwrap().slot(1).is_some(),
            "provider must refresh after writing"
        );
        menu.poll(session.vm_mut());
        let parent = menu.page.unwrap();
        let retry = session
            .vm()
            .get_property(parent, "RestartBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, retry.clone());
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 13).unwrap();
        assert!(menu.load_slot.is_none(), "cancel must not reload");
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, retry);
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 37).unwrap();
        menu.key(&mut session, 13).unwrap();
        assert_eq!(
            menu.load_slot,
            Some(1),
            "Retry must select the menu-written save"
        );
        let mut restored = Session::open_checkpoint(&game_dir, &saved.map).unwrap();
        restored.restore_checkpoint(&saved).unwrap();
        assert_eq!(restored.player_health(), Some(saved.health));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opt_in_controls_page_writes_config_then_pops_without_unpausing() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to test retail Controls");
            return;
        };
        let dir = std::env::temp_dir().join(format!("xiii-item82-controls-{}", std::process::id()));
        let mut session = Session::open(&game_dir, "Plage00").unwrap();
        let mut menu = InGameMenu::default();
        menu.configure(Some(dir.join("save")), Some("Plage00".into()));
        menu.install_settings(&mut session, &game_dir, &dir.join("config"))
            .unwrap();
        menu.open(&mut session);
        let parent = menu.page.unwrap();
        let focus = session
            .vm()
            .get_property(parent, "ControlsBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, focus);
        menu.key(&mut session, 13).unwrap();
        let page = menu.page.unwrap();
        assert!(
            session
                .vm()
                .set()
                .path(session.vm().objects[page as usize].class)
                .eq_ignore_ascii_case("xidinterf.XIIIMenuInputPC")
        );
        assert!(session.vm().trace.iter().any(|e| matches!(&e.kind, TraceKind::Native { path, args, result, .. } if path == "PlayerController.ConsoleCommand" && args.iter().any(|a| a.contains("KEYNAME 65")) && result.contains('A'))));
        paint_test_page(&mut menu, &mut session);
        let clock = session
            .vm()
            .get_property(session.vm().find_level_info().unwrap(), "TimeSeconds")
            .cloned();
        assert!(session.vm_mut().tick_gui_state(page, f32::NAN).is_err());
        assert!(
            session
                .vm_mut()
                .tick_gui_state(menu.root.unwrap(), 0.1)
                .is_err()
        );
        menu.key(&mut session, 27).unwrap();
        for _ in 0..4 {
            menu.frame(&mut session, 0.1);
        }
        menu.poll(session.vm_mut());
        assert_eq!(menu.page, Some(parent), "{:?}", menu.errors);
        assert!(dir.join("config/menu.ini").exists());
        assert_eq!(
            clock,
            session
                .vm()
                .get_property(session.vm().find_level_info().unwrap(), "TimeSeconds")
                .cloned()
        );
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opt_in_skills_wraps_focus_and_enter_returns_to_parent() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to test retail Skills");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").unwrap();
        let mut menu = InGameMenu::default();
        menu.open(&mut session);
        let parent = menu.page.unwrap();
        let focus = session
            .vm()
            .get_property(parent, "CompetencesBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, focus);
        menu.key(&mut session, 13).unwrap();
        let page = menu.page.unwrap();
        assert!(
            session
                .vm()
                .set()
                .path(session.vm().objects[page as usize].class)
                .eq_ignore_ascii_case("xidinterf.XIIIMenuCompetencesIngame")
        );
        paint_test_page(&mut menu, &mut session);
        let first = session.vm().get_property(page, "FocusedControl").cloned();
        for _ in 0..8 {
            menu.key(&mut session, 40).unwrap();
        }
        assert_eq!(
            first,
            session.vm().get_property(page, "FocusedControl").cloned()
        );
        menu.key(&mut session, 13).unwrap();
        assert_eq!(menu.page, Some(parent));
        assert!(menu.is_open());
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
    }

    #[test]
    fn opt_in_main_menu_confirmation_cancel_then_retail_travel() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to test retail Main menu");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").unwrap();
        let mut menu = InGameMenu::default();
        menu.open(&mut session);
        let parent = menu.page.unwrap();
        let quit = session
            .vm()
            .get_property(parent, "QuitBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, quit.clone());
        menu.key(&mut session, 13).unwrap();
        paint_test_page(&mut menu, &mut session);
        let root = menu.root.unwrap();
        let page = menu.page.unwrap();
        let font = match session
            .vm_mut()
            .send_event(root, "GetMenuFont", vec![Value::Str("GUISmallFont".into())])
            .unwrap()
        {
            Some(Value::Object(Some(ObjRef::Instance(id)))) => id,
            other => panic!("GetMenuFont: {other:?}"),
        };
        let names = session
            .vm()
            .get_property(font, "FontArrayNames")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(font, "FontArrayNames", 0, Value::Array(vec![]));
        assert!(
            session
                .vm_mut()
                .send_event(font, "GetFont", vec![Value::Int(1280)])
                .is_err()
        );
        session
            .vm_mut()
            .set_property(font, "FontArrayNames", 0, names);
        let object_count = session.vm().objects.len();
        let root_class = session.vm().objects[root as usize].class;
        assert!(
            session
                .vm_mut()
                .spawn_gui_font_template(root_class)
                .is_err()
        );
        assert_eq!(
            session.vm().objects.len(),
            object_count,
            "invalid template must not allocate a non-font"
        );
        let width = session.vm().get_property(page, "WinWidth").unwrap().clone();
        let owner = session
            .vm()
            .get_property(page, "MenuOwner")
            .unwrap()
            .clone();
        let bound = session
            .vm()
            .get_property(page, "bBoundToParent")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(page, "WinWidth", 0, Value::Float(f32::NAN));
        assert!(
            session
                .vm_mut()
                .send_event(page, "ActualWidth", vec![])
                .is_err()
        );
        session
            .vm_mut()
            .set_property(page, "WinWidth", 0, Value::Float(0.5));
        session
            .vm_mut()
            .set_property(page, "bBoundToParent", 0, Value::Bool(false));
        assert_eq!(
            session
                .vm_mut()
                .send_event(page, "ActualWidth", vec![])
                .unwrap(),
            Some(Value::Float(640.0))
        );
        session
            .vm_mut()
            .set_property(page, "bBoundToParent", 0, Value::Bool(true));
        session.vm_mut().set_property(
            page,
            "MenuOwner",
            0,
            Value::Object(Some(ObjRef::Instance(page))),
        );
        assert!(
            session
                .vm_mut()
                .send_event(page, "ActualWidth", vec![])
                .is_err()
        );
        session.vm_mut().set_property(page, "WinWidth", 0, width);
        session.vm_mut().set_property(page, "MenuOwner", 0, owner);
        session
            .vm_mut()
            .set_property(page, "bBoundToParent", 0, bound);
        assert!(
            session
                .vm_mut()
                .send_event(root, "GetStyle", vec![Value::Str("MissingStyle".into())])
                .is_err()
        );
        assert!(
            session
                .vm_mut()
                .send_event(root, "GetMenuFont", vec![Value::Str("MissingFont".into())])
                .is_err()
        );
        menu.key(&mut session, 13).unwrap();
        assert!(menu.travel.is_none(), "default No must not quit");
        assert_eq!(menu.page, Some(parent));
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, quit);
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 37).unwrap();
        menu.key(&mut session, 13).unwrap();
        let travel = menu.travel.as_ref().unwrap();
        assert!(InGameMenu::is_front_end_map(&travel.map));
        assert_eq!(travel.url, "MapMenu");
        assert!(!menu.is_open());
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
    }

    /// item89b acceptance: with the windowed `--play` trace gates **off** (`trace_enabled`,
    /// `trace_natives`), the in-game menu's Main menu travel (`QuitGame` → `ClientTravel
    /// MapMenu`) and the Retry load-slot request (`LoadAtCheckpoint`) must still reach the
    /// host — through the VM's bounded host-request queue, not the diagnostic trace. Same
    /// decoded `menu_key` steps as the traced fixtures above (Quit/Retry confirm dialogs
    /// included: the default No must request nothing).
    #[test]
    fn opt_in_traceless_ingame_menu_travel_and_retry_still_reach_the_host() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to run the traceless in-game menu acceptance");
            return;
        };
        let dir = std::env::temp_dir().join(format!("xiii-item89b-menu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Session::open(&game_dir, "Plage00").unwrap();
        // Windowed-play parity (item89): the play plugin sets both gates off on the session's
        // VM; the menu must not depend on the trace.
        session.vm_mut().trace_enabled = false;
        session.vm_mut().trace_natives = false;
        let trace_len = session.vm().trace.len();
        let mut menu = InGameMenu::default();
        menu.configure(Some(dir.clone()), Some("Plage00".into()));
        menu.open(&mut session);
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);

        // Write one save slot through the menu Save page (the newest slot Retry resumes).
        menu.key(&mut session, 13).unwrap();
        let page = menu.page.unwrap();
        assert!(
            session
                .vm()
                .set()
                .path(session.vm().objects[page as usize].class)
                .eq_ignore_ascii_case("xidinterf.XIIIMenuSave"),
            "open page is not the save page with the trace off"
        );
        let slot = match session.vm().get_property(page, "SaveSlots") {
            Some(Value::Array(slots)) => slots[1].clone(),
            other => panic!("SaveSlots: {other:?}"),
        };
        session
            .vm_mut()
            .set_property(page, "FocusedControl", 0, slot);
        menu.key(&mut session, 13).unwrap();
        menu.frame(&mut session, 1.0 / 60.0);
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
        let saved = crate::save::SaveStore::open(dir.clone()).read(1).unwrap();
        assert_eq!(saved.description, "Menu save (Plage00)");

        // Main menu: Enter on Quit opens the confirmation; the default No cancels, Yes
        // requests the front-end travel handoff (and closes the page, like retail).
        menu.poll(session.vm_mut());
        let parent = menu.page.unwrap();
        let quit = session
            .vm()
            .get_property(parent, "QuitBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, quit.clone());
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 13).unwrap();
        assert!(
            menu.travel.is_none(),
            "the confirmation default No must not quit"
        );
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, quit);
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 37).unwrap();
        menu.key(&mut session, 13).unwrap();
        let travel = menu.travel.take().expect("QuitGame travel request");
        assert!(InGameMenu::is_front_end_map(&travel.map), "{:?}", travel);
        assert_eq!(travel.url, "MapMenu");
        assert!(menu.travel.is_none(), "Quit must not load a slot");

        // Retry: Esc reopens the menu, Enter on Restart, the default No must not request,
        // then Yes must hand the newest slot to the host.
        menu.open(&mut session);
        menu.poll(session.vm_mut());
        let parent = menu.page.unwrap();
        let retry = session
            .vm()
            .get_property(parent, "RestartBtn")
            .unwrap()
            .clone();
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, retry.clone());
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 13).unwrap();
        assert!(
            menu.load_slot.is_none(),
            "the default No must not request anything"
        );
        session
            .vm_mut()
            .set_property(parent, "FocusedControl", 0, retry);
        menu.key(&mut session, 13).unwrap();
        menu.key(&mut session, 37).unwrap();
        menu.key(&mut session, 13).unwrap();
        assert_eq!(
            menu.load_slot,
            Some(1),
            "Retry load-slot request must reach the host with the trace off"
        );
        assert_eq!(
            session.vm().trace.len(),
            trace_len,
            "the diagnostic trace must not grow while the gates are off"
        );
        assert!(menu.errors.is_empty(), "{:?}", menu.errors);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Opt-in corpus test (item72): on Plage00 the retail open path pauses the level clock
    /// (`SetPause` → `Level.Pauser`), the page's own Esc handler closes and unpauses, the clock
    /// runs again.
    #[test]
    fn opt_in_ingame_menu_pauses_clock_and_resumes() {
        let Some(game_dir) = std::env::var_os("XIII_GOG_DIR").map(PathBuf::from) else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session =
            Session::open(std::path::Path::new(&game_dir), "Plage00").expect("open Plage00");
        let mut menu = InGameMenu::default();
        menu.configure(None, Some("Plage00".into()));

        let level = session.vm_mut().find_level_info().expect("LevelInfo");
        let clock = |session: &Session| match session.vm().get_property(level, "TimeSeconds") {
            Some(Value::Float(t)) => *t,
            other => panic!("TimeSeconds is {other:?}"),
        };

        // --- open: the decoded ShowTheMenu path pauses the level --------------------------
        menu.open(&mut session);
        assert!(
            menu.is_open(),
            "menu did not open: errors {:?}",
            menu.errors
        );
        let vm = session.vm_mut();
        let page = menu.page.expect("open page");
        assert!(
            vm.set()
                .path(vm.objects[page as usize].class)
                .eq_ignore_ascii_case("xidinterf.xiiimenuingame"),
            "active page is {}",
            vm.set().path(vm.objects[page as usize].class)
        );
        let pauser = vm.get_property(level, "Pauser").cloned();
        assert!(
            !matches!(pauser, Some(Value::Object(None)) | None),
            "Level.Pauser must be set after the open, got {pauser:?}"
        );

        // --- paused: the level clock freezes across ticks ---------------------------------
        let before = clock(&session);
        for _ in 0..6 {
            session.vm_mut().tick_suspending(0.1);
        }
        let after = clock(&session);
        assert_eq!(
            before, after,
            "the level clock must be frozen while the page is open"
        );
        println!("[menu test] clock frozen at {before}s across 6 x 0.1s ticks");

        // The page's Return/Esc handler closes through the decoded root EndState pause path.
        menu.request_close(&mut session);
        assert!(!menu.is_open(), "Esc must close the page and resume");
        let unpaused = clock(&session);
        for _ in 0..6 {
            session.vm_mut().tick_suspending(0.1);
        }
        let running = clock(&session);
        assert!(
            running > unpaused,
            "clock did not resume: {unpaused} -> {running}"
        );
        println!("[menu test] resumed clock: {unpaused} -> {running}");
    }
}
