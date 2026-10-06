//! Versioned, user-directory checkpoint files. This stores the state XIII's checkpoint
//! script prepares, rather than a snapshot of the interpreter heap.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"XIIISAV\0";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct SaveFile {
    pub map: String,
    pub teleporter: String,
    pub description: String,
    pub health: f32,
    pub speed_factor_limit: f32,
    pub checkpoint_number: i32,
    pub location: [f32; 3],
    pub rotation: [i32; 3],
    pub objectives: Vec<Objective>,
    pub inventory: Vec<InventoryItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Objective {
    pub completed: bool,
    pub primary: bool,
    pub anti_goal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryItem {
    pub class_path: String,
    pub name: String,
}

/// Metadata shown by the decoded load-game page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotInfo {
    pub slot: u32,
    pub description: String,
    pub modified_unix: u64,
}

/// Host-side store adapter for the ten GUI save slots.
pub struct SaveStore {
    dir: PathBuf,
}

impl SaveStore {
    pub fn open(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn list(&self) -> Result<Vec<SlotInfo>, String> {
        let mut slots = Vec::new();
        for slot in 0..10 {
            let path = slot_path(&self.dir, slot);
            if !path.is_file() {
                continue;
            }
            let save = read(&self.dir, slot)?;
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map_err(|e| format!("reading modification time for {}: {e}", path.display()))?;
            let modified_unix = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("save slot {} predates the Unix epoch: {e}", path.display()))?
                .as_secs();
            slots.push(SlotInfo {
                slot,
                description: save.description,
                modified_unix,
            });
        }
        Ok(slots)
    }
    pub fn read(&self, slot: u32) -> Result<SaveFile, String> {
        read(&self.dir, slot)
    }
    pub fn newest(&self) -> Result<Option<SlotInfo>, String> {
        Ok(self
            .list()?
            .into_iter()
            .max_by_key(|s| (s.modified_unix, s.slot)))
    }
}

pub fn default_save_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA is not set; provide --save-dir".to_owned())?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| "neither XDG_DATA_HOME nor HOME is set; provide --save-dir".to_owned())?;
    Ok(base.join("xiii-rustito").join("saves"))
}

fn slot_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("slot{slot}.sav"))
}

pub fn write(dir: &Path, slot: u32, save: &SaveFile) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("creating save directory {}: {e}", dir.display()))?;
    let path = slot_path(dir, slot);
    let temp = dir.join(format!(".slot{slot}.{}.tmp", std::process::id()));
    let bytes = encode(save)?;
    std::fs::write(&temp, bytes).map_err(|e| format!("writing {}: {e}", temp.display()))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("replacing {}: {e}", path.display()));
    }
    Ok(())
}

pub fn read(dir: &Path, slot: u32) -> Result<SaveFile, String> {
    let path = slot_path(dir, slot);
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("reading save slot {slot} ({}): {e}", path.display()))?;
    decode(&bytes)
}

pub fn exists(dir: &Path, slot: u32) -> bool {
    slot_path(dir, slot).is_file()
}

fn put_string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let len = u32::try_from(value.len()).map_err(|_| "save string too long".to_owned())?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode(s: &SaveFile) -> Result<Vec<u8>, String> {
    if !s.health.is_finite()
        || !s.speed_factor_limit.is_finite()
        || s.location.iter().any(|v| !v.is_finite())
    {
        return Err("save contains non-finite player values".into());
    }
    let mut b = MAGIC.to_vec();
    b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    put_string(&mut b, &s.map)?;
    put_string(&mut b, &s.teleporter)?;
    put_string(&mut b, &s.description)?;
    b.extend_from_slice(&s.health.to_le_bytes());
    b.extend_from_slice(&s.speed_factor_limit.to_le_bytes());
    b.extend_from_slice(&s.checkpoint_number.to_le_bytes());
    for v in s.location {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for v in s.rotation {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(
        &u32::try_from(s.objectives.len())
            .map_err(|_| "too many objectives")?
            .to_le_bytes(),
    );
    for o in &s.objectives {
        b.extend_from_slice(&[o.completed as u8, o.primary as u8, o.anti_goal as u8]);
    }
    b.extend_from_slice(
        &u32::try_from(s.inventory.len())
            .map_err(|_| "inventory too large")?
            .to_le_bytes(),
    );
    for i in &s.inventory {
        put_string(&mut b, &i.class_path)?;
        put_string(&mut b, &i.name)?;
    }
    Ok(b)
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.p.checked_add(n).ok_or("save length overflow")?;
        let v = self.b.get(self.p..end).ok_or("truncated save file")?;
        self.p = end;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| "truncated u32")?,
        ))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| "truncated i32")?,
        ))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| "truncated f32")?,
        ))
    }
    fn string(&mut self) -> Result<String, String> {
        let n = usize::try_from(self.u32()?).map_err(|_| "string length overflow")?;
        if n > 1_048_576 {
            return Err("save string exceeds 1 MiB limit".into());
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|e| format!("invalid UTF-8 in save: {e}"))
    }
}

