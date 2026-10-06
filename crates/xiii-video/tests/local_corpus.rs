//! Opt-in tests that read a real XIII installation at runtime (video and Bink Audio).
//!
//! Without `XIII_GOG_DIR` (and `XIII_STEAM_DIR`) these print `SKIPPED` and pass. They never write
//! to the installation. Pixel agreement with the FFmpeg black-box oracle is measured by
//! `xiii-tool video validate --psnr-ref` (see `local/reports/item17b-bink-cleanroom.md`); these
//! tests check that decoding succeeds and that every size-delimited plane is consumed exactly
//! (the shipped reader works in 32-bit words, so fewer than 32 bits may remain unread).

use std::path::PathBuf;

fn dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

#[test]
fn gog_tables_load_from_dll() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let tables = xiii_video::BinkTables::from_install(&root).expect("GOG tables load");
    assert_eq!(tables.huffman_lengths.rows[0], [4u8; 16]);
    assert_eq!(tables.huffman_tables.len(), 16);
    assert_eq!(tables.tree_maxbits[0], 4);
    assert_eq!(tables.quant.tables[0][0], 65536);
    println!("GOG tables: dll {} bytes", tables.dll_size);
}

#[test]
fn steam_tables_load_from_dll() {
    let Some(root) = dir("XIII_STEAM_DIR") else {
        println!("SKIPPED: XIII_STEAM_DIR not set");
        return;
    };
    let tables = xiii_video::BinkTables::from_install(&root).expect("Steam tables load");
    assert_eq!(tables.huffman_lengths.rows[0], [4u8; 16]);
    assert_eq!(tables.huffman_tables.len(), 16);
    println!("Steam tables: dll {} bytes", tables.dll_size);
}

#[test]
fn gog_container_parses_all_cutscenes() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let video = root.join("Video");
    let mut files = 0usize;
    for entry in std::fs::read_dir(&video).into_iter().flatten().flatten() {
        let p = entry.path();
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bik")) {
            continue;
        }
        let data = std::fs::read(&p).expect("read bik");
        let bik = xiii_video::container::BikFile::parse(&data).expect("parse bik");
        assert!(bik.frame_count() > 0, "{} has no frames", p.display());
        files += 1;
    }
    assert!(files >= 19, "expected 19 cutscenes, found {files}");
    println!("parsed {files} Bink containers");
}

/// Decodes `count` frames (all when `None`) and checks the per-frame consumption invariants.
fn decode_checked(data: &[u8], decoder: &xiii_video::Decoder, count: Option<usize>, name: &str) {
    let bik = xiii_video::container::BikFile::parse(data).expect("parse bik");
    let n = count.unwrap_or(bik.frame_count()).min(bik.frame_count());
    let mut prev = None;
    for i in 0..n {
        let video = xiii_video::frame_video(data, &bik, i).expect("frame range");
        let (frame, stats) = decoder
            .decode_frame(video, &bik.header, prev.as_ref())
            .unwrap_or_else(|e| panic!("{name} frame {i}: {e}"));
        assert!(!stats.plane_overrun, "{name} frame {i}: plane overrun");
        assert!(
            stats.max_plane_slack_bits < 32,
            "{name} frame {i}: {} unread bits in a plane",
            stats.max_plane_slack_bits
        );
        assert_eq!(stats.stale_reads, 0, "{name} frame {i}: stale bundle reads");
        assert!(!stats.saw_unknown_block, "{name} frame {i}: block type > 9");
        prev = Some(frame);
    }
}

/// The spec's decode check: every frame of `ubi.bik`.
#[test]
fn gog_ubi_decodes_all_frames() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let data = std::fs::read(root.join("Video").join("ubi.bik")).expect("read ubi.bik");
    let tables = xiii_video::BinkTables::from_install(&root).expect("tables");
    let decoder = xiii_video::Decoder::new(tables);
    decode_checked(&data, &decoder, None, "ubi");
}

/// The spec's corpus check: the first 60 frames of every cutscene.
#[test]
fn gog_first_60_frames_of_every_file_decode() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let tables = xiii_video::BinkTables::from_install(&root).expect("tables");
    let decoder = xiii_video::Decoder::new(tables);
    let mut files = 0usize;
    for entry in std::fs::read_dir(root.join("Video"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = entry.path();
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bik")) {
            continue;
        }
        let data = std::fs::read(&p).expect("read bik");
        decode_checked(&data, &decoder, Some(60), &p.display().to_string());
        files += 1;
    }
    assert!(files >= 19, "expected 19 cutscenes, found {files}");
}

