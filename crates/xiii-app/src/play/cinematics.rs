//! Cutscene cameras, view targets and dialogue subtitles for `--play`.
//!
//! **Evidence (decoded scripts).** The map's scripted sequences run through the `xidcine` package:
//!
//! * `CineController2.Interpret` handles the sequence actions `FPC`/`FreezePlayer`
//!   (`PC.GotoState('NoControl')`) and `FPL`/`FreezePlayerLocation` (`PC.GotoState('NoMove')`), so
//!   the host suppresses movement while the player controller is in one of those states.
//! * `XIIIPlayerController.SetCamView(C, F)` sets `self.CamView = C`, forces `DesiredFOV` and
//!   enters the `CameraView` state; `XIIIPlayerController.CameraView.PlayerCalcView` then sets
//!   `CameraLocation = self.CamView.Location` and `CameraRotation = self.CamView.Rotation`.
//! * `Engine.PlayerController.SetViewTarget(NewViewTarget)` stores `ViewTarget`; the general
//!   `PlayerCalcView` path renders from it when it is not the player pawn.
//!
//! The host therefore renders the main camera from `CamView` (when the controller is in
//! `CameraView`) or from `ViewTarget` (when it is not the player pawn), using the actor's own
//! `Location`/`Rotation`. Dialogue subtitles come from the typed
//! [`xiii_script::PresentationEvent::Dialogue`] events the VM emits for `Actor.PlayStrVoice`,
//! whose text was resolved from the `DialogueManager`'s `Lines`/`Speakers` map data.
//!
//! This is a **presentation** layer: it never mutates simulation fields. Input suppression is a
//! host decision that mirrors the script's own `NoControl`/`NoMove`/`CameraView` states so the
//! movement simulation cannot walk the pawn out of a cutscene.

use bevy::prelude::*;
use xiii_decode::common::{ROTATOR_UNITS_PER_TURN, to_bevy_position};
use xiii_script::{ObjectId, Value, Vm};

use super::session::Session;

/// Where the cutscene camera pose comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewSource {
    /// `XIIIPlayerController.CameraView`: `CamView.Location`/`Rotation`.
    CameraView,
    /// A `ViewTarget` actor that is not the player pawn.
    ViewTarget,
}

impl ViewSource {
    /// Short stable label for the trace/report.
    pub fn as_str(self) -> &'static str {
        match self {
            ViewSource::CameraView => "CameraView(CamView)",
            ViewSource::ViewTarget => "ViewTarget",
        }
    }
}

/// A cutscene camera pose in Unreal units/rotator units.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewPose {
    /// Camera `Location` (UU).
    pub location: [f32; 3],
    /// Camera `Rotation` (Unreal rotator units).
    pub rotation: [i32; 3],
    /// Actor the pose was read from.
    pub actor: String,
    /// Which script path selected it.
    pub source: ViewSource,
}

/// One on-screen subtitle line.
#[derive(Debug, Clone, PartialEq)]
pub struct Subtitle {
    /// Speaking actor (`RollOffActor` name), if any.
    pub speaker: Option<String>,
    /// Displayed text (resolved line, or the voice name when no text is available).
    pub text: String,
    /// Voice name.
    pub sound: String,
    /// VM time the line started.
    pub start: f64,
    /// VM time the line should disappear.
    pub end: f64,
}

/// Cinematic presentation state, updated each fixed step and drawn in `Update`.
#[derive(Resource, Default)]
pub struct CinematicState {
    /// Current cutscene view, `None` when rendering the player pawn.
    pub view: Option<ViewPose>,
    /// True while the scripts say the player must not move.
    pub input_suppressed: bool,
    /// Active subtitles (oldest first).
    pub subtitles: Vec<Subtitle>,
    /// Total dialogue events consumed.
    pub dialogues: u64,
    /// VM time of the last update.
    pub vm_time: f64,
    /// Timeline entries: view changes and dialogue lines, in order.
    pub log: Vec<String>,
    /// Number of camera source changes (for the report).
    pub view_changes: u64,
    /// Total dialogue lines seen (for the report).
    pub lines: u64,
    /// Shared counter of voice names the `VoiceDuration` provider could not resolve (set when the
    /// provider is installed; those lines use the script's `NoSound` fallback).
    pub voice_unresolved: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Last `(view actor, source)` so a change is logged once.
    last_view: Option<(String, ViewSource)>,
    /// Last controller state used for suppression logging.
    last_suppress_state: Option<String>,
}

