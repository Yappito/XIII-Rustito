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
use xiii_video::{BinkTables, Decoder, VideoErrorKind};

pub const USAGE: &str = "\
  xiii-tool video info <file.bik>
      Parse the Bink 1 container and print the header, audio tracks, frame count,
      keyframe positions and per-frame byte sizes.

  xiii-tool video tables --game-dir <install-root>
      Locate and report the fixed decoder tables in the installation's
      system/binkw32.dll (Huffman code lengths, run-fill patterns, DCT scan order
      and dequantisation), with the structural invariant each passed.

  xiii-tool video frames <file.bik> --game-dir <install-root>
          [--start K] [--count N] [--png-out DIR]
      Decode frames K..K+N and print per-frame bit consumption and block-type
      histogram. With --png-out, write each frame as RGBA8 PNG. Refuses to write
      PNGs inside the installation.

  xiii-tool video validate <install-root> [--limit N] [--all]
      Read every Video/*.bik under the installation, load the DLL tables once and
      decode frames sequentially (all frames unless --limit N). Prints per-file
      decoded frames, errors, bit consumption and decode fps. Exits 1 on any
      decode error or missing table.

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
            "  track {i}: channels={} rate={} flags=0x{:04x} stereo={} dct={} id={}",
            t.channels,
            t.sample_rate,
            t.flags,
            t.stereo(),
            t.audio_dct(),
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
                "quant: q0[0]={} q15[0]={} q0[63]={}",
                t.quant.tables[0][0], t.quant.tables[15][0], t.quant.tables[0][63]
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

fn decode_range(
    decoder: &Decoder,
    bik: &BikFile,
    data: &[u8],
    start: usize,
    count: usize,
) -> (usize, Vec<usize>, Option<String>) {
    let mut prev = None;
    let mut decoded = 0usize;
    let mut bits = Vec::new();
    let end = (start + count).min(bik.frame_count());
    for i in start..end {
        let video = match xiii_video::frame_video(data, bik, i) {
            Ok(v) => v,
            Err(e) => return (decoded, bits, Some(e.to_string())),
        };
        match decoder.decode_frame(video, &bik.header, prev.as_ref()) {
            Ok((frame, stats)) => {
                bits.push(stats.bits_used);
                prev = Some(frame);
                decoded += 1;
            }
            Err(e) => return (decoded, bits, Some(format!("frame {i}: {e}"))),
        }
    }
    (decoded, bits, None)
}

fn frames(args: &[String]) -> ExitCode {
    let mut file = None;
    let mut game_dir = None;
    let mut start = 0usize;
    let mut count = 1usize;
    let mut png_out: Option<PathBuf> = None;
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
    if let Some(out) = &png_out
        && inside(out, &game_dir)
    {
        eprintln!(
            "error: refusing to write PNGs inside the installation ({})",
            out.display()
        );
        return ExitCode::from(2);
    }
    let decoder = Decoder::new(tables);
    let started = Instant::now();
    let mut prev = None;
    let mut decoded = 0usize;
    for i in start..(start + count).min(bik.frame_count()) {
        let video = match xiii_video::frame_video(&data, &bik, i) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        };
        match decoder.decode_frame(video, &bik.header, prev.as_ref()) {
            Ok((frame, stats)) => {
                let rgba = frame.to_rgba();
                println!(
                    "frame {i}: {}x{} bits {}/{} block-types {:?}",
                    frame.width,
                    frame.height,
                    stats.bits_used,
                    stats.bits_total,
                    stats.block_type_counts
                );
                if let Some(dir) = &png_out {
                    let path = dir.join(format!("{}_{i:05}.png", stem(&file)));
                    if let Err(e) = write_png(&path, &rgba, frame.width, frame.height) {
                        eprintln!("error: cannot write {}: {e}", path.display());
                        return ExitCode::from(2);
                    }
                }
                prev = Some(frame);
                decoded += 1;
            }
            Err(e) => {
                eprintln!("error: frame {i}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    println!(
        "decoded {decoded} frame(s) in {:.3}s ({:.1} fps)",
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
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
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
    let files = find_bik_files(&root);
    if files.is_empty() {
        eprintln!("error: no .bik files under {}", root.display());
        return ExitCode::from(1);
    }
    let mut failures = 0usize;
    let mut total_frames = 0usize;
    let mut total_errors = 0usize;
    println!(
        "{:<16} {:>7} {:>7} {:>10} {:>9} {:>9}  note",
        "file", "frames", "decoded", "bits", "secs", "fps"
    );
    for path in &files {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                println!("{:<16} ERROR cannot read: {e}", stem(path));
                failures += 1;
                continue;
            }
        };
        let bik = match BikFile::parse(&data) {
            Ok(b) => b,
            Err(e) => {
                println!("{:<16} ERROR container: {e}", stem(path));
                failures += 1;
                continue;
            }
        };
        let count = limit.unwrap_or(bik.frame_count());
        let started = Instant::now();
        let (decoded, bits, err) = decode_range(&decoder, &bik, &data, 0, count);
        let secs = started.elapsed().as_secs_f64();
        let bits_used: usize = bits.iter().sum();
        total_frames += decoded;
        if err.is_some() {
            total_errors += 1;
            failures += 1;
        }
        println!(
            "{:<16} {:>7} {:>7} {:>10} {:>9.3} {:>9.1}  {}",
            stem(path),
            bik.frame_count(),
            decoded,
            bits_used,
            secs,
            decoded as f64 / secs.max(1e-9),
            err.as_deref().unwrap_or("ok")
        );
    }
    println!(
        "summary: {} files, {total_frames} frames decoded, {total_errors} files with errors",
        files.len()
    );
    if failures == 0 {
        println!("result: OK");
        ExitCode::SUCCESS
    } else {
        println!("result: FAIL ({failures} file(s))");
        let _ = VideoErrorKind::Unsupported;
        ExitCode::from(1)
    }
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
