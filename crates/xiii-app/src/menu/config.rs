//! item16c user INI overlay. Installation defaults are only read.
use std::path::{Path, PathBuf};
use xiii_script::canvas::MenuSettings;

pub fn default_dir() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .map(|p| p.join("xiii-rustito/config"))
        .ok_or_else(|| "No user configuration directory; supply --config-dir".into())
}

/// Resolve existing ancestors too, so an alias/junction into the installation is rejected.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return path.canonicalize().map_err(|e| e.to_string());
    }
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let parent = absolute
        .parent()
        .ok_or("configuration path has no parent")?;
    let name = absolute
        .file_name()
        .ok_or("configuration path has no filename")?;
    Ok(resolved(parent)?.join(name))
}

pub fn user_path(game: &Path, dir: &Path) -> Result<PathBuf, String> {
    let install = resolved(game)?;
    let file = resolved(&dir.join("menu.ini"))?;
    if file.starts_with(&install) {
        return Err("--config-dir must be outside the read-only installation".into());
    }
    Ok(file)
}

pub fn parse(text: &str, settings: &mut MenuSettings) -> Result<(), String> {
    let mut section = String::new();
    for (i, raw) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with([';', '#']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_owned();
        } else if let Some((key, value)) = line.split_once('=') {
            if section.is_empty() || key.trim().is_empty() {
                return Err(format!("INI line {}: empty section/key", i + 1));
            }
            settings.set(&section, key.trim(), value.trim().to_owned());
        } else {
            return Err(format!("INI line {}: expected section or key=value", i + 1));
        }
    }
    Ok(())
}

pub fn encode(settings: &MenuSettings) -> String {
    let mut out = String::from("; XIII runtime user settings\n");
    let mut previous = "";
    for ((section, key), value) in &settings.values {
        if previous != section {
            out.push_str(&format!("\n[{section}]\n"));
            previous = section;
        }
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

pub fn load(game: &Path, user: &Path) -> Result<MenuSettings, String> {
    let mut settings = MenuSettings::default();
    for path in [
        game.join("system/Default.ini"),
        game.join("system/DefUser.ini"),
    ] {
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Retail INIs use the installation's legacy single-byte encoding. The engine keys and
        // commands needed here are ASCII; preserve non-ASCII bytes as Latin-1 code points.
        parse(
            &bytes.iter().map(|b| char::from(*b)).collect::<String>(),
            &mut settings,
        )?;
    }
    if user.exists() {
        parse(
            &std::fs::read_to_string(user).map_err(|e| e.to_string())?,
            &mut settings,
        )?;
    }
    // Installed class defaults measured from XIIIMenuVideoClientWindow and the PC controller;
    // user values parsed above retain precedence.
    for (section, key, value) in [
        ("XIIIMenuVideoClientWindow", "UserBrightness", "0.5"),
        ("XIIIMenuVideoClientWindow", "UserGamma", "1"),
        ("XIIIMenuVideoClientWindow", "UserContrast", "0.5"),
        ("XIIIMenuVideoClientWindow", "DecalX", "0"),
        ("XIIIMenuVideoClientWindow", "DecalY", "0"),
        (
            "XIIIPlayerController",
            "ConfigType",
            "CT_StrafeLookSameAxis",
        ),
    ] {
        if settings.get(section, key).is_none() {
            settings.set(section, key, value.to_owned());
        }
    }
    settings.master_db = settings
        .get("HXAudio.HXAudioSubsystem", "MasterVolume")
        .ok_or("missing MasterVolume")?
        .parse()
        .map_err(|_| "invalid MasterVolume")?;
    settings.music = settings
        .get("HXAudio.HXAudioSubsystem", "MusicSliderPos")
        .ok_or("missing MusicSliderPos")?
        .parse()
        .map_err(|_| "invalid MusicSliderPos")?;
    if !settings.master_db.is_finite()
        || !(-100.0..=0.0).contains(&settings.master_db)
        || !(0..=2).contains(&settings.music)
    {
        return Err("invalid audio configuration range".into());
    }
    settings.resolutions = vec![
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1600, 900),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];
    Ok(settings)
}

pub fn save(path: &Path, settings: &mut MenuSettings) -> Result<(), String> {
    let parent = path.parent().ok_or("configuration file has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("ini.tmp");
    std::fs::write(&tmp, encode(settings)).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    settings.save_requested = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ini_round_trip_preserves_commands_and_case_insensitive_override() {
        let mut s = MenuSettings::default();
        parse("[Engine.Input]\nW=Axis aBaseY Speed=+1 | Other\n[HXAUDIO.HXAudioSubsystem]\nMasterVolume=-12\nmastervolume=-8\n", &mut s).unwrap();
        let mut t = MenuSettings::default();
        parse(&encode(&s), &mut t).unwrap();
        assert_eq!(s.values, t.values);
        assert_eq!(t.get("input", "w"), Some("Axis aBaseY Speed=+1 | Other"));
        assert_eq!(
            t.get("hxaudio.hxaudiosubsystem", "MasterVolume"),
            Some("-8")
        );
        assert!(parse("key=value", &mut t).is_err());
        assert!(parse("[x]\n=bad", &mut t).is_err());
        assert!(parse("[unterminated", &mut t).is_err());
    }

    #[test]
    fn save_is_atomic_and_user_override_is_visible_after_restart() {
        let dir =
            std::env::temp_dir().join(format!("xiii-menu-config-test-{}", std::process::id()));
        let path = dir.join("menu.ini");
        let _ = std::fs::remove_dir_all(&dir);
        let mut first = MenuSettings::default();
        first.set("HXAudio.HXAudioSubsystem", "MasterVolume", "-17".into());
        first.set("HXAudio.HXAudioSubsystem", "MusicSliderPos", "0".into());
        first.save_requested = true;
        save(&path, &mut first).unwrap();
        assert!(!first.save_requested);

        let text = std::fs::read_to_string(&path).unwrap();
        let mut restarted = MenuSettings::default();
        parse(&text, &mut restarted).unwrap();
        assert_eq!(
            restarted.get("HXAudio.HXAudioSubsystem", "MasterVolume"),
            Some("-17")
        );
        assert_eq!(
            restarted.get("HXAudio.HXAudioSubsystem", "MusicSliderPos"),
            Some("0")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn rejects_installation_and_parent_alias() {
        let root = std::env::current_dir().unwrap();
        assert!(user_path(&root, &root.join("new/config")).is_err());
        assert!(user_path(&root, &root.join("new/../config")).is_err());
    }
}
