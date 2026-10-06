//! Cutscene audio-track selection, the way the retail game does it.
//!
//! `UPCVideoPlayerDevice::Open` (`WinDrv.dll` `0x111027f0`) calls `UObject::GetLanguage()`
//! (the installation's `[Engine.Engine] Language=` value) and compares its first three
//! characters, case-insensitively, against five fixed prefixes with `appStrnicmp`
//! (`WinDrv.dll` `0x1110289d..0x11102915`). The match is handed to
//! `BinkSetSoundTrack(1, &n)` (`0x11102955`) before the file is (re)opened, which makes
//! `BinkOpen` play the matching audio track. The five prefixes and their track indices,
//! read from the immediate operands at `0x1110fba8..0x1110fb98`:
//!
//! | prefix | track | language |
//! |--------|-------|----------|
//! | `ukt`  | 0     | English (UK) |
//! | `frt`  | 1     | French |
//! | `det`  | 2     | German |
//! | `itt`  | 3     | Italian |
//! | `est`  | 4     | Spanish |
//!
//! Any other value (including the GOG install's `Language=int`) selects track 0. When the
//! file carries at most one audio track the game forces the index to 0 regardless
//! (`cmpl $1, 0xf0(%eax); jg; mov $0, ...` at `0x11102936..0x1110293f`).

use std::path::Path;

/// The cutscene audio track the retail game selects for `language`.
///
/// `language` is the installation's `[Engine.Engine] Language=` value (typically already
/// lowercased by the caller, but the comparison is case-insensitive either way).
pub fn select_audio_track(language: &str) -> usize {
    let prefix = language.get(..3).map(str::to_ascii_lowercase);
    match prefix.as_deref() {
        Some("ukt") => 0,
        Some("frt") => 1,
        Some("det") => 2,
        Some("itt") => 3,
        Some("est") => 4,
        _ => 0,
    }
}

/// The track the game plays for `language` given a file with `track_count` audio tracks:
/// the [`select_audio_track`] index, forced to 0 when the file has at most one track.
pub fn select_audio_track_for(language: &str, track_count: usize) -> usize {
    if track_count <= 1 {
        return 0;
    }
    select_audio_track(language).min(track_count - 1)
}

/// Reads `[Engine.Engine] Language=` from the installation's `Default.ini` (then `XIII.ini`),
/// lowercased and trimmed. Returns `None` when neither file carries the key. The search is
/// case-insensitive in both the directory name (`System`/`system`) and the section/key.
pub fn language_from_install(root: &Path) -> Option<String> {
    for name in [
        "System/Default.ini",
        "system/Default.ini",
        "System/XIII.ini",
        "system/XIII.ini",
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        if let Some(v) = ini_language(&text) {
            return Some(v);
        }
    }
    None
}

/// Extracts `[Engine.Engine] Language=` from INI `text` (lowercased, trimmed).
fn ini_language(text: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = inner.trim().eq_ignore_ascii_case("Engine.Engine");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("Language")
        {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_ascii_lowercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_map_to_their_tracks() {
        assert_eq!(select_audio_track("ukt"), 0);
        assert_eq!(select_audio_track("frt"), 1);
        assert_eq!(select_audio_track("det"), 2);
        assert_eq!(select_audio_track("itt"), 3);
        assert_eq!(select_audio_track("est"), 4);
    }

    #[test]
    fn comparison_is_case_insensitive_and_uses_the_first_three_chars() {
        assert_eq!(select_audio_track("UKT"), 0);
        assert_eq!(select_audio_track("Frt"), 1);
        assert_eq!(select_audio_track("dEt"), 2);
        // Longer values are matched on the first three characters only.
        assert_eq!(select_audio_track("ittaliano"), 3);
        assert_eq!(select_audio_track("estrell"), 4);
    }

    #[test]
    fn unknown_and_empty_languages_select_track_zero() {
        assert_eq!(select_audio_track("int"), 0);
        assert_eq!(select_audio_track("english"), 0);
        assert_eq!(select_audio_track(""), 0);
        assert_eq!(select_audio_track("int"), 0);
    }

    #[test]
    fn single_track_files_force_track_zero() {
        // The game forces index 0 when the file has at most one audio track, even when the
        // language would otherwise pick a higher dub.
        assert_eq!(select_audio_track_for("itt", 0), 0);
        assert_eq!(select_audio_track_for("itt", 1), 0);
        assert_eq!(select_audio_track_for("itt", 5), 3);
        assert_eq!(select_audio_track_for("int", 5), 0);
        // A language index beyond the file's track count is clamped to the last track.
        assert_eq!(select_audio_track_for("est", 2), 1);
    }

    #[test]
    fn ini_language_reads_the_engine_engine_section() {
        let ini = "[URL]\nLanguage=ignored\n[Engine.Engine]\nRenderDevice=x\nLanguage=FrT\n\
                   [Other]\nLanguage=also-ignored\n";
        assert_eq!(ini_language(ini).as_deref(), Some("frt"));
    }

    #[test]
    fn ini_language_is_case_insensitive_and_skips_comments() {
        let ini = "[engine.engine]\n; Language=commented\n  lAnGuAgE =  DET  \n";
        assert_eq!(ini_language(ini).as_deref(), Some("det"));
    }

    #[test]
    fn ini_language_returns_none_without_the_key() {
        assert_eq!(ini_language("[Engine.Engine]\nRenderDevice=x\n"), None);
        assert_eq!(ini_language("[Other]\nLanguage=int\n"), None);
        assert_eq!(ini_language(""), None);
    }
}