/// item23 parity: every Steam cutscene decodes, including the three 2-frame logo stubs
/// (`alien`/`nvidia`/`ubi`) that differ from GOG's full logo videos (measured: 392-byte
/// 2-frame files, all three byte-identical on the Steam install).
#[test]
fn steam_first_60_frames_of_every_file_decode() {
    let Some(root) = dir("XIII_STEAM_DIR") else {
        println!("SKIPPED: XIII_STEAM_DIR not set");
        return;
    };
    let tables = xiii_video::BinkTables::from_install(&root).expect("tables");
    let decoder = xiii_video::Decoder::new(tables);
    let mut files = 0usize;
    for entry in std::fs::read_dir(root.join("Video"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = entry.path();
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bik")) {
            continue;
        }
        let data = std::fs::read(&p).expect("read bik");
        decode_checked(&data, &decoder, Some(60), &p.display().to_string());
        files += 1;
    }
    assert!(files >= 19, "expected 19 cutscenes, found {files}");
    println!("Steam: {files} cutscenes, first 60 frames each decode with 0 errors");
}

// ------------------------------------------------------------------ Bink Audio

/// Structural checks only: the table values themselves stay in the DLL.
#[test]
fn gog_audio_tables_load_from_dll() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let t = xiii_video::AudioTables::from_install(&root).expect("audio tables load");
    assert_eq!(t.critical_freqs[0], 0);
    assert!(t.critical_freqs.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(t.exponent_scale[23], 1.0);
    assert!(t.rle.windows(2).all(|w| w[0] < w[1]));
    println!("GOG audio tables at DLL file offset 0x{:x}", t.rle_offset);
}

#[test]
fn steam_audio_tables_load_from_dll() {
    let Some(root) = dir("XIII_STEAM_DIR") else {
        println!("SKIPPED: XIII_STEAM_DIR not set");
        return;
    };
    let t = xiii_video::AudioTables::from_install(&root).expect("Steam audio tables load");
    assert_eq!(t.critical_freqs[0], 0);
}

/// Decodes every track over `frames` frames (all when `None`) and checks the packet
/// invariants: no error, every declared byte produced, and every payload consumed exactly
/// (the DLL's blocks end on the payload's last dword). Returns (packets, samples of track 0).
fn decode_audio_checked(
    data: &[u8],
    tables: &xiii_video::AudioTables,
    frames: Option<usize>,
    name: &str,
) -> (usize, usize) {
    let bik = xiii_video::container::BikFile::parse(data).expect("parse bik");
    let n = frames.unwrap_or(bik.frame_count()).min(bik.frame_count());
    let mut packets = 0usize;
    let mut track0 = 0usize;
    for t in 0..bik.audio.len() {
        let mut declared = 0usize;
        for f in 0..n {
            if let Some(a) = bik.frame_packets(data, f).expect("packets").audio[t] {
                declared += a.decoded_bytes as usize / 2;
            }
        }
        let d = xiii_video::audio::decode_track(data, &bik, t, tables, Some(n))
            .unwrap_or_else(|e| panic!("{name} track {t}: {e}"));
        assert_eq!(d.errors, 0, "{name} track {t}: {:?}", d.first_error);
        assert_eq!(
            d.pcm.len(),
            declared,
            "{name} track {t}: declared vs decoded"
        );
        assert_eq!(
            d.stats.trailing_bytes, 0,
            "{name} track {t}: unconsumed bytes"
        );
        assert_eq!(d.stats.exponent_out_of_table, 0, "{name} track {t}");
        packets += d.packets;
        if t == 0 {
            track0 = d.pcm.len();
        }
    }
    (packets, track0)
}

/// The spec's audio check: all audio of `ubi.bik`.
#[test]
fn gog_ubi_audio_decodes_completely() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let data = std::fs::read(root.join("Video").join("ubi.bik")).expect("read ubi.bik");
    let tables = xiii_video::AudioTables::from_install(&root).expect("audio tables");
    let (packets, samples) = decode_audio_checked(&data, &tables, None, "ubi");
    let bik = xiii_video::container::BikFile::parse(&data).expect("parse");
    let track = &bik.audio[0];
    let secs = samples as f64 / f64::from(track.channels()) / f64::from(track.sample_rate);
    let video_secs = bik.frame_count() as f64 / bik.header.fps();
    println!("ubi: {packets} packets, {samples} samples = {secs:.3} s (video {video_secs:.3} s)");
    assert!(packets > 0);
    assert!(
        (secs - video_secs).abs() < 0.1,
        "audio {secs:.3} s vs video {video_secs:.3} s"
    );
}

