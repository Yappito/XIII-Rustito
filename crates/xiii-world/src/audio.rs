//! Level-placed audio: ambient-sound emitters and the level's music cue.
//!
//! This is a small, Bevy-free discovery pass over a map's tagged properties, using the same
//! read-only package access as [`crate::import_map`]. It never plays anything: it reports what
//! the map asks for, and the caller (`xiii-app`) resolves each `Sound` path through
//! `xiii-audio` and plays it.
//!
//! ## Ambient emitters (measured)
//!
//! On the GOG Plage maps, ambient sound is a non-null `AmbientSound` object property on ordinary
//! actors — `XIDCine.HelicoDeco.AmbientSound = XIIIsound.Vehicles.PlageHelico0`,
//! `XIDCine.NiceHelico.AmbientSound = XIIIsound.Vehicles.helicoflashloop` (Plage00);
//! `XIII.BoatDeco.BoatDeco1` and `XIDCine.Cine2.Cine2` (Plage01). The dedicated
//! `Engine.AmbientSound` actor class exists in `engine.u` but is not placed on these maps
//! (measured: 0 exports). Emitters are therefore discovered **by the `AmbientSound` property on
//! any actor**, plus the `Engine.AmbientSound` class itself for maps that use it.
//!
//! The roll-off distances come from the actor's inherited class defaults: `Engine.Actor`
//! declares `SaturationDistance = 400`, `StabilisationDistance = 1392`,
//! `StabilisationVolume = -40.0` (measured via resolved class defaults), matching the
//! `CRolloffParam` accessors exported by `HXAudio.dll`. `SoundVolume`/`SoundPitch` do not exist
//! as `Actor` properties in this corpus; the per-call volume/pitch are the `PlaySound`
//! `Param2`/`Param4` arguments instead.
//!
//! ## Music cue (measured)
//!
//! `XIIIGameInfo.AcceptInventory` starts a loaded save's music with
//! `PlayMusic(S.SoundToLaunch)` where `S` is a `XIII.XIIISaveGameTrigger` (disassembled from
//! `xiii.u`). `HXAudio.dll`'s `PlayMusicInit` loads `XIIIsound.Music__<Title>.<...>__hMusicInit`
//! (measured string `XIIIsound.Music__Hualpar2.Hualpar2__hMusicInit`), and every campaign map
//! imports a `<Title>__hMusicInit` / `__hMusicInit2` object from `Music__<Title>`. The cue is
//! therefore, in order: the map's non-null `LevelInfo.InitMusic`, else a `XIIISaveGameTrigger`
//! `SoundToLaunch`, else the `<Title>__hMusicInit` convention derived from `LevelInfo.Title`.

use std::collections::BTreeMap;
use std::path::Path;

use xiii_decode::common::Props;
use xiii_package::{Limits, ObjectRef, Package, PropertyValue};

use crate::ClassDefaults;

/// Where a music cue came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicSource {
    /// `LevelInfo.InitMusic` (the map's explicit music property).
    LevelInitMusic,
    /// A `XIII.XIIISaveGameTrigger`'s `SoundToLaunch` (the save/checkpoint music).
    SaveGameTrigger,
    /// The `<Title>__hMusicInit` convention in the `Music__<Title>` package.
    TitleConvention,
}

impl MusicSource {
    /// Short stable label.
    pub fn as_str(self) -> &'static str {
        match self {
            MusicSource::LevelInitMusic => "level_init_music",
            MusicSource::SaveGameTrigger => "save_game_trigger",
            MusicSource::TitleConvention => "title_convention",
        }
    }
}

/// Distances/volume of one emitter's roll-off, in Unreal units and dB. This mirrors
/// `xiii_audio::Attenuation` but stays `xiii-world`-local so the world crate does not depend on
/// the audio crate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rolloff {
    /// Full-volume distance (UU).
    pub saturation_distance: f32,
    /// Distance at the stabilisation volume (UU).
    pub stabilisation_distance: f32,
    /// Volume floor (dB, negative).
    pub stabilisation_volume_db: f32,
}

impl Default for Rolloff {
    fn default() -> Self {
        Self {
            saturation_distance: 400.0,
            stabilisation_distance: 1392.0,
            stabilisation_volume_db: -40.0,
        }
    }
}

