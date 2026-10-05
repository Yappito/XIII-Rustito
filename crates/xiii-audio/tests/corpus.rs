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
