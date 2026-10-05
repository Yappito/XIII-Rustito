//! Synthetic installation trees for unit tests (no proprietary data).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::PACKAGE_MAGIC;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `std::env::temp_dir()`, removed on drop.
pub struct TempTree {
    root: PathBuf,
}

impl TempTree {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "xiii-install-test-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp tree");
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn bytes(&self, rel: &str, data: &[u8]) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, data).expect("write");
    }

    pub fn text(&self, rel: &str, s: &str) {
        self.bytes(rel, s.as_bytes());
    }

    /// Writes a file that starts with the package magic.
    pub fn package(&self, rel: &str) {
        let mut data = PACKAGE_MAGIC.to_vec();
        data.extend_from_slice(&[100, 0, 58, 0]);
        self.bytes(rel, &data);
    }

    pub fn dir(&self, rel: &str) {
        fs::create_dir_all(self.path(rel)).expect("mkdir");
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub const GOG_DEFAULT_INI: &str = "[Core.System]\r\nSavePath=..\\Save\r\nCachePath=..\\Cache\r\n\
    Paths=..\\System\\*.u\r\nPaths=..\\MapsUser\\*.unr\r\nPaths=..\\Maps\\*.unr\r\n\
    Paths=..\\Sounds\\*.uax\r\nPaths=..\\Music\\*.umx\r\n;;;Paths=..\\StaticMeshes\\*.usx\r\n\
    Paths=..\\Animations\\*.ukx\r\nPlateForm=0\r\nSpecificPackage=XIIIPersos.u\r\n\
    SpecificPackage=GUI.u\r\n\r\n[Engine.GameEngine]\r\nCacheSizeMegs=1\r\n\
    ServerPackages=GamePlay\r\nServerPackages=XIII\r\n\r\n[Editor.EditorEngine]\r\n\
    EditPackages=Core\r\nEditPackages=Engine\r\nEditPackages=XIII\r\n";

pub const STEAM_DEFAULT_INI: &str = "[Core.System]\r\nPaths=..\\System\\*.u\r\n\
    Paths=..\\Skins\\*.u\r\nPaths=..\\Maps\\*.unr\r\nPaths=..\\Maps\\BaseSP\\*.unr\r\n\
    Paths=..\\Maps\\BaseMP\\*.unr\r\nPaths=..\\Sounds\\*.uax\r\nPaths=..\\Music\\*.umx\r\n\
    Paths=..\\Textures\\*.utx\r\nPaths=..\\StaticMeshes\\*.usx\r\nPaths=..\\Animations\\*.ukx\r\n\
    SpecificPackage=XIIIPersos.u\r\nSpecificPackage=GUI.u\r\n\r\n[Engine.GameEngine]\r\n\
    ServerPackages=GamePlay\r\nServerPackages=XIII\r\nServerPackages=XIIIMPPlus\r\n\r\n\
    [Editor.EditorEngine]\r\nEditPackages=Core\r\nEditPackages=Engine\r\nEditPackages=XIII\r\n\
    EditPackages=XIIIPlus\r\nEditPackages=XIIIPersos\r\n";

/// Miniature of the measured GOG layout (spellings copied from the inventory).
pub fn gog_tree() -> TempTree {
    let t = TempTree::new("gog");
    for p in [
        "system/core.u",
        "system/engine.u",
        "system/xiii.u",
        "system/xidmaps.u",
        "system/PC/xiiipersos.u",
        "system/PC/gui.u",
        "Maps/Plage00.unr",
        "Maps/Plage01.unr",
        "TexturesPC/XIIIBar.utx",
        "StaticMeshes/StaticPlage2.usx",
        "Sounds/Footsteps.uax",
    ] {
        t.package(p);
    }
    t.text("system/Default.ini", GOG_DEFAULT_INI);
    t.bytes("Sounds/PC/Plage00.hxc", b"not a package");
    t.dir("MapsUser");
    t.text("Save/Profiles/profile.sav", "user data");
    t.text("system/XIII.log", "log");
    t
}

/// Miniature of the measured patched Steam layout.
pub fn steam_tree() -> TempTree {
    let t = TempTree::new("steam");
    for p in [
        "System/Core.u",
        "System/Engine.u",
        "System/XIII.u",
        "System/XIIIPlus.u",
        "System/XIDMaps.u",
        "System/XIIIPersos.u",
        "System/GUI.u",
        "Maps/BaseSP/Plage00.unr",
        "Maps/BaseSP/Plage01.unr",
        "Maps/BaseMP/CTF_Base.unr",
        "Textures/XIIIBar.utx",
        "StaticMeshes/StaticPlage2.usx",
        "Sounds/Footsteps.uax",
    ] {
        t.package(p);
    }
    t.text("System/Default.ini", STEAM_DEFAULT_INI);
    t.text(
        "Maps/Patch 1.4 Credits.txt",
        "XIII UNOFFICIAL PATCH 1.4 | Developed by someone",
    );
    t.dir("Skins");
    t
}
