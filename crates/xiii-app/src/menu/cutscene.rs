//! item21 hook: the new-game cutscene (`cine00`) for `--menu`.
//!
//! Evidence (decoded scripts, disassembled with `xiii-tool script disasm`):
//! * `xidinterf.XIIIMenu.InternalOnClick` (New game): creates `Engine.VideoPlayer` into
//!   `self.VP`, sets `bPlayingVideo = true`, `VP.Open(self.sVideo)` (class default `cine00`,
//!   measured with `xiii-tool script defaults xidinterf.u XIIIMenu --name sVideo`),
//!   `VP.Play()`, `StopAllSounds()`, `GotoState('PlayingVideo')`.
//! * `xidinterf.XIIIMenu.PlayingVideo.Tick`: `VP != None && VP.GetStatus() == 0` ->
//!   `EndOfVideo()` (`CloseAll`, `ClientTravel("Plage00")` — the host travel step).
//! * `xidinterf.XIIIMenu.InternalOnKeyEvent` (with `State == 1` and `self.VP != None`):
//!   **Enter (13) or Escape (27) -> `VP.Stop()` then `EndOfVideo()`** — the game's own skip
//!   path, reachable through live keyboard input or `--menu-script key enter|escape`. The host
//!   forwards the menu's recognized key codes to this handler and invents no skip mapping.
//!
//! The overlay is the same fullscreen letterboxed player the `--play` mode uses; `GetStatus`
//! follows the host playback, so the menu travels exactly when the video actually ends.

use bevy::prelude::*;

use crate::video::{BinkAudio, CutsceneHost, VideoOverlay};

/// Per-frame sync (Update, after the menu draw): advances the cutscene, keeps the letterboxed
/// fullscreen overlay above the menu layer and tears it down on the game's `VideoPlayer.Stop`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync(
    host: NonSendMut<CutsceneHost>,
    mut menu_session: NonSendMut<Result<super::MenuSession, String>>,
    mut overlay: ResMut<VideoOverlay>,
    mut images: ResMut<Assets<Image>>,
    mut audio_assets: ResMut<Assets<BinkAudio>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
) {
    // Forward real menu key presses through the game's own handler while a VideoPlayer is open.
    // InternalOnKeyEvent itself decides whether a given key is a valid skip (Enter/Escape).
    let pressed = [
        (KeyCode::ArrowLeft, 37),
        (KeyCode::ArrowUp, 38),
        (KeyCode::ArrowRight, 39),
        (KeyCode::ArrowDown, 40),
        (KeyCode::Enter, 13),
        (KeyCode::Escape, 27),
        (KeyCode::Space, 32),
        (KeyCode::Backspace, 8),
    ];
    if let Ok(session) = menu_session.as_mut() {
        for (key, code) in pressed {
            if keys.just_pressed(key) {
                session.video_key_event(code);
            }
        }
    }
    let Some(host) = host.0.as_ref() else {
        return;
    };
    let window = windows
        .single()
        .map(|w| [w.width(), w.height()])
        .unwrap_or([1280.0, 720.0]);
    host.sync_overlay(
        &mut overlay,
        &mut images,
        &mut audio_assets,
        &mut commands,
        window,
    );
}

/// Prints the host playback report when the menu app exits.
pub fn report_exit(host: NonSend<CutsceneHost>, mut exiting: MessageReader<AppExit>) {
    if exiting.read().next().is_none() {
        return;
    }
    let Some(host) = host.0.as_ref() else {
        return;
    };
    println!("{}", host.report_line());
    for d in host.diagnostics() {
        println!("[video host]   {d}");
    }
}

/// Registers the sync + exit-report systems for the `--menu` app. The host handle itself is
/// installed by `MenuPlugin` (it owns the session).
pub struct CutsceneSystems;

impl Plugin for CutsceneSystems {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync).add_systems(Last, report_exit);
    }
}