/// One placed ambient-sound emitter.
#[derive(Debug, Clone, PartialEq)]
pub struct AmbientEmitter {
    /// Actor object path in the map (`HelicoDeco1`).
    pub actor: String,
    /// Actor class path (`XIDCine.HelicoDeco`).
    pub class: String,
    /// Decoded `AmbientSound` object path (`Package.Group.Leaf`), `None` when the reference is
    /// null/unresolvable (counted, never silently dropped).
    pub sound: Option<String>,
    /// Location in Unreal units (map property, else class default, else zero).
    pub location: [f32; 3],
    /// Rotation rotator (map property, else zero).
    pub rotation: [i32; 3],
    /// Resolved roll-off (class defaults, else the `Engine.Actor` values).
    pub rolloff: Rolloff,
    /// `bAmbientCreature` (a creature whose calls come from its own mesh), when declared.
    pub ambient_creature: bool,
    /// `Engine.AmbientSound` class marker rather than a property-bearing actor.
    pub is_class_ambient: bool,
}

/// The level's music cue.
#[derive(Debug, Clone, PartialEq)]
pub struct MusicCue {
    /// Decoded `Sound` object path (`XIIIsound.Music__Plage00.Plage00__hMusicInit`).
    pub sound: String,
    /// Which rule produced the cue.
    pub source: MusicSource,
    /// The actor the cue was read from (LevelInfo or the save trigger), for the report.
    pub actor: String,
}

/// Discovery result for one map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevelAudio {
    /// Map title (`LevelInfo.Title`).
    pub title: String,
    /// Placed ambient emitters.
    pub ambients: Vec<AmbientEmitter>,
    /// The level music cue, when one is found.
    pub music: Option<MusicCue>,
    /// Explicit counters for every case (never a silent drop).
    pub counters: BTreeMap<String, usize>,
}

impl LevelAudio {
    /// Counts one named case.
    fn count(&mut self, key: &str, n: usize) {
        *self.counters.entry(key.to_owned()).or_default() += n;
    }

    /// Discovers a map's level audio from an installation, read-only. Opens the code packages
    /// for inherited class defaults and the map itself; a failure to open either is returned.
    pub fn discover(root: &Path, map: &str) -> Result<Self, String> {
        // `PackageCache` reads the map; `ClassDefaults` reads the code packages for inherited
        // defaults. Both are read-only and independent.
        let mut cache = crate::PackageCache::open(root)?;
        let mut defaults = ClassDefaults::open(root)?;
        let map_pkg = cache.map(map)?;
        Ok(Self::discover_from_package(
            &map_pkg.package,
            &map_pkg.data,
            &mut defaults,
        ))
    }

    /// Discovers level audio from a parsed map package and resolved class defaults.
    pub fn discover_from_package(
        package: &Package,
        data: &[u8],
        defaults: &mut ClassDefaults,
    ) -> Self {
        let mut out = LevelAudio::default();
        let exports = package.exports();

        // First pass: the level title from the LevelInfo export (used by the music convention).
        let mut level_info: Option<usize> = None;
        for i in 0..exports.len() {
            let class = package.export_class_path(i).unwrap_or("");
            if class.eq_ignore_ascii_case("Engine.LevelInfo") {
                // The primary LevelInfo is export 0 on the corpus; prefer it, else the first.
                if level_info.is_none() || i == 0 {
                    level_info = Some(i);
                }
            }
        }
        if let Some(i) = level_info
            && let Ok(props) = package.read_object_properties(data, i, &Limits::default())
        {
            let p = Props::new(package, &props);
            if let Some(PropertyValue::Str(t)) = p.get("Title").map(|x| &x.value) {
                out.title = t.clone();
            }
        }
        out.count(
            if out.title.is_empty() {
                "music.title.missing"
            } else {
                "music.title.found"
            },
            1,
        );

        // Second pass: ambient emitters and the music cue.
        let mut save_trigger_cue: Option<MusicCue> = None;
        let mut init_music_cue: Option<MusicCue> = None;
        for i in 0..exports.len() {
            let class = package.export_class_path(i).unwrap_or("").to_owned();
            let short = class.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            let path = package
                .object_path(ObjectRef::Export(i as u32))
                .unwrap_or("?")
                .to_owned();
            let Ok(props) = package.read_object_properties(data, i, &Limits::default()) else {
                continue;
            };
            let p = Props::new(package, &props);

            // AmbientSound object property (any actor).
            let ambient_ref = p.object("AmbientSound");
            if let Some(r) = ambient_ref
                && !r.is_null()
            {
                let sound = package.object_path(r).map(str::to_owned);
                out.count("ambient.actors", 1);
                match &sound {
                    Some(_) => out.count("ambient.sound_resolved_path", 1),
                    None => out.count("ambient.sound_null_path", 1),
                }
                out.ambients.push(Self::make_emitter(
                    &class, &path, sound, &p, defaults, false,
                ));
            } else if short == "ambientsound" {
                // The dedicated class, when placed. Its `AmbientSound` may hold the wave.
                out.count("ambient.class_actors", 1);
                let sound = p
                    .object("AmbientSound")
                    .filter(|r| !r.is_null())
                    .and_then(|r| package.object_path(r))
                    .map(str::to_owned);
                out.ambients
                    .push(Self::make_emitter(&class, &path, sound, &p, defaults, true));
            }

            // Music: LevelInfo.InitMusic and the save trigger's SoundToLaunch.
            if class.eq_ignore_ascii_case("Engine.LevelInfo")
                && init_music_cue.is_none()
                && let Some(r) = p.object("InitMusic").filter(|r| !r.is_null())
                && let Some(sound) = package.object_path(r)
            {
                init_music_cue = Some(MusicCue {
                    sound: sound.to_owned(),
                    source: MusicSource::LevelInitMusic,
                    actor: path.clone(),
                });
                out.count("music.init_music.found", 1);
            }
            if short == "xiiisavegametrigger"
                && save_trigger_cue.is_none()
                && let Some(r) = p.object("SoundToLaunch").filter(|r| !r.is_null())
                && let Some(sound) = package.object_path(r)
            {
                save_trigger_cue = Some(MusicCue {
                    sound: sound.to_owned(),
                    source: MusicSource::SaveGameTrigger,
                    actor: path.clone(),
                });
                out.count("music.save_trigger.found", 1);
            }
        }

        // Preference: explicit LevelInfo.InitMusic, then a save trigger, then the title convention.
        out.music = init_music_cue.or(save_trigger_cue).or_else(|| {
            if out.title.is_empty() {
                out.count("music.cue.unresolved", 1);
                return None;
            }
            out.count("music.title_convention.used", 1);
            Some(MusicCue {
                sound: format!(
                    "XIIIsound.Music__{title}.{title}__hMusicInit",
                    title = out.title
                ),
                source: MusicSource::TitleConvention,
                actor: format!("<{} title convention>", out.title),
            })
        });
        if out.music.is_none() {
            out.count("music.cue.missing", 1);
        }
        out
    }

