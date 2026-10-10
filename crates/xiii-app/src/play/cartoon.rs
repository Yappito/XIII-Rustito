//! XIII's comic-panel "cartoon effect" for `--play`.
//!
//! **Evidence (decoded scripts).** The effect is the HUD's own script, not a native:
//!
//! * `xiii.XIIIBaseHud.DrawHUD(Canvas)` sets `HudCartoonSFX` and, when
//!   `Level.InitialCartoonEffect == 0`, immediately writes
//!   `XIIIGameInfo(Level.Game).MapInfo.EndCartoonEffect = true` (the "no cartoon" fast path).
//! * `xiii.XIIIBaseHud.DrawCartoonWindowBis(Canvas)` is the presentation. On its first call it
//!   resolves `Level.InitialCartoonEffect == -1` to `Rand(3)+1` (one of the map's comic layouts),
//!   lays out five panels (`X`/`Y`/`W`/`H` + animated `OffsetX/Y/W/H` scaled by `SpeedFactor`),
//!   cycles the active panel every `switchDelay` seconds while the player is still, and draws
//!   the panels with `Canvas.DrawTile` (black bars via `WhiteTex`, panel content via the HUD's
//!   `CWndMat` `RenderTargetMaterial`). It ends — setting `InitialCartoonEffect = 0`,
//!   `HudCartoonSFX = false`, `MapInfo.EndCartoonEffect = true` and firing the
//!   `'EndCartoonEffect'` event — when the active panel has grown to the full clip
//!   (`W == ClipX && H == ClipY && X <= 0 && Y <= 0`), or as soon as the player moves
//!   (`PawnOwner.Velocity != 0`).
//! * `xidcine.Cine2.CineInit` (the Plage00 opening) blocks on `MI.EndCartoonEffect` while
//!   `XIIIGameInfo.CheckpointNumber < 2`, which is exactly why the intro never started in
//!   item3l.
//!
//! The script drives the presentation through the normal `HUD.PostRender(Canvas)` path the
//! item10 HUD pipeline already runs, so the panel rectangles, borders and tiles are recorded as
//! `DrawCommand`s and rendered by [`super::hud`]. This module only **observes** the script state
//! (it never mutates simulation fields): it tracks the `EndCartoonEffect` gate, the panel
//! layout and the `RenderTargetMaterial` camera updates the script emits, and reports the
//! timeline on exit.

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use xiii_script::{ObjRef, ObjectId, PresentationEvent, Value, Vm};

use super::hud::HudRuntime;
use super::session::Session;

/// Number of comic panels the HUD lays out (`XIIIBaseHud.X[0..5]`).
pub const PANELS: usize = 5;

/// One panel rectangle in canvas pixels (the HUD's `X`/`Y`/`W`/`H` arrays).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Panel {
    /// Left edge (`X`).
    pub x: f32,
    /// Top edge (`Y`).
    pub y: f32,
    /// Width (`W`).
    pub w: f32,
    /// Height (`H`).
    pub h: f32,
}

/// Latest `RenderTargetMaterial.Update` camera the script requested, to render-to-texture.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderTargetUpdate {
    /// `RenderTargetMaterial` actor name (the `DrawTile` material the panels sample).
    pub actor: String,
    /// Destination rect in the render target.
    pub x: i32,
    /// Destination rect top.
    pub y: i32,
    /// Destination width.
    pub width: i32,
    /// Destination height.
    pub height: i32,
    /// Camera location (Unreal units).
    pub cam_location: [f32; 3],
    /// Camera rotation (Unreal rotator units).
    pub cam_rotation: [i32; 3],
    /// Vertical FOV in degrees.
    pub fov: f32,
    /// VM time.
    pub time: f64,
}