/// The subtitle UI text marker.
#[derive(Component)]
pub struct SubtitleText;

/// True when `id` holds a live instance in object property `name`.
fn instance_object(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(xiii_script::ObjRef::Instance(i))))
            if vm.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
        {
            Some(*i)
        }
        _ => None,
    }
}

/// `(Location, Rotation)` of a live actor, or `None` if either is missing.
fn actor_pose(vm: &Vm<'_>, id: ObjectId) -> Option<([f32; 3], [i32; 3])> {
    let loc = vm.vector_prop(id, "Location")?;
    let rot = match vm.get_property(id, "Rotation") {
        Some(Value::Rotator(r)) => *r,
        _ => [0; 3],
    };
    Some((loc, rot))
}

/// Selects the cutscene view from already-read script state, per the decoded script paths.
/// `CameraView` (the `SetCamView` entry point) wins over a general `ViewTarget`; a `ViewTarget`
/// that is the player pawn is not a cutscene camera. `None` means "render from the player pawn".
pub fn choose_view(
    in_camera_view: bool,
    cam_view: Option<(String, [f32; 3], [i32; 3])>,
    view_target: Option<(String, [f32; 3], [i32; 3], bool)>,
) -> Option<ViewPose> {
    if in_camera_view && let Some((actor, location, rotation)) = cam_view {
        return Some(ViewPose {
            location,
            rotation,
            actor,
            source: ViewSource::CameraView,
        });
    }
    if let Some((actor, location, rotation, is_player)) = view_target
        && !is_player
    {
        return Some(ViewPose {
            location,
            rotation,
            actor,
            source: ViewSource::ViewTarget,
        });
    }
    None
}

/// Selects the cutscene view from the player controller, per the decoded script paths.
/// `None` means "render from the player pawn" (no cutscene camera is active).
pub fn active_view(session: &Session) -> Option<ViewPose> {
    let pc = session.controller?;
    let vm = session.vm();
    let cam_view = instance_object(vm, pc, "CamView").and_then(|cam| {
        actor_pose(vm, cam).map(|(l, r)| (vm.objects[cam as usize].name.clone(), l, r))
    });
    let view_target = instance_object(vm, pc, "ViewTarget").and_then(|vt| {
        actor_pose(vm, vt).map(|(l, r)| {
            (
                vm.objects[vt as usize].name.clone(),
                l,
                r,
                vt == session.player,
            )
        })
    });
    choose_view(vm.is_in_state(pc, "CameraView"), cam_view, view_target)
}

/// True while the scripts freeze the player: `CineController2.Interpret` puts the controller in
/// `NoControl` (`FPC`/`FreezePlayer`) or `NoMove` (`FPL`/`FreezePlayerLocation`); `CameraView` and
/// `PlayingVideo` likewise ignore movement input.
pub fn input_suppressed(session: &Session) -> bool {
    let Some(pc) = session.controller else {
        return false;
    };
    let vm = session.vm();
    ["NoControl", "NoMove", "CameraView", "PlayingVideo"]
        .iter()
        .any(|s| vm.is_in_state(pc, s))
}

/// The suppression state name, for logging.
fn suppress_state(session: &Session) -> Option<String> {
    let pc = session.controller?;
    let vm = session.vm();
    ["NoControl", "NoMove", "CameraView", "PlayingVideo"]
        .into_iter()
        .find(|s| vm.is_in_state(pc, s))
        .map(str::to_owned)
}

