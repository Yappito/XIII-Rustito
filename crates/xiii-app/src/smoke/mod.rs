//! M0 smoke test: native window, 3D PBR rendering, UI text, input, audio.
//!
//! Uses no game data. Everything here is a visible diagnostic; none of it is
//! part of a playable-content claim.

mod audio;
mod camera;
mod overlay;
mod report;
mod scene;

use bevy::diagnostic::FrameTimeDiagnosticsPlugin;
use bevy::prelude::*;

use crate::cli::Options;

pub struct SmokePlugin {
    pub options: Options,
}

impl Plugin for SmokePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin::default())
            .insert_resource(report::RunConfig::from_options(&self.options))
            .add_plugins((
                scene::ScenePlugin,
                camera::FlyCameraPlugin,
                audio::TonePlugin,
                overlay::OverlayPlugin,
                report::ReportPlugin,
            ))
            .add_systems(Update, quit_on_escape);
    }
}

fn quit_on_escape(keys: Res<ButtonInput<KeyCode>>, mut exit: MessageWriter<AppExit>) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.write(AppExit::Success);
    }
}
