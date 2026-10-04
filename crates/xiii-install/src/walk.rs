//! Bounded, read-only directory walk that never follows links.

use std::fs;
use std::path::{Path, PathBuf};

/// Directory names (compared case-insensitively, at any depth) that hold
/// per-user or generated state rather than shipped content. They are never
/// entered. The original engine writes saves to `Save`, its download cache to
/// `Cache` (see `CachePath=..\Cache` in `Default.ini`) and logs next to the
/// executable.
pub const EXCLUDED_DIR_NAMES: &[&str] = &["save", "saves", "cache", "logs"];

/// File extensions (lowercase, without the dot) that are never inventoried
/// because they are generated at runtime (for example `System/XIII.log`).
pub const EXCLUDED_FILE_EXTENSIONS: &[&str] = &["log", "tmp"];

/// One regular file found inside the installation root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryFile {
    /// Path relative to the root, exact on-disk spelling, `/`-separated.
    pub relative: String,
    /// File size in bytes as reported by the filesystem.
    pub size: u64,
}

impl InventoryFile {
    /// Lowercase `/`-separated parent directory (empty for root files).
    pub fn parent_key(&self) -> String {
        match self.relative.rfind('/') {
            Some(i) => self.relative[..i].to_ascii_lowercase(),
            None => String::new(),
        }
    }

    /// Exact file name.
    pub fn file_name(&self) -> &str {
        match self.relative.rfind('/') {
            Some(i) => &self.relative[i + 1..],
            None => &self.relative,
        }
    }
}

/// Result of the bounded walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Regular files, sorted by relative path.
    pub files: Vec<InventoryFile>,
    /// Directories (relative, exact spelling, `/`-separated), sorted.
    pub directories: Vec<String>,
    /// Directories that were deliberately not entered (saves, caches, logs).
    pub excluded_directories: Vec<String>,
    /// Symbolic links and junctions that were not followed.
    pub skipped_links: Vec<String>,
    /// Entries whose names are not valid UTF-8 and were skipped.
    pub skipped_non_utf8: Vec<PathBuf>,
    /// Directories deeper than the depth limit that were not entered.
    pub depth_limited: Vec<String>,
    /// Entries that could not be read (path, error text).
    pub unreadable: Vec<(String, String)>,
    /// True if the entry limit stopped the walk early.
    pub truncated: bool,
}

pub(crate) struct WalkLimits {
    pub max_depth: usize,
    pub max_entries: usize,
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Walks `root` without following symbolic links or junctions.
///
/// Depth 0 is the root itself; a directory at depth `max_depth` is listed but
/// its subdirectories are not entered.
pub(crate) fn walk(root: &Path, limits: &WalkLimits) -> Inventory {
    let mut inv = Inventory::default();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    let mut seen = 0usize;

    'outer: while let Some((dir, rel, depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                inv.unreadable.push((rel.clone(), err.to_string()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    inv.unreadable.push((rel.clone(), err.to_string()));
                    continue;
                }
            };
            seen += 1;
            if seen > limits.max_entries {
                inv.truncated = true;
                break 'outer;
            }
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                inv.skipped_non_utf8.push(entry.path());
                continue;
            };
            let child_rel = join_rel(&rel, name);
            // DirEntry::file_type does not traverse links; on Windows it
            // reports junctions and symlinks (name-surrogate reparse points)
            // as symlinks.
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(err) => {
                    inv.unreadable.push((child_rel, err.to_string()));
                    continue;
                }
            };
            if ft.is_symlink() {
                inv.skipped_links.push(child_rel);
            } else if ft.is_dir() {
                let lower = name.to_ascii_lowercase();
                if EXCLUDED_DIR_NAMES.contains(&lower.as_str()) {
                    inv.excluded_directories.push(child_rel);
                } else if depth >= limits.max_depth {
                    inv.depth_limited.push(child_rel);
                } else {
                    inv.directories.push(child_rel.clone());
                    stack.push((entry.path(), child_rel, depth + 1));
                }
            } else if ft.is_file() {
                let ext = Path::new(name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                if EXCLUDED_FILE_EXTENSIONS.contains(&ext.as_str()) {
                    continue;
                }
                let size = match entry.metadata() {
                    Ok(m) => m.len(),
                    Err(err) => {
                        inv.unreadable.push((child_rel, err.to_string()));
                        continue;
                    }
                };
                inv.files.push(InventoryFile {
                    relative: child_rel,
                    size,
                });
            }
        }
    }

    inv.files.sort_by(|a, b| a.relative.cmp(&b.relative));
    inv.directories.sort();
    inv.excluded_directories.sort();
    inv.skipped_links.sort();
    inv.depth_limited.sort();
    inv
}