/// Bevy `(translation, rotation)` of an Unreal camera pose. Uses the same axis policy as the
/// rest of the runtime ([`to_bevy_position`]) and the `YXZ` euler order the player camera uses;
/// the roll sign is a **hypothesis** (cutscene cameras in the corpus have roll 0).
pub fn camera_transform(location: [f32; 3], rotation: [i32; 3]) -> (Vec3, Quat) {
    let k = std::f32::consts::TAU / ROTATOR_UNITS_PER_TURN;
    let pitch = rotation[0] as f32 * k;
    let yaw = rotation[1] as f32 * k;
    let roll = rotation[2] as f32 * k;
    (
        Vec3::from_array(to_bevy_position(location)),
        Quat::from_euler(EulerRot::YXZ, -yaw, pitch, roll),
    )
}

/// Reads new dialogue events and the current view from the session. Runs in `Update` before
/// [`sync_camera`](super::sync_camera). `audio` is used (when present and enabled) to compute the
/// real decoded length of a voice so the subtitle stays up as long as the wave.
pub fn collect(
    mut session: NonSendMut<Result<Session, String>>,
    mut state: ResMut<CinematicState>,
    mut audio: Option<ResMut<crate::audio::AudioRes>>,
) {
    let session = match &mut *session {
        Ok(session) => session,
        Err(_) => return,
    };
    // Install the decoded voice-duration provider once the audio library exists, so
    // `Actor.PlayStrVoice` takes the engine's voice-completion path (real wave length) instead of
    // the script's `NoSound` fallback. The provider shares the same `SoundLibrary` the audio layer
    // already scanned; unresolved names keep returning `false` and are counted.
    if !session.vm().has_voice_duration()
        && let Some(audio) = audio.as_mut()
        && audio.stats.enabled
    {
        let (provider, unresolved) =
            crate::play::voice::LibraryVoiceDuration::new(audio.library.clone());
        session.vm_mut().set_voice_duration(Box::new(provider));
        state.voice_unresolved = Some(unresolved);
        println!("[cine] voice-duration provider installed from the decoded HX library");
    }
    state.vm_time = session.vm_time();

    let mut seen = state.dialogues;
    let new: Vec<xiii_script::DialogueEvent> = session
        .new_dialogues(&mut seen)
        .into_iter()
        .cloned()
        .collect();
    state.dialogues = seen;

    for d in new {
        let duration = d.duration.or_else(|| {
            let audio = audio.as_ref()?;
            voice_duration(audio, &d.sound)
        });
        let text = d
            .text
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("[{}]", d.sound));
        let end = d.time + f64::from(duration.unwrap_or(3.0)).max(0.5);
        state.lines += 1;
        state.log.push(format!(
            "[{:.3}s] dialogue speaker={} sound={} duration={} text={:?}",
            d.time,
            d.speaker.as_deref().unwrap_or("-"),
            d.sound,
            duration.map_or_else(|| "?".to_owned(), |x| format!("{x:.3}s")),
            text
        ));
        state.subtitles.push(Subtitle {
            speaker: d.speaker.clone(),
            text,
            sound: d.sound.clone(),
            start: d.time,
            end,
        });
    }

    // Current view and its changes.
    let now = state.vm_time;
    let view = active_view(session);
    match &view {
        Some(p) => {
            let key = (p.actor.clone(), p.source);
            if state.last_view.as_ref() != Some(&key) {
                state.view_changes += 1;
                state.log.push(format!(
                    "[{now:.3}s] view -> {} ({}) at ({:.1}, {:.1}, {:.1}) UU rot ({}, {}, {})",
                    p.actor,
                    p.source.as_str(),
                    p.location[0],
                    p.location[1],
                    p.location[2],
                    p.rotation[0],
                    p.rotation[1],
                    p.rotation[2]
                ));
                state.last_view = Some(key);
            }
        }
        None => {
            if state.last_view.is_some() {
                state.log.push(format!("[{now:.3}s] view -> player pawn"));
                state.last_view = None;
            }
        }
    }
    state.view = view;

    // Input suppression changes.
    let st = suppress_state(session);
    if st != state.last_suppress_state {
        state
            .log
            .push(format!("[{now:.3}s] input suppressed by {st:?}"));
        state.last_suppress_state = st;
    }
    state.input_suppressed = state.last_suppress_state.is_some();

    state.subtitles.retain(|s| s.end > now);
}

