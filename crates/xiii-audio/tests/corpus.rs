//! Opt-in integration test over a local GOG/Steam installation.
//!
//! Prints `SKIPPED` and passes when `XIII_GOG_DIR` is not set. It parses every `.hxc` bank and
//! decodes one in-bank PCM entry and one streamed ADPCM entry to exercise both codec paths.
//! No game bytes are written or committed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use xiii_audio::hx::{Codec, DataLocation, HxLimits};
use xiii_audio::{PcmAudio, decode_entry, hx};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("hxc"))
        {
            out.push(path);
        }
    }
}

fn find_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

/// Extracts `Plage00.hsc` from a stored resource name like `.\\Plage00.hsc`.
fn resource_file(resource: &str) -> String {
    resource
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(resource)
        .to_owned()
}

#[test]
fn local_hx_banks_parse_and_decode() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let root = PathBuf::from(root);
    let mut banks = Vec::new();
    collect(&root, &mut banks);
    assert!(
        !banks.is_empty(),
        "no .hxc banks found under {}",
        root.display()
    );

    let limits = HxLimits::default();
    let mut parse_failures: Vec<String> = Vec::new();
    let mut codecs: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unparsed_bytes = 0usize;
    let mut decoded_pcm: Option<(String, PcmAudio)> = None;
    let mut decoded_adpcm: Option<(String, PcmAudio)> = None;

    for path in &banks {
        let bytes = std::fs::read(path).expect("read bank");
        let bank = match hx::parse_bank(&bytes, &limits) {
            Ok(b) => b,
            Err(e) => {
                parse_failures.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        unparsed_bytes += bank.unparsed.iter().map(|s| s.len()).sum::<usize>();

        // Read the companion stream at most once.
        let mut stream: Option<Vec<u8>> = None;
        for entry in &bank.entries {
            let Some(wave) = entry.as_wave() else {
                continue;
            };
            *codecs.entry(wave.codec.as_str()).or_default() += 1;
            match wave.codec {
                Codec::Pcm if decoded_pcm.is_none() => {
                    if let Ok(audio) = decode_entry(&bank, entry.index, &bytes, None) {
                        decoded_pcm = Some((format!("{}#{}", path.display(), entry.index), audio));
                    }
                }
                Codec::UbiAdpcm if decoded_adpcm.is_none() => {
                    let DataLocation::External { .. } = &wave.data else {
                        continue;
                    };
                    let Some(resource) = &wave.resource_name else {
                        continue;
                    };
                    if stream.is_none() {
                        let name = resource_file(resource);
                        let dir = path.parent().expect("bank dir");
                        let Some(stream_path) = find_case_insensitive(dir, &name) else {
                            continue;
                        };
                        stream = std::fs::read(&stream_path).ok();
                    }
                    if let Some(stream) = stream.as_deref()
                        && let Ok(audio) = decode_entry(&bank, entry.index, &bytes, Some(stream))
                    {
                        decoded_adpcm =
                            Some((format!("{}#{}", path.display(), entry.index), audio));
                    }
                }
                _ => {}
            }
        }
    }

    assert!(
        parse_failures.is_empty(),
        "parse failures: {parse_failures:#?}"
    );
    assert_eq!(unparsed_bytes, 0, "banks have unparsed bytes");

    let (pcm_name, pcm) = decoded_pcm.expect("decode at least one in-bank PCM entry");
    assert!(pcm.frames() > 0, "{pcm_name} decoded zero frames");
    assert!(pcm.samples.len().is_multiple_of(pcm.channels as usize));

    let (adpcm_name, adpcm) = decoded_adpcm.expect("decode at least one streamed ADPCM entry");
    assert!(adpcm.frames() > 0, "{adpcm_name} decoded zero frames");

    println!(
        "parsed {} banks; codecs {:?}; decoded PCM {} ({} samples, {} ch, {} Hz), \
         ADPCM {} ({} samples, {} ch, {} Hz)",
        banks.len(),
        codecs,
        pcm_name,
        pcm.samples.len(),
        pcm.channels,
        pcm.sample_rate,
        adpcm_name,
        adpcm.samples.len(),
        adpcm.channels,
        adpcm.sample_rate,
    );
}

/// Opt-in: the name-resolution library resolves the item6 trace sounds (weapon, footstep,
/// dialogue) and the Plage00 `TriggerSound0` property to decoded PCM.
#[test]
fn local_sound_library_resolves_item6_traces() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let root = PathBuf::from(root);
    let mut lib = xiii_audio::SoundLibrary::scan(&root);
    let stats = lib.stats();
    assert!(
        stats.banks_parsed > 0,
        "no HX banks parsed under the install"
    );
    assert_eq!(stats.banks_failed, 0, "some banks failed to parse");

    // Trace 1 (weapon), 2 (footstep), 3 (dialogue), and a real Plage00 event target.
    for (path, expect_named) in [
        ("XIIIsound.Guns.M16Fire1", "M16Fire1"),
        ("XIIIsound.Footsteps.FtSkMar1", "FtSkMar1"),
        ("Plage00Voices.Plage00_XIIIa_00", "Plage00_XIIIa_00"),
        ("XIIIsound.Interface.EndBig", "EndBig"),
    ] {
        let r = lib
            .resolve_path(path)
            .unwrap_or_else(|| panic!("{path} did not resolve to a bank entry"));
        let leaf = xiii_audio::library::leaf(path).unwrap();
        assert!(leaf.eq_ignore_ascii_case(expect_named));
        let audio = lib
            .load(&r.entry)
            .unwrap_or_else(|e| panic!("{path}: decode failed: {e:?}"));
        assert!(audio.frames() > 0, "{path} decoded zero frames");
        println!(
            "resolved {path} -> {}#{} {} {} ch {} Hz {} frames (rule={}, candidates={})",
            r.entry.bank.display(),
            r.entry.entry,
            r.entry.codec.as_str(),
            r.entry.channels,
            r.entry.sample_rate,
            audio.frames(),
            r.rule.as_str(),
            r.candidates
        );
    }

    // An unknown name is counted, not silently ignored.
    assert!(lib.resolve_name("definitely_not_a_sound").is_none());
}

