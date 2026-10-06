//! `xiii-tool video ...`: Bink 1 cutscene inspection, frame export and corpus validation.
//!
//! Outputs (decoded PNG frames) contain original game imagery: they are only ever written to
//! paths the caller names, and the helper refuses to write inside the installation. The
//! decoder tables are read at runtime from `system/binkw32.dll` (read-only).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use xiii_video::container::BikFile;
use xiii_video::{AudioTables, BinkTables, Decoder};

pub const USAGE: &str = "\
  xiii-tool video info <file.bik>
      Parse the Bink 1 container and print the header, audio tracks, frame count,
      keyframe positions and per-frame byte sizes.

  xiii-tool video tables --game-dir <install-root>
      Locate and report the fixed decoder tables in the installation's
      system/binkw32.dll (Huffman code lengths, run-fill patterns, DCT scan order
      and dequantisation), with the structural invariant each passed.

  xiii-tool video frames <file.bik> --game-dir <install-root>
          [--start K] [--count N] [--png-out DIR] [--yuv-out FILE]
      Decode frames 0..K+N (frames before K are decoded as references but not
      printed) and print per-frame bit consumption and block-type histogram.
      With --png-out, write each frame K.. as RGBA8 PNG; with --yuv-out, append
      each frame K.. as raw display-size yuv420p. Refuses to write inside the
      installation.

  xiii-tool video validate <install-root> [--limit N] [--all] [--psnr-ref DIR]
          [--audio-ref DIR]
      Read every Video/*.bik under the installation, load the DLL tables once and
      decode frames sequentially (all frames unless --limit N). Prints per-file
      decoded frames, errors, bit consumption, plane slack and decode fps. With
      --psnr-ref, DIR holds black-box oracle frames per file (<stem>.frames: frame
      numbers; <stem>.yuv: those frames as display-size yuv420p) and the sampled
      frames are compared (exact match or PSNR). Exits 1 on any decode error,
      plane overrun, missing reference sample or missing table. Every audio track
      is decoded over the same frame range (a.pkts/a.err columns); with
      --audio-ref, DIR/<stem>.s16 holds the black-box oracle PCM of track 0
      (interleaved s16le) and its SNR is printed. Audio errors fail the run.

  xiii-tool video audio <file.bik> [--game-dir DIR] [--track N] [--frames N]
          [--out FILE.wav] [--ref FILE.s16]
      Decode one Bink Audio track (default 0) to 16-bit PCM using the tables in
      the installation's binkw32.dll (the root is inferred from <root>/Video/).
      --out writes a WAV (refused inside the installation); --ref compares with
      a black-box oracle s16le file (SNR, max difference, differing samples in
      and outside the block cross-fades).

Run with no subcommand for this help.";

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\nUSAGE:\n{USAGE}");
    ExitCode::from(2)
}

/// Entry point for `xiii-tool video <sub> ...`.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("info") => info(&args[1..]),
        Some("tables") => tables(&args[1..]),
        Some("frames") => frames(&args[1..]),
        Some("validate") => validate(&args[1..]),
        Some("audio") => audio(&args[1..]),
        Some(other) => usage_error(&format!("unknown video command '{other}'")),
        None => {
            println!("USAGE:\n{USAGE}");
            ExitCode::SUCCESS
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, ExitCode> {
    std::fs::read(path).map_err(|e| {
        eprintln!("error: cannot read {}: {e}", path.display());
        ExitCode::from(2)
    })
}

fn info(args: &[String]) -> ExitCode {
    let Some(file) = args.iter().find(|a| !a.starts_with("--")) else {
        return usage_error("video info needs a .bik file");
    };
    let data = match read_file(Path::new(file)) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let bik = match BikFile::parse(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {}: {e}", file);
            return ExitCode::from(1);
        }
    };
    let h = &bik.header;
    let mut out = String::new();
    let _ = writeln!(out, "file: {file}");
    let _ = writeln!(
        out,
        "header: revision='{}' size_field={} frames={} frames_dup={} largest_frame={} {}x{} fps={}/{} ({:.3}) flags=0x{:08x} audio_tracks={}",
        h.revision as char,
        h.file_size_field,
        h.frames_a,
        h.frames_b,
        h.largest_frame,
        h.width,
        h.height,
        h.fps_num,
        h.fps_den,
        h.fps(),
        h.flags,
        h.audio_tracks
    );
    let _ = writeln!(out, "flags: alpha={} gray={}", h.has_alpha(), h.has_gray());
    if bik.used_duplicate_frame_count {
        let _ = writeln!(
            out,
            "note: offset-8 frame count {} is inconsistent with the index; using the duplicate field {}",
            h.frames_a, h.frames_b
        );
    }
    let _ = writeln!(out, "audio:");
    for (i, t) in bik.audio.iter().enumerate() {
        let _ = writeln!(
            out,
            "  track {i}: channels={} rate={} flags=0x{:04x} stereo={} dct={} 16bit={} max_decoded_bytes={} id={}",
            t.channels(),
            t.sample_rate,
            t.flags,
            t.stereo(),
            t.audio_dct(),
            t.sixteen_bit(),
            t.max_decoded_bytes,
            t.id
        );
    }
    let _ = writeln!(
        out,
        "frames: {} entries, data starts at byte {} (file {} bytes)",
        bik.frame_count(),
        bik.data_start,
        data.len()
    );
    let key: Vec<usize> = bik
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.keyframe)
        .map(|(i, _)| i)
        .take(20)
        .collect();
    let _ = writeln!(out, "keyframes (first {}): {:?}", key.len(), key);
    let total: u64 = bik.frames.iter().map(|f| f.size).sum();
    let _ = writeln!(
        out,
        "payload bytes: {} total, mean {:.1}",
        total,
        total as f64 / bik.frame_count().max(1) as f64
    );
    print!("{out}");
    ExitCode::SUCCESS
}

fn tables(args: &[String]) -> ExitCode {
    let mut game_dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game-dir" => game_dir = it.next().map(PathBuf::from),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(game_dir) = game_dir else {
        return usage_error("video tables needs --game-dir");
    };
    match BinkTables::from_install(&game_dir) {
        Ok(t) => {
            println!("dll: {} bytes, fnv1a64={:016x}", t.dll_size, t.dll_fnv1a);
            println!("huffman_lengths: row0={:?}", t.huffman_lengths.rows[0]);
            println!(
                "patterns: pattern0[0..8]={:?}, pattern15[0..8]={:?}",
                &t.patterns.patterns[0][..8],
                &t.patterns.patterns[15][..8]
            );
            println!(
                "scan: first 16 = {:?} (is_permutation={})",
                &t.scan[..16],
                is_permutation(&t.scan)
            );
            println!(
                "quant (intra): q0[0]={} q15[0]={} q0[63]={}",
                t.quant.tables[0][0], t.quant.tables[15][0], t.quant.tables[0][63]
            );
            println!(
                "quant (inter): q0[0]={} q15[0]={} q0[63]={}",
                t.quant_inter.tables[0][0],
                t.quant_inter.tables[15][0],
                t.quant_inter.tables[0][63]
            );
            println!(
                "huffman lookup widths: {:?}; rle runs: {:?}",
                t.tree_maxbits, t.rle_runs
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn is_permutation(v: &[u8; 64]) -> bool {
    let mut seen = [false; 64];
    for &x in v {
        if x >= 64 || seen[x as usize] {
            return false;
        }
        seen[x as usize] = true;
    }
    true
}

/// Aggregate results of decoding a frame range.
#[derive(Default)]
struct RangeResult {
    decoded: usize,
    bits: usize,
    error: Option<String>,
    max_slack_bits: usize,
    overruns: usize,
    stale_reads: u64,
    noop_blocks: u64,
    clamped_motion: u64,
    block_types: [u64; 10],
    sub_types: [u64; 10],
    /// (frame, squared-error sum, sample count, exact) per compared sample.
    samples: Vec<(usize, u64, u64, bool)>,
}

/// Oracle reference frames for a file: frame numbers plus their raw yuv420p bytes.
struct Reference {
    frames: Vec<usize>,
    yuv: Vec<u8>,
}

fn load_reference(dir: &Path, stem: &str) -> Option<Reference> {
    let list = std::fs::read_to_string(dir.join(format!("{stem}.frames"))).ok()?;
    let frames: Vec<usize> = list
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    let yuv = std::fs::read(dir.join(format!("{stem}.yuv"))).ok()?;
    Some(Reference { frames, yuv })
}

fn yuv420_bytes(frame: &xiii_video::YuvFrame) -> Vec<u8> {
    let mut v = Vec::new();
    let _ = write_yuv420(&mut v, frame);
    v
}

fn decode_range(
    decoder: &Decoder,
    bik: &BikFile,
    data: &[u8],
    count: usize,
    reference: Option<&Reference>,
) -> RangeResult {
    let mut r = RangeResult::default();
    let mut prev = None;
    let end = count.min(bik.frame_count());
    for i in 0..end {
        let video = match xiii_video::frame_video(data, bik, i) {
            Ok(v) => v,
            Err(e) => {
                r.error = Some(e.to_string());
                return r;
            }
        };
        match decoder.decode_frame(video, &bik.header, prev.as_ref()) {
            Ok((frame, stats)) => {
                r.bits += stats.bits_used;
                r.max_slack_bits = r.max_slack_bits.max(stats.max_plane_slack_bits);
                r.overruns += usize::from(stats.plane_overrun);
                r.stale_reads += stats.stale_reads;
                r.noop_blocks += stats.noop_blocks;
                r.clamped_motion += stats.clamped_motion;
                for k in 0..10 {
                    r.block_types[k] += stats.block_type_counts[k];
                    r.sub_types[k] += stats.sub_type_counts[k];
                }
                if let Some(rf) = reference
                    && let Some(k) = rf.frames.iter().position(|&f| f == i)
                {
                    let ours = yuv420_bytes(&frame);
                    let n = ours.len();
                    match rf.yuv.get(k * n..(k + 1) * n) {
                        Some(theirs) => {
                            let se: u64 = ours
                                .iter()
                                .zip(theirs)
                                .map(|(&a, &b)| {
                                    let d = i64::from(a) - i64::from(b);
                                    (d * d) as u64
                                })
                                .sum();
                            r.samples.push((i, se, n as u64, ours == theirs));
                        }
                        None => {
                            r.error = Some(format!("reference has no sample for frame {i}"));
                            return r;
                        }
                    }
                }
                prev = Some(frame);
                r.decoded += 1;
            }
            Err(e) => {
                r.error = Some(format!("frame {i}: {e}"));
                return r;
            }
        }
    }
    r
}

fn psnr(se: u64, n: u64) -> f64 {
    if se == 0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 * n as f64 / se as f64).log10()
    }
}

fn frames(args: &[String]) -> ExitCode {
    let mut file = None;
    let mut game_dir = None;
    let mut start = 0usize;
    let mut count = 1usize;
    let mut png_out: Option<PathBuf> = None;
    let mut yuv_out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game-dir" => game_dir = it.next().map(PathBuf::from),
            "--start" => {
                start = match it.next().and_then(|v| v.parse().ok()) {
                    Some(v) => v,
                    None => return usage_error("--start needs a number"),
                };
            }
            "--count" => {
                count = match it.next().and_then(|v| v.parse().ok()) {
                    Some(v) => v,
                    None => return usage_error("--count needs a number"),
                };
            }
            "--png-out" => png_out = it.next().map(PathBuf::from),
            "--yuv-out" => yuv_out = it.next().map(PathBuf::from),
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if file.is_none() => file = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let (Some(file), Some(game_dir)) = (file, game_dir) else {
        return usage_error("video frames needs a .bik file and --game-dir");
    };
    let data = match read_file(&file) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let bik = match BikFile::parse(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return ExitCode::from(1);
        }
    };
    let tables = match BinkTables::from_install(&game_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    for out in png_out.iter().chain(yuv_out.iter()) {
        if inside(&out.join("x"), &game_dir) || inside(out, &game_dir) {
            eprintln!(
                "error: refusing to write inside the installation ({})",
                out.display()
            );
            return ExitCode::from(2);
        }
    }
    let mut yuv_file = match &yuv_out {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Some(std::io::BufWriter::new(f)),
            Err(e) => {
                eprintln!("error: cannot create {}: {e}", p.display());
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let decoder = Decoder::new(tables);
    let started = Instant::now();
    let mut prev = None;
    let mut decoded = 0usize;
    let mut reported = 0usize;
    for i in 0..(start + count).min(bik.frame_count()) {
        let video = match xiii_video::frame_video(&data, &bik, i) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        };
        match decoder.decode_frame(video, &bik.header, prev.as_ref()) {
            Ok((frame, _stats)) if i < start => {
                prev = Some(frame);
                decoded += 1;
            }
            Ok((frame, stats)) => {
                println!(
                    "frame {i}: {}x{} bits {}/{} block-types {:?} noop {} stale {} clamped-mv {}",
                    frame.width,
                    frame.height,
                    stats.bits_used,
                    stats.bits_total,
                    stats.block_type_counts,
                    stats.noop_blocks,
                    stats.stale_reads,
                    stats.clamped_motion
                );
                if let Some(f) = yuv_file.as_mut()
                    && let Err(e) = write_yuv420(f, &frame)
                {
                    eprintln!("error: cannot write YUV: {e}");
                    return ExitCode::from(2);
                }
                if let Some(dir) = &png_out {
                    let rgba = frame.to_rgba();
                    let path = dir.join(format!("{}_{i:05}.png", stem(&file)));
                    if let Err(e) = write_png(&path, &rgba, frame.width, frame.height) {
                        eprintln!("error: cannot write {}: {e}", path.display());
                        return ExitCode::from(2);
                    }
                }
                prev = Some(frame);
                decoded += 1;
                reported += 1;
            }
            Err(e) => {
                eprintln!("error: frame {i}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    println!(
        "decoded {decoded} frame(s) ({reported} reported) in {:.3}s ({:.1} fps)",
        started.elapsed().as_secs_f64(),
        decoded as f64 / started.elapsed().as_secs_f64().max(1e-9)
    );
    ExitCode::SUCCESS
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "frame".into())
}

fn inside(out: &Path, root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    lower(&parent).starts_with(&lower(&root))
}

fn find_bik_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("bik")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn validate(args: &[String]) -> ExitCode {
    let mut root = None;
    let mut limit: Option<usize> = None;
    let mut psnr_ref: Option<PathBuf> = None;
    let mut audio_ref: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--psnr-ref" => psnr_ref = it.next().map(PathBuf::from),
            "--audio-ref" => audio_ref = it.next().map(PathBuf::from),
            "--limit" => {
                limit = it.next().and_then(|v| v.parse().ok());
                if limit.is_none() {
                    return usage_error("--limit needs a number");
                }
            }
            "--all" => limit = None,
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("video validate needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    let tables = match BinkTables::from_install(&root) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "tables: binkw32.dll {} bytes fnv1a64={:016x}",
        tables.dll_size, tables.dll_fnv1a
    );
    let decoder = Decoder::new(tables);
    let audio_tables = match AudioTables::from_install(&root) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "audio tables: rle at file offset 0x{:x}, {} critical frequencies, exponent scales 2^-23..2^0",
        audio_tables.rle_offset,
        audio_tables.critical_freqs.len()
    );
    let files = find_bik_files(&root);
    if files.is_empty() {
        eprintln!("error: no .bik files under {}", root.display());
        return ExitCode::from(1);
    }
    let mut failures = 0usize;
    let mut total_frames = 0usize;
    let mut total_errors = 0usize;
    let mut total_audio_packets = 0usize;
    let mut total_audio_errors = 0usize;
    let mut block_types = [0u64; 10];
    let mut sub_types = [0u64; 10];
    println!(
        "{:<8} {:>6} {:>7} {:>11} {:>5} {:>5} {:>5} {:>5} {:>6} {:>7} {:>6} {:>5} {:>6}  per-sample result / note",
        "file",
        "frames",
        "decoded",
        "bits",
        "slack",
        "ovrun",
        "stale",
        "noop",
        "fps",
        "exact",
        "a.pkts",
        "a.err",
        "a.snr",
    );
    for path in &files {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                println!("{:<8} ERROR cannot read: {e}", stem(path));
                failures += 1;
                continue;
            }
        };
        let bik = match BikFile::parse(&data) {
            Ok(b) => b,
            Err(e) => {
                println!("{:<8} ERROR container: {e}", stem(path));
                failures += 1;
                continue;
            }
        };
        let reference = psnr_ref
            .as_deref()
            .and_then(|d| load_reference(d, &stem(path)));
        if psnr_ref.is_some() && reference.is_none() {
            println!(
                "{:<8} ERROR no oracle reference in the --psnr-ref directory",
                stem(path)
            );
            failures += 1;
            continue;
        }
        let count = limit.unwrap_or(bik.frame_count());
        let started = Instant::now();
        let r = decode_range(&decoder, &bik, &data, count, reference.as_ref());
        let secs = started.elapsed().as_secs_f64();
        total_frames += r.decoded;
        for k in 0..10 {
            block_types[k] += r.block_types[k];
            sub_types[k] += r.sub_types[k];
        }
        let mut bad = r.error.is_some() || r.overruns > 0;
        if r.error.is_some() {
            total_errors += 1;
        }
        let mut note = String::new();
        if let Some(rf) = &reference {
            let wanted = rf.frames.iter().filter(|&&f| f < count).count();
            if r.samples.len() != wanted {
                bad = true;
            }
            for (f, se, n, exact) in &r.samples {
                if *exact {
                    let _ = write!(note, "{f}:exact ");
                } else {
                    let _ = write!(note, "{f}:{:.2}dB ", psnr(*se, *n));
                }
            }
        }
        if let Some(e) = &r.error {
            note.push_str(e);
        }
        // Audio: every track over the same frame range; SNR of track 0 against the oracle.
        let mut a_packets = 0usize;
        let mut a_errors = 0usize;
        let mut a_snr = String::from("-");
        for t in 0..bik.audio.len() {
            match xiii_video::audio::decode_track(&data, &bik, t, &audio_tables, Some(count)) {
                Ok(d) => {
                    a_packets += d.packets;
                    a_errors += d.errors;
                    if let Some(e) = &d.first_error {
                        let _ = write!(note, " audio track {t}: {e}");
                    }
                    if t == 0
                        && let Some(dir) = &audio_ref
                    {
                        match read_s16(&dir.join(format!("{}.s16", stem(path)))) {
                            Ok(rf) => {
                                let c = xiii_video::audio::compare_with_oracle(
                                    &rf,
                                    &d.pcm,
                                    d.samples_per_block,
                                    d.overlap,
                                );
                                a_snr = format!("{:.1}", c.snr_db);
                                let _ = write!(
                                    note,
                                    " audio: max|d| {} (outside cross-fades {:.1} dB, max|d| {})",
                                    c.max_diff, c.snr_body_db, c.max_diff_body
                                );
                            }
                            Err(e) => {
                                let _ = write!(note, " audio: no oracle reference ({e})");
                                bad = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    a_errors += 1;
                    let _ = write!(note, " audio track {t}: {e}");
                }
            }
        }
        if a_errors > 0 {
            bad = true;
        }
        total_audio_packets += a_packets;
        total_audio_errors += a_errors;
        if bad {
            failures += 1;
        }
        let exact = format!(
            "{}/{}",
            r.samples.iter().filter(|s| s.3).count(),
            r.samples.len()
        );
        println!(
            "{:<8} {:>6} {:>7} {:>11} {:>5} {:>5} {:>5} {:>5} {:>6.0} {:>7} {:>6} {:>5} {:>6}  {}",
            stem(path),
            bik.frame_count(),
            r.decoded,
            r.bits,
            r.max_slack_bits,
            r.overruns,
            r.stale_reads,
            r.noop_blocks,
            r.decoded as f64 / secs.max(1e-9),
            exact,
            a_packets,
            a_errors,
            a_snr,
            if note.is_empty() { "ok" } else { note.trim() }
        );
        if r.clamped_motion > 0 {
            println!(
                "{:<8} note: {} motion vectors left the plane and were clamped",
                stem(path),
                r.clamped_motion
            );
        }
    }
    println!(
        "summary: {} files, {total_frames} frames decoded, {total_errors} files with errors; \
         {total_audio_packets} audio packets (all tracks), {total_audio_errors} audio errors",
        files.len()
    );
    println!("block types 0..9: {block_types:?}");
    println!("16x16 sub-types 0..9: {sub_types:?}");
    println!(
        "columns: slack = max unread bits in a size-delimited plane (<32 expected); ovrun = \
         frames with a plane read past its size word; stale = bundle reads past decoded data; \
         noop = blocks the shipped decoder skips; exact = sampled frames identical to the \
         oracle / sampled frames; a.pkts/a.err = audio packets decoded / failed over all \
         tracks; a.snr = track 0 SNR in dB against <audio-ref>/<stem>.s16 (common length)"
    );
    if failures == 0 {
        println!("result: OK");
        ExitCode::SUCCESS
    } else {
        println!("result: FAIL ({failures} file(s))");
        ExitCode::from(1)
    }
}

// ------------------------------------------------------------------ Bink Audio

/// Installation root for `<root>/Video/<file>.bik`, else `None`.
fn install_root_of(file: &Path) -> Option<PathBuf> {
    let video_dir = file.parent()?;
    let name = video_dir
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    (name == "video").then(|| video_dir.parent().map(Path::to_path_buf))?
}

/// Reads an oracle reference as interleaved little-endian `i16`.
fn read_s16(path: &Path) -> std::io::Result<Vec<i16>> {
    let b = std::fs::read(path)?;
    Ok(b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect())
}

fn audio(args: &[String]) -> ExitCode {
    let mut file: Option<PathBuf> = None;
    let mut game_dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut reference: Option<PathBuf> = None;
    let mut track = 0usize;
    let mut frames: Option<usize> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game-dir" => game_dir = it.next().map(PathBuf::from),
            "--out" => out = it.next().map(PathBuf::from),
            "--ref" => reference = it.next().map(PathBuf::from),
            "--track" => {
                track = match it.next().and_then(|v| v.parse().ok()) {
                    Some(v) => v,
                    None => return usage_error("--track needs a number"),
                };
            }
            "--frames" => {
                frames = it.next().and_then(|v| v.parse().ok());
                if frames.is_none() {
                    return usage_error("--frames needs a number");
                }
            }
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if file.is_none() => file = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(file) = file else {
        return usage_error("video audio needs a .bik file");
    };
    let Some(game_dir) = game_dir.or_else(|| install_root_of(&file)) else {
        return usage_error("video audio needs --game-dir (or a file inside <root>/Video)");
    };
    if let Some(o) = &out
        && inside(o, &game_dir)
    {
        eprintln!(
            "error: refusing to write inside the installation ({})",
            o.display()
        );
        return ExitCode::from(2);
    }
    let data = match read_file(&file) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let bik = match BikFile::parse(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return ExitCode::from(1);
        }
    };
    let tables = match AudioTables::from_install(&game_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let started = Instant::now();
    let decoded = match xiii_video::audio::decode_track(&data, &bik, track, &tables, frames) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return ExitCode::from(1);
        }
    };
    let secs = started.elapsed().as_secs_f64();
    let ch = usize::from(decoded.channels.max(1));
    let duration = decoded.pcm.len() as f64 / ch as f64 / f64::from(decoded.sample_rate.max(1));
    println!(
        "track {track}: {} Hz x {} ch, {} packets, {} errors, {} samples ({:.3} s) decoded in {:.3}s ({:.0}x realtime)",
        decoded.sample_rate,
        decoded.channels,
        decoded.packets,
        decoded.errors,
        decoded.pcm.len(),
        duration,
        secs,
        duration / secs.max(1e-9)
    );
    let s = &decoded.stats;
    println!(
        "blocks {} clipped {} exponent_out_of_table {} trailing_bytes {}",
        s.blocks, s.clipped, s.exponent_out_of_table, s.trailing_bytes
    );
    if let Some(e) = &decoded.first_error {
        println!("first error: {e}");
    }
    if let Some(r) = &reference {
        match read_s16(r) {
            Ok(rf) => {
                let c = xiii_video::audio::compare_with_oracle(
                    &rf,
                    &decoded.pcm,
                    decoded.samples_per_block,
                    decoded.overlap,
                );
                println!(
                    "oracle {}: ref {} samples, ours {} (diff {}); compared {}: SNR {:.2} dB, max |diff| {}; outside cross-fades SNR {:.2} dB, max |diff| {}; differing samples {} in cross-fades, {} elsewhere",
                    r.display(),
                    rf.len(),
                    decoded.pcm.len(),
                    decoded.pcm.len() as i64 - rf.len() as i64,
                    c.compared,
                    c.snr_db,
                    c.max_diff,
                    c.snr_body_db,
                    c.max_diff_body,
                    c.differing_crossfade,
                    c.differing_body
                );
            }
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", r.display());
                return ExitCode::from(2);
            }
        }
    }
    if let Some(o) = &out {
        if let Some(dir) = o.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let wav = xiii_video::audio::wav_bytes(&decoded.pcm, decoded.sample_rate, decoded.channels);
        if let Err(e) = std::fs::write(o, wav) {
            eprintln!("error: cannot write {}: {e}", o.display());
            return ExitCode::from(2);
        }
        println!("wrote {}", o.display());
    }
    if decoded.errors > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Appends the display area of `frame` as planar yuv420p (chroma `(w+1)/2 x (h+1)/2`).
fn write_yuv420(
    out: &mut impl std::io::Write,
    frame: &xiii_video::YuvFrame,
) -> std::io::Result<()> {
    for y in 0..frame.height {
        let o = y * frame.plane_width;
        out.write_all(&frame.y[o..o + frame.width])?;
    }
    let (cw, ch) = (frame.width.div_ceil(2), frame.height.div_ceil(2));
    for plane in [&frame.u, &frame.v] {
        for y in 0..ch {
            let o = y * frame.chroma_width;
            out.write_all(&plane[o..o + cw])?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ minimal PNG writer

/// Writes an RGBA8 PNG using stored (uncompressed) DEFLATE blocks and the CRC32/Adler32
/// checksums defined by the PNG/zlib specifications.
pub fn write_png(path: &Path, rgba: &[u8], width: usize, height: usize) -> std::io::Result<()> {
    assert_eq!(rgba.len(), width * height * 4);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut raw = Vec::with_capacity(height * (1 + width * 4));
    for y in 0..height {
        raw.push(0u8);
        raw.extend_from_slice(&rgba[y * width * 4..(y + 1) * width * 4]);
    }
    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_in = Vec::with_capacity(4 + data.len());
    crc_in.extend_from_slice(tag);
    crc_in.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0;
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while i < data.len() {
        let n = (data.len() - i).min(65535);
        let last = i + n >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&(!(n as u16)).to_le_bytes());
        out.extend_from_slice(&data[i..i + n]);
        i += n;
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
