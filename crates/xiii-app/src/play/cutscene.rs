//! item21 hook: in-game cutscene playback for `--play`.
//!
//! When the game's own code opens a cutscene through `Engine.VideoPlayer` — the level-end
//! `XIIIPlayerController.GameEndedSuccess.Timer` -> `PlayingVideo` path (`cine01` on Plage01) —
//! this module's systems decode and play it with the clean-room `xiii-video` decoder:
//! fullscreen (letterboxed) over the scene, with its Bink Audio track. The VM's
//! `VideoPlayer.GetStatus` polls the host playback, so `PlayingVideo.PlayerTick` requests the
//! level travel exactly when the video actually ends (not on a separate timer).
//!
//! Evidence (decoded scripts, disassembled with `xiii-tool script disasm`):
//! * `xiii.XIIIPlayerController.GameEndedSuccess.Timer`: when
//!   `MapInfo.EndMapVideo != ""`, creates `Engine.VideoPlayer` into `self.VP`,
//!   `VP.Open(MapInfo.EndMapVideo)`, then `GotoState('PlayingVideo')`.
//! * `xiii.XIIIPlayerController.PlayingVideo.BeginState`: `KillAllSounds(); VP.Play();
//!   StopAllSounds()`.
//! * `xiii.XIIIPlayerController.PlayingVideo.PlayerTick`: `VP == None` -> travel; else
//!   `switch (VP.GetStatus())`: case 0 -> `Log("End video or no play")` + travel; case 2 ->
//!   `Log("Error playing video")` + travel; case 1 -> keep playing.
//!
//! **Input during playback follows the game's code**: this controller state has no input
//! handler and no skip key — the level-end video always plays to its end (or the game's own
//! error path). The host adds no skip key; movement input is already frozen during
//! `PlayingVideo` by `cinematics::input_suppressed`. (The menu's skip path — Enter/Escape in
//! `XIIIMenu.InternalOnKeyEvent` calling `VideoPlayer.Stop` — is cited in
//! `menu::cutscene`.)

use std::path::Path;

use bevy::prelude::*;

use xiii_script::Vm;

use crate::video::{BinkAudio, VideoHostHandle, VideoOverlay};

pub use crate::video::CutsceneHost;

/// Installs the host playback provider into the session's VM and returns the handle the sync
/// systems drive. `audio_on` follows the `--audio` flag (the Bink track mixes through the same
/// output device as the VM's sound events).
pub fn install(vm: &mut Vm<'static>, game_dir: &Path, audio_on: bool) -> VideoHostHandle {
    let host = VideoHostHandle::new(game_dir.to_path_buf(), audio_on);
    vm.set_video_host(Box::new(host.clone()));
    host
}

/// Per-frame sync (Update, after the HUD draw): advances the cutscene, keeps the letterboxed
/// fullscreen overlay over the scene and tears it down when the game stops the clip.
fn sync(
    host: NonSendMut<CutsceneHost>,
    mut overlay: ResMut<VideoOverlay>,
    mut images: ResMut<Assets<Image>>,
    mut audio_assets: ResMut<Assets<BinkAudio>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut commands: Commands,
) {
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

/// True when the running cutscene has been playing for at least `min_secs` of playback (the
/// unattended screenshot trigger; the video may start long after the wall-clock budget began).
pub fn playing_for(host: &CutsceneHost, min_secs: f64) -> bool {
    host.0
        .as_ref()
        .is_some_and(|h| h.is_playing() && h.time_estimate() >= min_secs)
}

/// Prints the host playback report when the app exits, so an unattended run records what the
/// game's `VideoPlayer` natives played.
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

/// Playback window seconds before an unattended run screenshots the cutscene.
pub const VIDEO_SHOT_AFTER_SECS: f64 = 3.0;

/// Registers the overlay resources and adds the per-frame sync + exit report systems to the
/// `--play` app. The host handle is installed by `PlayPlugin`/`travel` (they own the session
/// lifetime).
pub struct CutsceneSystems;

impl Plugin for CutsceneSystems {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync).add_systems(Last, report_exit);
    }
}