/// Opt-in item6d: the Plage00 music cue and the two placed ambient emitters (from the map's
/// `AmbientSound` properties) resolve to HX entries, and a streamed entry decoded in chunks equals
/// a whole-file decode (the streaming invariant on real `.hsc` data).
#[test]
fn local_plage00_music_and_ambients_resolve_and_stream() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let root = PathBuf::from(root);
    let mut lib = xiii_audio::SoundLibrary::scan(&root);

    // Music cue (title convention) and the two ambient emitters measured on Plage00.
    for path in [
        "XIIIsound.Music__plage00.plage00__hMusicInit",
        "XIIIsound.Vehicles.PlageHelico0",
        "XIIIsound.Vehicles.helicoflashloop",
    ] {
        let r = lib
            .resolve_path(path)
            .unwrap_or_else(|| panic!("{path} did not resolve"));
        let stream = lib
            .open_stream(&r.entry)
            .unwrap_or_else(|e| panic!("{path}: stream failed: {e:?}"));
        assert!(stream.total_frames() > 0, "{path} declared zero frames");
        println!(
            "stream {path} -> {}#{} {} {} ch {} Hz {} frames",
            r.entry.bank.display(),
            r.entry.entry,
            r.entry.codec.as_str(),
            stream.channels(),
            stream.sample_rate(),
            stream.total_frames()
        );
        // Chunked decode must equal a whole decode for a real streamed entry.
        let full = lib
            .load(&r.entry)
            .unwrap_or_else(|e| panic!("{path}: full decode: {e:?}"));
        let stream = lib.open_stream(&r.entry).expect("reopen");
        let chunked = xiii_audio::decode_stream(stream, 4096).expect("chunked");
        assert_eq!(
            chunked, *full,
            "{path}: chunked decode differs from the full decode"
        );
    }
}

/// Opt-in item6c: the event-shaped `Guns__9mmSelWp.9mmSelWp__h9mmSelWp` Sound resolves through
/// its native-tail HX resource reference to decoded PCM (the item6 name rule does not match it).
#[test]
fn local_9mmselwp_resource_reference_resolves() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let root = PathBuf::from(root);
    let mut lib = xiii_audio::SoundLibrary::scan(&root);

    let path = "XIIIsound.Guns__9mmSelWp.9mmSelWp__h9mmSelWp";
    // The name fallback must fail for this event-shaped path (why item6b reported no match).
    assert!(
        lib.resolve_by_name(path).is_none(),
        "the event-shaped path has no leaf name match (name rule cannot resolve it)"
    );
    let r = lib
        .resolve_path(path)
        .unwrap_or_else(|| panic!("{path} did not resolve via its resource reference"));
    assert_eq!(
        r.rule,
        xiii_audio::ResolutionRule::ResourceRef,
        "resolved through the Sound native-tail resource reference"
    );
    let audio = lib
        .load(&r.entry)
        .unwrap_or_else(|e| panic!("{path}: decode failed: {e:?}"));
    assert!(audio.frames() > 0, "{path} decoded zero frames");
    println!(
        "resolved {path} -> {}#{} {} {} ch {} Hz {} frames (rule={}, candidates={}, chosen={}, seed={:#x})",
        r.entry.bank.display(),
        r.entry.entry,
        r.entry.codec.as_str(),
        r.entry.channels,
        r.entry.sample_rate,
        audio.frames(),
        r.rule.as_str(),
        r.candidates,
        r.chosen,
        r.seed
    );
}
