//! Best-effort, side-effect-free discovery of installation candidates.
//!
//! Discovery only proposes directories; it never opens, selects or merges an
//! installation. Callers must still pass the chosen root to
//! [`crate::Installation::open`], which validates content. Locations come from
//! environment variables (`ProgramFiles`, `ProgramFiles(x86)`, `SystemDrive`,
//! `HOME`) and Steam's own `libraryfolders.vdf`/`appmanifest_*.acf`; no drive
//! letters are hardcoded and the Windows registry is not read.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Steam application id of "XIII - Classic".
pub const STEAM_APP_ID: u32 = 1_170_760;

const MAX_VDF_BYTES: u64 = 4 << 20;
const MAX_VDF_DEPTH: usize = 32;
const MAX_GOG_DIR_ENTRIES: usize = 4096;

/// Where a candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateSource {
    /// `steamapps/appmanifest_1170760.acf` in this Steam library.
    SteamLibrary { library: PathBuf },
    /// A directory whose name starts with `XIII` under a known GOG parent.
    GogKnownLocation { parent: PathBuf },
}

/// A proposed installation root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub source: CandidateSource,
    /// Cheap content probe: a `system` directory with `Core.u` carrying the
    /// package magic. Not a substitute for [`crate::Installation::open`].
    pub looks_valid: bool,
}

/// Steam and GOG locations to search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryRoots {
    /// Steam client roots (containing `steamapps/libraryfolders.vdf`).
    pub steam_roots: Vec<PathBuf>,
    /// Directories whose `XIII*` children are GOG candidates.
    pub gog_parents: Vec<PathBuf>,
}

impl DiscoveryRoots {
    /// Default locations derived from environment variables only.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let mut roots = Self::default();
        for pf in [var("ProgramFiles(x86)"), var("ProgramFiles")]
            .into_iter()
            .flatten()
        {
            roots.steam_roots.push(pf.join("Steam"));
            roots.gog_parents.push(pf.join("GOG Galaxy").join("Games"));
            roots.gog_parents.push(pf.join("GOG Games"));
        }
        if let Some(drive) = var("SystemDrive") {
            let mut d = drive.into_os_string();
            d.push("\\");
            roots.gog_parents.push(PathBuf::from(d).join("GOG Games"));
        }
        if let Some(home) = var("HOME") {
            roots.steam_roots.push(home.join(".steam").join("steam"));
            roots
                .steam_roots
                .push(home.join(".local").join("share").join("Steam"));
            roots.gog_parents.push(home.join("GOG Games"));
        }
        roots
    }
}

/// Discovers candidates using [`DiscoveryRoots::from_env`].
pub fn discover_candidates() -> Vec<Candidate> {
    discover_candidates_in(&DiscoveryRoots::from_env())
}

/// Discovers candidates from explicit locations. Missing or unreadable
/// locations are skipped silently: discovery is advisory.
pub fn discover_candidates_in(roots: &DiscoveryRoots) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |path: PathBuf, source: CandidateSource| {
        if !out.iter().any(|c| same_path(&c.path, &path)) && path.is_dir() {
            let looks_valid = probe(&path);
            out.push(Candidate {
                path,
                source,
                looks_valid,
            });
        }
    };

    for steam in &roots.steam_roots {
        for library in steam_libraries(steam) {
            if let Some(dir) = steam_install_dir(&library) {
                push(dir, CandidateSource::SteamLibrary { library });
            }
        }
    }
    for parent in &roots.gog_parents {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_GOG_DIR_ENTRIES) {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_dir && name.to_ascii_lowercase().starts_with("xiii") {
                push(
                    entry.path(),
                    CandidateSource::GogKnownLocation {
                        parent: parent.clone(),
                    },
                );
            }
        }
    }
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Cheap probe: `<root>/system/Core.u` (any case) starts with the package magic.
pub fn probe(root: &Path) -> bool {
    let Some(system) = find_child_ci(root, "system", true) else {
        return false;
    };
    find_child_ci(&system, "core.u", false).is_some_and(|p| crate::has_package_magic(&p))
}

fn find_child_ci(dir: &Path, name: &str, want_dir: bool) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let ft = e.file_type().ok()?;
        let ok = if want_dir { ft.is_dir() } else { ft.is_file() };
        (ok && e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).then(|| e.path())
    })
}

fn read_vdf(path: &Path) -> Option<Kv> {
    let mut buf = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_VDF_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    parse_vdf(&String::from_utf8_lossy(&buf))
}

/// Library folders listed by a Steam client root (including the root itself).
pub fn steam_libraries(steam_root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![steam_root.to_path_buf()];
    let vdf = ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"]
        .iter()
        .find_map(|rel| read_vdf(&steam_root.join(rel)));
    if let Some(Kv::Obj(top)) = vdf
        && let Some(Kv::Obj(folders)) = kv_get(&top, "libraryfolders")
    {
        for (_, folder) in folders {
            match folder {
                // Current format: "0" { "path" "..." ... }
                Kv::Obj(fields) => {
                    if let Some(Kv::Str(p)) = kv_get(fields, "path") {
                        libs.push(PathBuf::from(p));
                    }
                }
                // Legacy format: "1" "D:\\SteamLibrary"
                Kv::Str(p) => libs.push(PathBuf::from(p)),
            }
        }
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for l in libs {
        if !unique.iter().any(|u| same_path(u, &l)) {
            unique.push(l);
        }
    }
    unique
}

/// Install directory of app [`STEAM_APP_ID`] in one library, from its
/// `appmanifest_1170760.acf` `installdir` (a single path component).
pub fn steam_install_dir(library: &Path) -> Option<PathBuf> {
    let steamapps = library.join("steamapps");
    let manifest = steamapps.join(format!("appmanifest_{STEAM_APP_ID}.acf"));
    let Kv::Obj(top) = read_vdf(&manifest)? else {
        return None;
    };
    let Some(Kv::Obj(state)) = kv_get(&top, "AppState") else {
        return None;
    };
    let Some(Kv::Str(dir)) = kv_get(state, "installdir") else {
        return None;
    };
    let single = !dir.is_empty() && dir != "." && dir != ".." && !dir.contains(['/', '\\', ':']);
    single.then(|| steamapps.join("common").join(dir))
}

/// Minimal Valve KeyValues (text VDF) tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kv {
    Str(String),
    Obj(Vec<(String, Kv)>),
}