    /// Builds one emitter record: location/rotation from the map, roll-off from class defaults.
    fn make_emitter(
        class: &str,
        path: &str,
        sound: Option<String>,
        p: &Props<'_>,
        defaults: &mut ClassDefaults,
        is_class_ambient: bool,
    ) -> AmbientEmitter {
        let location = p
            .vector("Location")
            .or_else(|| defaults.vector_default(class, "Location").ok().flatten())
            .unwrap_or([0.0; 3]);
        let rotation = p.rotator("Rotation").unwrap_or([0; 3]);
        let rolloff = rolloff_for(class, defaults);
        AmbientEmitter {
            actor: path.to_owned(),
            class: class.to_owned(),
            sound,
            location,
            rotation,
            rolloff,
            ambient_creature: p.bool("bAmbientCreature").unwrap_or(false),
            is_class_ambient,
        }
    }
}

/// Reads the `SaturationDistance`/`StabilisationDistance`/`StabilisationVolume` class defaults of
/// `class`, falling back to the measured `Engine.Actor` values when absent.
pub fn rolloff_for(class: &str, defaults: &mut ClassDefaults) -> Rolloff {
    let mut r = Rolloff::default();
    if let Ok(Some(v)) = defaults.float_default(class, "SaturationDistance") {
        r.saturation_distance = v;
    }
    if let Ok(Some(v)) = defaults.float_default(class, "StabilisationDistance") {
        r.stabilisation_distance = v;
    }
    if let Ok(Some(v)) = defaults.float_default(class, "StabilisationVolume") {
        r.stabilisation_volume_db = v.min(0.0);
    }
    if r.stabilisation_distance < r.saturation_distance {
        r.stabilisation_distance = r.saturation_distance;
    }
    r
}

#[cfg(test)]
mod local_tests {
    use super::*;

    fn gog_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in (Part B): Plage00/Plage01 ambient emitters resolve (count) and each has a music cue.
    #[test]
    fn local_plage_ambient_and_music_discovery() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for (map, expect_ambients) in [("Plage00", 2usize), ("Plage01", 2)] {
            let audio = LevelAudio::discover(&root, map).expect("discover");
            println!(
                "[level-audio test] {map} title={:?} ambients={} music={:?} counters={:?}",
                audio.title,
                audio.ambients.len(),
                audio.music.as_ref().map(|m| (&m.sound, m.source)),
                audio.counters
            );
            assert_eq!(
                audio.ambients.len(),
                expect_ambients,
                "{map} placed ambient emitter count"
            );
            for a in &audio.ambients {
                assert!(
                    a.sound.is_some(),
                    "{} has a null AmbientSound path",
                    a.actor
                );
            }
            let music = audio.music.as_ref().expect("a music cue");
            assert!(
                music.sound.to_ascii_lowercase().contains("music__"),
                "{map} music cue is not a Music__ sound: {}",
                music.sound
            );
        }
    }
}