/// Observed cartoon-effect state, refreshed every rendered frame.
#[derive(Resource, Default)]
pub struct CartoonState {
    /// `MapInfo.EndCartoonEffect` (the flag `Cine2.CineInit` waits on).
    pub end_cartoon: bool,
    /// VM time `EndCartoonEffect` first became true.
    pub end_time: Option<f64>,
    /// `Level.InitialCartoonEffect` (-1 unresolved, 0 none, 1..4 layout).
    pub initial_effect: Option<i32>,
    /// `XIIIBaseHud.HudCartoonSFX`.
    pub hud_cartoon_sfx: Option<bool>,
    /// `XIIIBaseHud.HideCartoonHud`.
    pub hide_cartoon_hud: Option<bool>,
    /// `XIIIBaseHud.RealTimeWndId` (active panel index).
    pub real_time_wnd_id: Option<i32>,
    /// `XIIIBaseHud.CartoonWindowNumber`.
    pub panel_number: Option<i32>,
    /// Current panel rectangles.
    pub panels: [Panel; PANELS],
    /// Latest render-target camera request (the panel content view).
    pub render_target: Option<RenderTargetUpdate>,
    /// `PlayMenu` sounds the cartoon path requested.
    pub play_menus: u64,
    /// `RenderTargetMaterial` update calls observed.
    pub render_updates: u64,
    /// Ordered timeline (state changes and sounds).
    pub log: Vec<String>,
    /// One-shot chain diagnostic printed once.
    diagnosed: bool,
    /// Cumulative event cursor: render callbacks can emit multiple events at one VM time.
    last_event_total: u64,
}

/// Render target size in texels (`Engine.RenderTargetMaterial` default `USize`/`VSize` = 256).
pub const RENDER_TARGET_SIZE: u32 = 256;

/// The render-to-texture camera that draws the panel content.
#[derive(Component)]
pub struct CartoonCam;

/// One render-to-texture camera rendering the camera pose the script set through
/// `RenderTargetMaterial.Update`, so the panel `Canvas.DrawTile(HUD.CWndMat, ...)` commands can
/// sample it. The camera only runs while the script's cartoon presentation is active
/// (`HudCartoonSFX`), so maps without the effect (Plage00) are unaffected.
#[derive(Resource, Default)]
pub struct CartoonRenderTarget {
    /// Render-target image, created on the first panel update.
    pub image: Option<Handle<Image>>,
    /// The render camera entity.
    pub camera: Option<Entity>,
    /// `CWndMat` material name the panels sample.
    pub material: Option<String>,
    /// True while the camera is rendering.
    pub active: bool,
    /// Frames the camera has rendered.
    pub frames: u64,
}

/// Live instance in property `name`, or `None`.
fn instance_object(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(i))))
            if vm.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
        {
            Some(*i)
        }
        _ => None,
    }
}