fn kv_get<'a>(fields: &'a [(String, Kv)], key: &str) -> Option<&'a Kv> {
    fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Option<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => toks.push(Tok::Open),
            '}' => toks.push(Tok::Close),
            '/' if it.peek() == Some(&'/') => {
                for c in it.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            other => s.push(other),
                        },
                        other => s.push(other),
                    }
                }
                toks.push(Tok::Str(s));
            }
            other => {
                let mut s = String::from(other);
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    s.push(c);
                    it.next();
                }
                toks.push(Tok::Str(s));
            }
        }
    }
    Some(toks)
}

/// Parses text VDF into a top-level object. Returns `None` on malformed input.
pub fn parse_vdf(text: &str) -> Option<Kv> {
    fn obj(toks: &[Tok], pos: &mut usize, depth: usize, top: bool) -> Option<Vec<(String, Kv)>> {
        if depth > MAX_VDF_DEPTH {
            return None;
        }
        let mut fields = Vec::new();
        loop {
            match toks.get(*pos) {
                None if top => return Some(fields),
                None => return None,
                Some(Tok::Close) if !top => {
                    *pos += 1;
                    return Some(fields);
                }
                Some(Tok::Str(key)) => {
                    *pos += 1;
                    let value = match toks.get(*pos)? {
                        Tok::Str(v) => {
                            *pos += 1;
                            Kv::Str(v.clone())
                        }
                        Tok::Open => {
                            *pos += 1;
                            Kv::Obj(obj(toks, pos, depth + 1, false)?)
                        }
                        Tok::Close => return None,
                    };
                    fields.push((key.clone(), value));
                }
                Some(_) => return None,
            }
        }
    }
    let toks = tokenize(text)?;
    let mut pos = 0;
    obj(&toks, &mut pos, 0, true).map(Kv::Obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempTree;

    const LIBRARYFOLDERS: &str = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"__STEAM__"
		"apps" { "220" "1" }
	}
	"1"
	{
		"path"		"__LIB__"
		"apps"
		{
			"1170760"		"2656459688"
		}
	}
}
"#;

    #[test]
    fn parses_vdf_with_escapes_and_comments() {
        let kv = parse_vdf("// c\n\"a\" { \"p\" \"C:\\\\X\\\\Y\" \"n\" { } }").unwrap();
        let Kv::Obj(top) = kv else { panic!() };
        let Some(Kv::Obj(a)) = kv_get(&top, "A") else {
            panic!()
        };
        assert_eq!(kv_get(a, "p"), Some(&Kv::Str("C:\\X\\Y".into())));
        assert!(parse_vdf("\"a\" { \"b\" ").is_none());
        assert!(parse_vdf("\"a\" { \"b\" \"c\" } }").is_none());
    }

    #[test]
    fn finds_steam_library_install_and_gog_candidate() {
        let t = TempTree::new("discovery");
        let steam = t.path("Steam");
        let lib = t.path("Lib");
        let vdf = LIBRARYFOLDERS
            .replace("__STEAM__", &steam.to_string_lossy().replace('\\', "\\\\"))
            .replace("__LIB__", &lib.to_string_lossy().replace('\\', "\\\\"));
        t.text("Steam/steamapps/libraryfolders.vdf", &vdf);
        t.text(
            "Lib/steamapps/appmanifest_1170760.acf",
            "\"AppState\" { \"appid\" \"1170760\" \"installdir\" \"XIII - Classic\" }",
        );
        t.package("Lib/steamapps/common/XIII - Classic/System/Core.u");
        t.package("GogParent/XIII/system/core.u");
        t.text("GogParent/NotXIII/readme.txt", "x");
        // An installdir escaping the library is ignored.
        t.text(
            "Evil/steamapps/appmanifest_1170760.acf",
            "\"AppState\" { \"installdir\" \"..\\\\..\" }",
        );

        let roots = DiscoveryRoots {
            steam_roots: vec![steam.clone(), t.path("Evil"), t.path("missing")],
            gog_parents: vec![t.path("GogParent"), t.path("missing")],
        };
        assert_eq!(steam_libraries(&steam).len(), 2);
        assert_eq!(steam_install_dir(&t.path("Evil")), None);
        let found = discover_candidates_in(&roots);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|c| c.looks_valid));
        assert!(matches!(
            found[0].source,
            CandidateSource::SteamLibrary { .. }
        ));
        assert!(found[0].path.ends_with("XIII - Classic"));
        assert!(matches!(
            found[1].source,
            CandidateSource::GogKnownLocation { .. }
        ));
    }
}