/// The spec's corpus check: the first 10 s (by frame count) of every cutscene, all tracks.
#[test]
fn gog_first_10_seconds_of_every_file_audio_decodes() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let tables = xiii_video::AudioTables::from_install(&root).expect("audio tables");
    let mut files = 0usize;
    let mut with_audio = 0usize;
    for entry in std::fs::read_dir(root.join("Video"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = entry.path();
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bik")) {
            continue;
        }
        let data = std::fs::read(&p).expect("read bik");
        let bik = xiii_video::container::BikFile::parse(&data).expect("parse");
        let frames = (10.0 * bik.header.fps()).ceil() as usize;
        let (packets, _) =
            decode_audio_checked(&data, &tables, Some(frames), &p.display().to_string());
        if !bik.audio.is_empty() {
            assert!(packets > 0, "{} has tracks but no packets", p.display());
            with_audio += 1;
        }
        files += 1;
    }
    assert!(files >= 19, "expected 19 cutscenes, found {files}");
    println!("{files} files, {with_audio} with audio: first 10 s decode with 0 errors");
}

// ------------------------------------------------------------------ item17d track selection

/// item17d: the cutscene audio-track selection the way the game does it.
///
/// The GOG install's `[Engine.Engine] Language=int` matches no dub prefix (ukt/frt/det/est/itt),
/// so the game selects track 0. A five-track file (Cine00.bik) reports 5 tracks; the
/// single-track Cine01.bik reports 1 (measured on this corpus; the task's "Cine01.bik reports 5
/// tracks" premise is not met and is documented in the item17d report).
#[test]
fn gog_audio_track_selection() {
    let Some(root) = dir("XIII_GOG_DIR") else {
        println!("SKIPPED: XIII_GOG_DIR not set");
        return;
    };
    let language = xiii_video::language_from_install(&root)
        .unwrap_or_else(|| panic!("GOG install has no [Engine.Engine] Language= key"));
    println!("GOG [Engine.Engine] Language={language}");
    // The GOG install's `int` matches no dub prefix -> track 0, for any multi-track file.
    assert_eq!(
        xiii_video::select_audio_track_for(&language, 5),
        0,
        "GOG Language={language} should select track 0"
    );

    // A five-track file reports 5 tracks.
    let data = std::fs::read(root.join("Video").join("Cine00.bik")).expect("read Cine00.bik");
    let bik = xiii_video::container::BikFile::parse(&data).expect("parse Cine00.bik");
    assert_eq!(
        bik.audio.len(),
        5,
        "Cine00.bik should report 5 audio tracks"
    );
    println!(
        "Cine00.bik: {} tracks, GOG selects track 0",
        bik.audio.len()
    );

    // Cine01.bik is single-track in this corpus (measured), so it reports 1.
    let data = std::fs::read(root.join("Video").join("Cine01.bik")).expect("read Cine01.bik");
    let bik = xiii_video::container::BikFile::parse(&data).expect("parse Cine01.bik");
    assert_eq!(
        bik.audio.len(),
        1,
        "Cine01.bik reports 1 audio track in the GOG corpus"
    );
    println!("Cine01.bik: {} track, GOG selects track 0", bik.audio.len());
}

/// item23 parity: the Steam install's `[Engine.Engine] Language=int` (read from
/// `System/Default.ini`, capital `System`, with a `XIII.ini` the GOG install lacks) matches no
/// dub prefix, so the `--video` game rule selects track 0, the same as GOG.
#[test]
fn steam_audio_track_selection() {
    let Some(root) = dir("XIII_STEAM_DIR") else {
        println!("SKIPPED: XIII_STEAM_DIR not set");
        return;
    };
    let language = xiii_video::language_from_install(&root)
        .unwrap_or_else(|| panic!("Steam install has no [Engine.Engine] Language= key"));
    println!("Steam [Engine.Engine] Language={language}");
    assert_eq!(language, "int", "Steam Default.ini sets Language=int");
    assert_eq!(
        xiii_video::select_audio_track_for(&language, 5),
        0,
        "Steam Language={language} should select track 0"
    );

    // A five-track file reports 5 tracks; the rule still picks 0 for `int`.
    let data = std::fs::read(root.join("Video").join("Cine00.bik")).expect("read Cine00.bik");
    let bik = xiii_video::container::BikFile::parse(&data).expect("parse Cine00.bik");
    assert_eq!(
        bik.audio.len(),
        5,
        "Cine00.bik should report 5 audio tracks"
    );
    println!(
        "Cine00.bik: {} tracks, Steam selects track 0",
        bik.audio.len()
    );
}