/// Reads an `int` property (accepting `Byte`), or `None`.
fn int_prop(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<i32> {
    match vm.get_property(id, name)? {
        Value::Int(v) => Some(*v),
        Value::Byte(v) => Some(i32::from(*v)),
        _ => None,
    }
}

/// Reads a `bool` property, or `None`.
fn bool_prop(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<bool> {
    match vm.get_property(id, name)? {
        Value::Bool(v) => Some(*v),
        _ => None,
    }
}

/// Reads `f32` array element `i` of property `name`, or `None`.
fn float_elem(vm: &Vm<'_>, id: ObjectId, name: &str, i: usize) -> Option<f32> {
    match vm.get_property_elem(id, name, i)? {
        Value::Float(v) => Some(*v),
        _ => None,
    }
}

/// The `RenderTargetMaterial` actor the panels sample, if any (`XIIIBaseHud.CWndMat`).
fn cwnd_material(vm: &Vm<'_>, hud: ObjectId) -> Option<String> {
    instance_object(vm, hud, "CWndMat").map(|m| vm.objects[m as usize].name.clone())
}

/// The material name the panels sample, for the render-to-texture bridge (`None` when the HUD
/// has not created its `RenderTargetMaterial`).
pub fn cwnd_material_name(session: &Session, hud: &HudRuntime) -> Option<String> {
    let hud = hud.hud?;
    cwnd_material(session.vm(), hud)
}

/// Observes the script's cartoon state each rendered frame (after `hud::refresh` has called
/// `HUD.PostRender`). Never mutates the VM.
pub fn collect(
    session: NonSend<Result<Session, String>>,
    hud: Option<Res<HudRuntime>>,
    mut state: ResMut<CartoonState>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    let session = match &*session {
        Ok(s) => s,
        Err(_) => return,
    };
    let vm = session.vm();
    let now = vm.time;

    // The gate: XIIIGameInfo(Level.Game).MapInfo.EndCartoonEffect. Fall back to Cine0's own `MI`.
    let mut mi = session
        .game_info
        .and_then(|gi| instance_object(vm, gi, "MapInfo"));
    if mi.is_none() {
        mi = vm
            .find_object("Cine0")
            .and_then(|c| instance_object(vm, c, "MI"));
    }
    let end = mi
        .and_then(|mi| bool_prop(vm, mi, "EndCartoonEffect"))
        .unwrap_or(false);
    if end && !state.end_cartoon {
        state.end_time = Some(now);
        state
            .log
            .push(format!("[{now:.3}s] MapInfo.EndCartoonEffect = true"));
    }
    state.end_cartoon = end;

    // Level/InitialCartoonEffect and the HUD's cartoon fields.
    let initial = vm
        .find_level_info()
        .and_then(|li| int_prop(vm, li, "InitialCartoonEffect"));
    if initial != state.initial_effect {
        state.initial_effect = initial;
    }
    if let Some(hud) = hud.as_deref().and_then(|h| h.hud) {
        let sfx = bool_prop(vm, hud, "HudCartoonSFX");
        if sfx != state.hud_cartoon_sfx {
            state
                .log
                .push(format!("[{now:.3}s] HudCartoonSFX = {sfx:?}"));
            state.hud_cartoon_sfx = sfx;
        }
        let hide = bool_prop(vm, hud, "HideCartoonHud");
        if hide != state.hide_cartoon_hud {
            state
                .log
                .push(format!("[{now:.3}s] HideCartoonHud = {hide:?}"));
            state.hide_cartoon_hud = hide;
        }
        state.panel_number = int_prop(vm, hud, "CartoonWindowNumber");
        let wnd = int_prop(vm, hud, "RealTimeWndId");
        if wnd != state.real_time_wnd_id {
            state
                .log
                .push(format!("[{now:.3}s] cartoon panel id -> {wnd:?}"));
            state.real_time_wnd_id = wnd;
        }
        for (i, panel) in state.panels.iter_mut().enumerate() {
            panel.x = float_elem(vm, hud, "X", i).unwrap_or(0.0);
            panel.y = float_elem(vm, hud, "Y", i).unwrap_or(0.0);
            panel.w = float_elem(vm, hud, "W", i).unwrap_or(0.0);
            panel.h = float_elem(vm, hud, "H", i).unwrap_or(0.0);
        }
    }

    // HUD callbacks emit events after the fixed-step drain, often at the same VM timestamp as
    // the preceding rendered frame. A time cursor discarded these, including updates at t=0.
    let start = unseen_event_start(
        session.event_total,
        session.events.len(),
        state.last_event_total,
    );
    for (t, ev) in session.events.iter().skip(start) {
        match ev {
            PresentationEvent::PlaySound(e) if e.actor.starts_with("XIIIBaseHud") => {
                state.play_menus += 1;
                state.log.push(format!(
                    "[{t:.3}s] PlayMenu {} {}",
                    e.actor,
                    e.sound.as_deref().unwrap_or("<none>")
                ));
            }
            PresentationEvent::RenderTarget(rt) => {
                state.render_updates += 1;
                state.render_target = Some(RenderTargetUpdate {
                    actor: rt.actor.clone(),
                    x: rt.x,
                    y: rt.y,
                    width: rt.width,
                    height: rt.height,
                    cam_location: rt.cam_location,
                    cam_rotation: rt.cam_rotation,
                    fov: rt.fov,
                    time: rt.time,
                });
                state.log.push(format!(
                    "[{t:.3}s] RenderTargetMaterial.Update {} rect {}x{} cam ({:.0},{:.0},{:.0})",
                    rt.actor,
                    rt.width,
                    rt.height,
                    rt.cam_location[0],
                    rt.cam_location[1],
                    rt.cam_location[2]
                ));
            }
            _ => {}
        }
    }
    state.last_event_total = session.event_total;

    if !state.diagnosed {
        state.diagnosed = true;
        let gi = session.game_info;
        let mapinfo = gi.and_then(|gi| instance_object(vm, gi, "MapInfo"));
        state.log.push(format!(
            "[{now:.3}s] chain: Level.InitialCartoonEffect={initial:?} GameInfo={} MapInfo={}",
            gi.map_or("-".to_owned(), |g| vm.objects[g as usize].name.clone()),
            mapinfo.map_or("-".to_owned(), |m| vm.objects[m as usize].name.clone()),
        ));
    }
    perf.span("cartoon_collect", t0);
}

fn unseen_event_start(total: u64, retained: usize, seen: u64) -> usize {
    let unseen = if seen > total { total } else { total - seen };
    retained.saturating_sub(unseen.min(retained as u64) as usize)
}

#[cfg(test)]
mod event_cursor_tests {
    use super::unseen_event_start;

    #[test]
    fn event_cursor_keeps_same_timestamp_updates_and_handles_retained_window_wrap() {
        let timestamps = [0.0, 0.0, 0.0];
        assert_eq!(&timestamps[unseen_event_start(3, 3, 1)..], &[0.0, 0.0]);
        assert_eq!(unseen_event_start(3, 3, 3), 3);
        assert_eq!(unseen_event_start(100, 64, 0), 0);
        assert_eq!(unseen_event_start(100, 64, 98), 62);
        assert_eq!(unseen_event_start(0, 0, 100), 0);
        assert_eq!(unseen_event_start(3, 3, 100), 0);
    }
}

/// Keeps the render-to-texture camera in sync with the script's requested panel view. Runs after
/// [`collect`] (which reads the `RenderTargetMaterial.Update` event) and before `hud::draw`
/// (which substitutes the image for the `CWndMat` tile). Inactive maps never spawn the camera.
#[allow(clippy::too_many_arguments)]
pub fn sync_render_target(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut rt: ResMut<CartoonRenderTarget>,
    state: Res<CartoonState>,
    session: NonSend<Result<Session, String>>,
    hud: Option<Res<HudRuntime>>,
    mut cams: Query<(&mut Camera, &mut Transform), With<CartoonCam>>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    let Ok(session) = &*session else {
        return;
    };
    if let Some(name) = hud.as_deref().and_then(|h| cwnd_material_name(session, h)) {
        rt.material = Some(name);
    }
    let pose = state
        .render_target
        .clone()
        .filter(|_| state.hud_cartoon_sfx == Some(true));
    let Some(pose) = pose else {
        rt.active = false;
        if let Some(e) = rt.camera
            && let Ok((mut cam, _)) = cams.get_mut(e)
        {
            cam.is_active = false;
        }
        return;
    };
    if rt.camera.is_none() {
        let mut image = Image::new_fill(
            Extent3d {
                width: RENDER_TARGET_SIZE,
                height: RENDER_TARGET_SIZE,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[0, 0, 0, 255],
            TextureFormat::Bgra8UnormSrgb,
            RenderAssetUsages::default(),
        );
        image.texture_descriptor.usage |=
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING;
        let handle = images.add(image);
        let mut cam = crate::viewer::main_camera_config(false);
        cam.order = -1;
        let entity = commands
            .spawn((
                Camera3d::default(),
                // This camera sees the same forward decals as the player camera. Bevy's
                // decal shader samples prepass_depth and cannot compile without this pass.
                bevy::core_pipeline::prepass::DepthPrepass,
                cam,
                RenderTarget::Image(handle.clone().into()),
                RenderLayers::layer(crate::viewer::MAIN_LAYER),
                Transform::default(),
                CartoonCam,
            ))
            .id();
        rt.image = Some(handle);
        rt.camera = Some(entity);
    }
    if let Some(e) = rt.camera
        && let Ok((mut cam, mut t)) = cams.get_mut(e)
    {
        cam.is_active = true;
        let (loc, rot) = super::cinematics::camera_transform(pose.cam_location, pose.cam_rotation);
        t.translation = loc;
        t.rotation = rot;
    }
    rt.active = true;
    rt.frames += 1;
    perf.span("cartoon_sync", t0);
}

/// Prints the cartoon timeline on exit, so an unattended run records the effect.
pub fn report_exit(
    state: Res<CartoonState>,
    session: NonSend<Result<Session, String>>,
    mut exiting: MessageReader<AppExit>,
) {
    if exiting.read().next().is_none() {
        return;
    }
    if let Ok(s) = &*session {
        let vm = s.vm();
        if let Some(c) = vm.find_object("Cine0") {
            println!(
                "[cartoon] Cine0 final: state={:?} bInitialized={:?} MI={}",
                vm.state_name(c),
                bool_prop(vm, c, "bInitialized"),
                instance_object(vm, c, "MI")
                    .map_or("-".to_owned(), |m| vm.objects[m as usize].name.clone()),
            );
        }
        if let Some(c) = vm.find_object("Cine0")
            && let Some(Value::Array(items)) = vm.get_property(c, "tabActions")
            && let Some(ctrl) = vm.find_object("CineController21")
            && let Some(Value::Int(idx)) = vm.get_property(ctrl, "ScriptedActionIndex")
            && let Some(Value::Str(action)) = items.get((*idx).max(0) as usize)
        {
            println!("[cartoon] current cine action[{idx}] = {action}");
        }
        if let Some(c) = vm.find_object("CineController21") {
            println!(
                "[cartoon] sequence action index {} / {} (flagsPaused {:?})",
                vm.get_property(c, "ScriptedActionIndex")
                    .map_or_else(|| "-".to_owned(), |v| format!("{v:?}")),
                vm.get_property(c, "WaitTimeEnd")
                    .map_or_else(|| "-".to_owned(), |_| "waiting".to_owned()),
                vm.get_property(c, "flagsPaused")
                    .map_or_else(|| "-".to_owned(), |v| format!("{v:?}")),
            );
        }
        for i in 0..vm.objects.len() {
            let id = i as ObjectId;
            let o = &vm.objects[i];
            if o.deleted || !o.is_actor || !o.name.starts_with("CineController") {
                continue;
            }
            println!(
                "[cartoon] {} state={:?} action={:?} flags={:?} warn={:?}",
                o.name,
                vm.state_name(id),
                vm.get_property(id, "ScriptedActionIndex"),
                vm.get_property(id, "flagsPaused"),
                vm.get_property(id, "WarnMemory"),
            );
        }
        if !s.saves.is_empty() {
            println!("[cartoon] checkpoint saves ({}):", s.save_total);
            for (t, e) in &s.saves {
                println!(
                    "[cartoon]   [{t:.3}s] SaveAtCheckpoint {} teleporter={:?} description={:?}",
                    e.actor, e.teleporter_name, e.description
                );
            }
        }
        if let Some(e) = s.first_error() {
            for line in e.lines() {
                println!("[cartoon] first script error: {line}");
            }
        }
        if s.failures.len() > 1 {
            println!("[cartoon] all suspensions ({}):", s.failures.len());
            for (name, err) in &s.failures {
                let first = err.lines().next().unwrap_or("<error>");
                println!("[cartoon]   {name}: {first}");
            }
        }
        if !s.suspended.is_empty() {
            println!(
                "[cartoon] suspended ({}) {}",
                s.suspended.len(),
                s.suspended.join(", ")
            );
        }
        if state.hud_cartoon_sfx == Some(true)
            && let Some(hud) = super::hud::find_hud(vm)
        {
            let li = vm.find_level_info();
            println!(
                "[cartoon] cartoon HUD diag: bHideHUD={:?} HudCartoonInit={:?} CanDisplay={:?} LevelAction={:?}",
                bool_prop(vm, hud, "bHideHUD"),
                bool_prop(vm, hud, "HudCartoonInit"),
                bool_prop(vm, hud, "CanDisplay"),
                li.and_then(|l| vm.get_property(l, "LevelAction").cloned()),
            );
        }
    }
    println!(
        "[cartoon] exit: EndCartoonEffect={} at {} | initial={:?} HudCartoonSFX={:?} HideCartoonHud={:?} panel={:?}/{:?} | PlayMenu {} render-updates {}",
        state.end_cartoon,
        state
            .end_time
            .map_or_else(|| "-".to_owned(), |t| format!("{t:.3}s")),
        state.initial_effect,
        state.hud_cartoon_sfx,
        state.hide_cartoon_hud,
        state.real_time_wnd_id,
        state.panel_number,
        state.play_menus,
        state.render_updates,
    );
    for line in &state.log {
        println!("[cartoon]   {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus test (requirement 4): the Plage00 opening runs the game's own cartoon path.
    /// The HUD's `PostRender`/`DrawHUD` sees `Level.InitialCartoonEffect == 0`, writes
    /// `MapInfo.EndCartoonEffect = true`, and the `Cine2.CineInit` gate opens so the `dial
    /// dial_debut` action speaks "I can't remember a thing..." through `DialogueManager0`.
    #[test]
    fn opt_in_plage00_end_cartoon_effect_and_intro_dialogue() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        let mut images = Assets::<Image>::default();
        let hud = super::super::hud::setup(&mut session, &game_dir, &mut images)
            .expect("set up the script HUD");
        let canvas = hud.canvas.expect("HUD canvas");
        let hud_id = hud.hud.expect("live HUD actor");

        let mut seen = 0u64;
        let mut lines: Vec<String> = Vec::new();
        let mut end_time = None;
        let mut loc = session.player_location().unwrap_or([0.0; 3]);
        // 90 s at the fixed 60 Hz step.
        for _ in 0..5400 {
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &crate::play::session::PlayerVMModes::default(),
            );
            {
                let vm = session.vm_mut();
                vm.set_property(canvas, "ClipX", 0, Value::Float(1280.0));
                vm.set_property(canvas, "ClipY", 0, Value::Float(720.0));
                let arg = Value::Object(Some(ObjRef::Instance(canvas)));
                let _ = vm.send_event(hud_id, "PostRender", vec![arg]);
            }
            for d in session.new_dialogues(&mut seen) {
                if d.text.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                    lines.push(format!("[{:.3}s] {} text={:?}", d.time, d.sound, d.text));
                }
            }
            if end_time.is_none() {
                let vm = session.vm();
                let end = session
                    .game_info
                    .and_then(|gi| instance_object(vm, gi, "MapInfo"))
                    .and_then(|mi| bool_prop(vm, mi, "EndCartoonEffect"))
                    .unwrap_or(false);
                if end {
                    end_time = Some(session.vm_time());
                }
            }
            loc = session.player_location().unwrap_or(loc);
        }
        println!(
            "[cartoon test] EndCartoonEffect at {end_time:?}; {} dialogue(s) with text: {lines:?}",
            lines.len()
        );
        assert!(
            end_time.is_some(),
            "Plage00 never set MapInfo.EndCartoonEffect"
        );
        assert!(
            !lines.is_empty(),
            "the opening emitted no dialogue with resolved text"
        );
    }
}
