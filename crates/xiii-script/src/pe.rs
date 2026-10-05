//! Minimal, bounded PE (Portable Executable) export-name reader.
//!
//! Used only to list exported symbol names of the game's native DLLs (metadata) so that
//! `native` script functions can be cross-referenced with `?execName@AClass@@...` symbols.
//! Reads the DOS header, PE signature, COFF header, the export data directory and the section
//! table; no code is examined. Layout per the Microsoft PE/COFF specification.

/// Errors are plain strings: this is a diagnostic helper, not part of the decoding chain.
pub type PeResult<T> = std::result::Result<T, String>;

fn u16_at(d: &[u8], o: usize) -> PeResult<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("u16 at {o} out of bounds"))
}

fn u32_at(d: &[u8], o: usize) -> PeResult<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("u32 at {o} out of bounds"))
}

/// Exported symbol names of a PE32/PE32+ image, in table order.
pub fn export_names(d: &[u8]) -> PeResult<Vec<String>> {
    if d.get(0..2) != Some(b"MZ") {
        return Err("missing MZ header".into());
    }
    let pe = u32_at(d, 0x3C)? as usize;
    if d.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("missing PE signature".into());
    }
    let coff = pe + 4;
    let sections = usize::from(u16_at(d, coff + 2)?);
    let opt_size = usize::from(u16_at(d, coff + 16)?);
    let opt = coff + 20;
    let magic = u16_at(d, opt)?;
    let dir0 = match magic {
        0x10B => opt + 96,
        0x20B => opt + 112,
        m => return Err(format!("unknown optional header magic 0x{m:X}")),
    };
    if dir0 + 8 > opt + opt_size {
        return Err("no export data directory".into());
    }
    let export_rva = u32_at(d, dir0)?;
    if export_rva == 0 {
        return Ok(Vec::new());
    }
    let sec_table = opt + opt_size;
    let mut secs = Vec::with_capacity(sections.min(96));
    for i in 0..sections.min(96) {
        let s = sec_table + i * 40;
        let vsize = u32_at(d, s + 8)?;
        let va = u32_at(d, s + 12)?;
        let raw_size = u32_at(d, s + 16)?;
        let raw_ptr = u32_at(d, s + 20)?;
        secs.push((va, vsize.max(raw_size), raw_ptr));
    }
    let to_off = |rva: u32| -> PeResult<usize> {
        for &(va, size, raw) in &secs {
            if rva >= va && rva < va.saturating_add(size) {
                return Ok((rva - va) as usize + raw as usize);
            }
        }
        Err(format!("RVA 0x{rva:X} not in any section"))
    };
    let ed = to_off(export_rva)?;
    let count = u32_at(d, ed + 24)? as usize;
    if count > 1 << 20 {
        return Err(format!("implausible export name count {count}"));
    }
    let names_rva = u32_at(d, ed + 32)?;
    let names_off = to_off(names_rva)?;
    let mut out = Vec::with_capacity(count.min(d.len() / 4));
    for i in 0..count {
        let name_rva = u32_at(d, names_off + i * 4)?;
        let o = to_off(name_rva)?;
        let tail = d.get(o..).ok_or("name out of bounds")?;
        let len = tail
            .iter()
            .take(4096)
            .position(|&b| b == 0)
            .ok_or("unterminated export name")?;
        out.push(tail[..len].iter().map(|&b| char::from(b)).collect());
    }
    Ok(out)
}

/// Splits an MSVC-decorated native thunk `?execName@AClass@@...` into
/// `(script class name, function name)`, removing the C++ `A`/`U` class prefix.
pub fn parse_exec_symbol(symbol: &str) -> Option<(String, String)> {
    let rest = symbol.strip_prefix("?exec")?;
    let (func, rest) = rest.split_once('@')?;
    let (cpp_class, _) = rest.split_once("@@")?;
    if func.is_empty() || cpp_class.len() < 2 {
        return None;
    }
    let script_class = match cpp_class.as_bytes()[0] {
        b'A' | b'U' => &cpp_class[1..],
        _ => cpp_class,
    };
    Some((script_class.to_owned(), func.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_symbols_split() {
        assert_eq!(
            parse_exec_symbol("?execSleep@AActor@@QAEXAAUFFrame@@QAX@Z"),
            Some(("Actor".into(), "Sleep".into()))
        );
        assert_eq!(
            parse_exec_symbol("?execAbs@UObject@@QAEXAAUFFrame@@QAX@Z"),
            Some(("Object".into(), "Abs".into()))
        );
        assert_eq!(
            parse_exec_symbol("?StaticClass@AActor@@SAPAVUClass@@XZ"),
            None
        );
        assert_eq!(parse_exec_symbol("?exec@AActor@@"), None);
    }

    fn tiny_pe(names: &[&str]) -> Vec<u8> {
        // DOS header + PE32 with one section holding an export directory.
        let mut d = vec![0u8; 0x400];
        d[0..2].copy_from_slice(b"MZ");
        d[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        d[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        d[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        d[coff + 16..coff + 18].copy_from_slice(&224u16.to_le_bytes());
        let opt = coff + 20;
        d[opt..opt + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        // Export directory at RVA 0x1000 -> file 0x200.
        d[opt + 96..opt + 100].copy_from_slice(&0x1000u32.to_le_bytes());
        let sec = opt + 224;
        d[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes());
        d[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes());
        d[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());
        let ed = 0x200;
        d[ed + 24..ed + 28].copy_from_slice(&(names.len() as u32).to_le_bytes());
        d[ed + 32..ed + 36].copy_from_slice(&0x1040u32.to_le_bytes());
        let mut str_rva = 0x1080u32;
        for (i, n) in names.iter().enumerate() {
            let p = 0x240 + i * 4;
            d[p..p + 4].copy_from_slice(&str_rva.to_le_bytes());
            let so = (str_rva - 0x1000 + 0x200) as usize;
            d[so..so + n.len()].copy_from_slice(n.as_bytes());
            str_rva += n.len() as u32 + 1;
        }
        d
    }

    #[test]
    fn reads_synthetic_export_names() {
        let d = tiny_pe(&["?execFoo@AActor@@QAEXAAUFFrame@@QAX@Z", "Bar"]);
        assert_eq!(
            export_names(&d).unwrap(),
            vec![
                "?execFoo@AActor@@QAEXAAUFFrame@@QAX@Z".to_owned(),
                "Bar".to_owned()
            ]
        );
    }

    #[test]
    fn rejects_non_pe_and_truncation() {
        assert!(export_names(b"XX").is_err());
        let mut d = tiny_pe(&["A"]);
        d.truncate(0x90);
        assert!(export_names(&d).is_err());
    }
}
