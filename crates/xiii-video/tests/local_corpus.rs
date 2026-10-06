//! Opt-in tests that read a real XIII installation at runtime.
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