fn decode(b: &[u8]) -> Result<SaveFile, String> {
    let mut c = Cursor { b, p: 0 };
    if c.take(MAGIC.len())? != MAGIC {
        return Err("invalid save magic".into());
    }
    let version = c.u32()?;
    if version != FORMAT_VERSION {
        return Err(format!(
            "unsupported save version {version} (supported {FORMAT_VERSION})"
        ));
    }
    let map = c.string()?;
    let teleporter = c.string()?;
    let description = c.string()?;
    let health = c.f32()?;
    let speed_factor_limit = c.f32()?;
    let checkpoint_number = c.i32()?;
    let mut location = [0.; 3];
    for v in &mut location {
        *v = c.f32()?;
    }
    let mut rotation = [0; 3];
    for v in &mut rotation {
        *v = c.i32()?;
    }
    let n = c.u32()?;
    if n > 4096 {
        return Err(format!("objective count {n} exceeds limit"));
    }
    let mut objectives = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let f = c.take(3)?;
        if f.iter().any(|v| *v > 1) {
            return Err("invalid objective flag".into());
        }
        objectives.push(Objective {
            completed: f[0] != 0,
            primary: f[1] != 0,
            anti_goal: f[2] != 0,
        });
    }
    let n = c.u32()?;
    if n > 4096 {
        return Err(format!("inventory count {n} exceeds limit"));
    }
    let mut inventory = Vec::with_capacity(n as usize);
    for _ in 0..n {
        inventory.push(InventoryItem {
            class_path: c.string()?,
            name: c.string()?,
        });
    }
    if c.p != b.len() {
        return Err(format!("{} trailing bytes in save", b.len() - c.p));
    }
    if !health.is_finite()
        || !speed_factor_limit.is_finite()
        || location.iter().any(|v| !v.is_finite())
    {
        return Err("save contains non-finite player values".into());
    }
    Ok(SaveFile {
        map,
        teleporter,
        description,
        health,
        speed_factor_limit,
        checkpoint_number,
        location,
        rotation,
        objectives,
        inventory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> SaveFile {
        SaveFile {
            map: "Plage00".into(),
            teleporter: "PlayerStart".into(),
            description: "Brighton Beach 1".into(),
            health: 87.,
            speed_factor_limit: 0.5,
            checkpoint_number: 2,
            location: [1., -2., 3.],
            rotation: [4, 5, 6],
            objectives: vec![Objective {
                completed: true,
                primary: false,
                anti_goal: false,
            }],
            inventory: vec![InventoryItem {
                class_path: "XIII.Fists".into(),
                name: "Fists0".into(),
            }],
        }
    }
    #[test]
    fn file_round_trip_and_slot_replacement() {
        let d = std::env::temp_dir().join(format!("xiii-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        write(&d, 0, &sample()).unwrap();
        assert_eq!(read(&d, 0).unwrap(), sample());
        let mut s = sample();
        s.health = 12.;
        write(&d, 0, &s).unwrap();
        assert_eq!(read(&d, 0).unwrap().health, 12.);
        let _ = std::fs::remove_dir_all(d);
    }
    #[test]
    fn rejects_unknown_version_truncation_and_trailing_data() {
        let mut b = encode(&sample()).unwrap();
        b[8] = 2;
        assert!(decode(&b).unwrap_err().contains("unsupported save version"));
        let b = encode(&sample()).unwrap();
        assert!(decode(&b[..b.len() - 1]).is_err());
        let mut b = encode(&sample()).unwrap();
        b.push(0);
        assert!(decode(&b).unwrap_err().contains("trailing bytes"));
    }
    #[test]
    fn rejects_non_finite_values() {
        let mut s = sample();
        s.health = f32::NAN;
        assert!(encode(&s).is_err());
    }

    #[test]
    fn store_lists_valid_slots_and_orders_newest_with_stable_ties() {
        let d = std::env::temp_dir().join(format!("xiii-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let store = SaveStore::open(d.clone());
        assert!(store.list().unwrap().is_empty());
        write(&d, 2, &sample()).unwrap();
        let mut newer = sample();
        newer.description = "newer".into();
        write(&d, 7, &newer).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(
            listed.iter().map(|s| s.slot).collect::<Vec<_>>(),
            vec![2, 7]
        );
        assert_eq!(store.read(7).unwrap().description, "newer");
        assert_eq!(store.newest().unwrap().unwrap().slot, 7);
        let _ = std::fs::remove_dir_all(d);
    }
}