/// Real decoded length of a voice name from the audio library, if available.
fn voice_duration(audio: &crate::audio::AudioRes, sound: &str) -> Option<f32> {
    if !audio.stats.enabled {
        return None;
    }
    let library = audio.library.clone();
    let mut library = library.lock().unwrap_or_else(|e| e.into_inner());
    let resolved = library.resolve_path(sound)?;
    let pcm = library.load(&resolved.entry).ok()?;
    let frames = pcm.samples.len() as f32 / f32::from(pcm.channels.max(1));
    (pcm.sample_rate > 0).then(|| frames / pcm.sample_rate as f32)
}

/// Draws the newest active subtitle at the bottom of the screen and a one-line cutscene status.
/// The node is created on first run; it is plain Bevy text (the item10 script-font path is for
/// the script HUD and is not reused here), labelled [`SubtitleText`].
pub fn draw(
    state: Res<CinematicState>,
    mut commands: Commands,
    mut text: Query<&mut Text, With<SubtitleText>>,
    mut init: Local<bool>,
) {
    if !*init {
        *init = true;
        commands.spawn((
            SubtitleText,
            Text::new(String::new()),
            TextFont {
                font_size: FontSize::Px(20.0),
                ..default()
            },
            TextColor(Color::srgb(1.0, 1.0, 0.9)),
            Node {
                position_type: PositionType::Absolute,
                bottom: px(36),
                left: px(0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            TextLayout::justify(Justify::Center),
        ));
        return;
    }
    let Ok(mut text) = text.single_mut() else {
        return;
    };
    let mut out = String::new();
    if let Some(line) = state.subtitles.last() {
        match &line.speaker {
            Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("none") => {
                out.push_str(s);
                out.push_str(": ");
            }
            _ => {}
        }
        out.push_str(&line.text);
    }
    if let Some(v) = &state.view {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[camera {} ({})]", v.actor, v.source.as_str()));
    }
    if state.input_suppressed {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[cutscene input frozen]");
    }
    text.0 = out;
}

/// Prints the cinematic timeline (view changes and dialogue lines) when the app exits, so an
/// unattended run records what the scripts drove.
pub fn report_exit(state: Res<CinematicState>, mut exiting: MessageReader<AppExit>) {
    if exiting.read().next().is_none() {
        return;
    }
    println!(
        "[cine] exit: {} dialogue line(s), {} view change(s)",
        state.lines, state.view_changes
    );
    if let Some(unresolved) = &state.voice_unresolved {
        println!(
            "[cine] voice durations: {} unresolved name(s)",
            unresolved.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    for line in &state.log {
        println!("[cine]   {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(name: &str, x: f32) -> (String, [f32; 3], [i32; 3]) {
        (name.to_owned(), [x, 0.0, 0.0], [0; 3])
    }

    /// `CameraView` wins over `ViewTarget`; a `ViewTarget` that is the player pawn is ignored.
    #[test]
    fn view_selection_prefers_camera_view_and_skips_the_player_pawn() {
        let cam = pose("Camera60", 100.0);
        let vt = pose("Camera65", 200.0);
        let picked = choose_view(
            true,
            Some(cam.clone()),
            Some((vt.0.clone(), vt.1, vt.2, false)),
        )
        .expect("camera view");
        assert_eq!(picked.source, ViewSource::CameraView);
        assert_eq!(picked.actor, "Camera60");
        assert_eq!(picked.location, [100.0, 0.0, 0.0]);

        // No CameraView: the non-player ViewTarget is used.
        let picked = choose_view(false, Some(cam), Some((vt.0.clone(), vt.1, vt.2, false)))
            .expect("view target");
        assert_eq!(picked.source, ViewSource::ViewTarget);
        assert_eq!(picked.actor, "Camera65");

        // The player pawn as ViewTarget is not a cutscene camera.
        assert!(
            choose_view(
                false,
                None,
                Some(("XIIIPlayerPawn0".into(), [0.0; 3], [0; 3], true))
            )
            .is_none()
        );
        // No camera at all -> first person.
        assert!(choose_view(false, None, None).is_none());
    }

    /// The unreal-to-bevy camera pose keeps the runtime axis policy: +X source is -Z Bevy and a
    /// +90 deg yaw turns the view to +X; positive pitch looks up.
    #[test]
    fn camera_transform_matches_the_axis_policy() {
        let (t, q) = camera_transform([90.0, 0.0, 0.0], [0, 0, 0]);
        assert!((t - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-5, "{t:?}");
        let fwd = q * Vec3::NEG_Z;
        assert!((fwd - Vec3::NEG_Z).length() < 1e-5, "{fwd:?}");
        // Yaw 16384 (90 deg) turns forward (+X source) to Bevy +X.
        let (_, q) = camera_transform([0.0; 3], [0, 16384, 0]);
        let fwd = q * Vec3::NEG_Z;
        assert!((fwd - Vec3::X).length() < 1e-4, "{fwd:?}");
        // Positive pitch raises the view.
        let (_, q) = camera_transform([0.0; 3], [16384, 0, 0]);
        let fwd = q * Vec3::NEG_Z;
        assert!((fwd - Vec3::Y).length() < 1e-4, "{fwd:?}");
    }

    /// `XIII_GOG_DIR` resolved against the workspace root, or `None` in CI.
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

    fn collect_dialogues(session: &mut Session, seen: &mut u64, lines: &mut Vec<String>) -> usize {
        let mut with_text = 0;
        for d in session.new_dialogues(seen) {
            if d.text.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                with_text += 1;
            }
            lines.push(format!(
                "[{:.3}s] speaker={} sound={} text={:?} duration={:?}",
                d.time,
                d.speaker.as_deref().unwrap_or("-"),
                d.sound,
                d.text,
                d.duration
            ));
        }
        with_text
    }

    /// Opt-in corpus diagnostic (requirement 5): step Plage00 for 60 s from map start with the
    /// player held at the PlayerStart and report the dialogue events and the gating script state.
    ///
    /// **Plage00 emits none.** The opening dialogue is driven by `Cine0`'s `Cine2` sequence
    /// (`dial dial_debut` -> `DialogueManager0`, tag `dial_debut`). `Cine2.CineInit` waits for
    /// `MapInfo.EndCartoonEffect` before calling `Play(5)` (the decoded state code); in the retail
    /// engine that flag is set when the opening cartoon presentation finishes, and the only
    /// decoded writer is `XIIIPlayerController.InitInputSystem` for the `mapmenu` URL. The headless
    /// runtime never plays the cartoon and no native sets the flag, so `Cine0` stays in `CineInit`
    /// with `bInitialized = false` and never starts the sequence. If any line *were* emitted, it
    /// must carry resolved text.
    #[test]
    fn opt_in_plage00_start_dialogue_diagnostic() {
        use xiii_script::Value;
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        let mut seen = 0u64;
        let mut lines: Vec<String> = Vec::new();
        let mut with_text = 0usize;
        let mut loc = session.player_location().unwrap_or([0.0; 3]);
        for _ in 0..3600 {
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &crate::play::session::PlayerVMModes::default(),
            );
            with_text += collect_dialogues(&mut session, &mut seen, &mut lines);
            loc = session.player_location().unwrap_or(loc);
        }
        println!(
            "[cine test] Plage00 map start, 60 s no input: {} dialogue(s), {} with text: {lines:?}",
            lines.len(),
            with_text
        );
        let c0 = session.vm().find_object("Cine0").expect("Cine0");
        let vm = session.vm();
        let end_cartoon = match vm.get_property(c0, "MI") {
            Some(Value::Object(Some(xiii_script::ObjRef::Instance(mi)))) => {
                vm.get_property(*mi, "EndCartoonEffect").cloned()
            }
            _ => None,
        };
        println!(
            "[cine test] gate: Cine0 state={:?} bInitialized={:?} EndCartoonEffect={:?}; first error {:?}",
            vm.state_name(c0),
            vm.get_property(c0, "bInitialized"),
            end_cartoon,
            session
                .first_error()
                .map(|e| e.lines().next().unwrap_or(""))
        );
        if lines.is_empty() {
            println!(
                "[cine test] BLOCKED (expected): Cine0 waits in CineInit for \
                 MapInfo.EndCartoonEffect (set by the opening cartoon presentation, not modelled); \
                 the dialogue natives themselves resolve text (see the forced-dialogue test)."
            );
            return;
        }
        assert!(
            with_text >= 1,
            "a dialogue fired but none carried resolved text: {lines:?}"
        );
    }

    /// Opt-in corpus test: `TouchTrigger1` (which carries `Event = 'XIII_approche'`) spawns
    /// inactive because `TouchTrigger.PostBeginPlay` sets `bActif = !bActivableParTrigger` and the
    /// map sets `bActivableParTrigger = true`. Touching it therefore does not start the `Cine0`
    /// sequence (no dialogue) until some other trigger calls its `Trigger`.
    #[test]
    fn opt_in_plage00_inactive_touchtrigger_does_not_start_the_cine() {
        use xiii_script::Value;
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        let tt = session
            .vm()
            .find_object("TouchTrigger1")
            .expect("TouchTrigger1");
        assert_eq!(
            session.vm().get_property(tt, "bActif"),
            Some(&Value::Bool(false)),
            "TouchTrigger1 must spawn inactive (bActivableParTrigger)"
        );
        let mut seen = 0u64;
        let mut lines = Vec::new();
        let loc = [6746.321, -474.648, 830.0];
        for _ in 0..600 {
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &crate::play::session::PlayerVMModes::default(),
            );
            collect_dialogues(&mut session, &mut seen, &mut lines);
        }
        assert_eq!(
            session.vm().get_property(tt, "bActif"),
            Some(&Value::Bool(false)),
            "Touch must not activate an inactive TouchTrigger"
        );
        assert!(
            lines.is_empty(),
            "an inactive TouchTrigger started the cine: {lines:?}"
        );
        println!(
            "[cine test] inactive TouchTrigger1 touched at {:?}; no dialogue ({} touches)",
            session.touches(),
            session.touches().len()
        );
    }

    /// Opt-in corpus test (requirement 5, mechanism): forcing the map's own opening
    /// `DialogueManager0` (tag `dial_debut`) through `StartDialogue` on the real decoded actor
    /// emits a dialogue event whose text resolved from `Speakers[..].Sentences[..]`.
    #[test]
    fn opt_in_plage00_forced_start_dialogue_resolves_text() {
        use xiii_script::Value;
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        let dm = session
            .vm()
            .find_object("DialogueManager0")
            .expect("Plage00 has DialogueManager0");
        session
            .vm_mut()
            .send_event(dm, "StartDialogue", vec![Value::Int(0)])
            .expect("DialogueManager0.StartDialogue");
        let mut seen = 0u64;
        let mut lines = Vec::new();
        let mut with_text = 0usize;
        let mut loc = session.player_location().unwrap_or([0.0; 3]);
        for _ in 0..600 {
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &crate::play::session::PlayerVMModes::default(),
            );
            with_text += collect_dialogues(&mut session, &mut seen, &mut lines);
            loc = session.player_location().unwrap_or(loc);
        }
        println!("[cine test] forced DialogueManager0 lines: {lines:?}");
        println!(
            "[cine test] forced first error: {:?}",
            session
                .first_error()
                .map(|e| e.lines().next().unwrap_or(""))
        );
        assert!(
            with_text >= 1,
            "the forced DialogueManager0 produced no dialogue with resolved text: {lines:?}"
        );
    }
}
