//! A set of loaded script packages with cross-package reference resolution.
//!
//! Filesystem-free: callers pass `(package name, bytes)`. Imports are resolved by path: an
//! import path `Engine.Actor.Spawn` names package `Engine` and object path `Actor.Spawn` in
//! it (case-insensitive), with a matching class name.

use std::collections::{BTreeMap, HashMap};

use xiii_package::{Limits, ObjectRef, Package, PackageError};

use crate::bytecode::ScriptLimits;
use crate::error::ScriptError;
use crate::reflect::{Function, ScriptObject, read_script_object, script_class_kind};

/// One package with all its reflected exports decoded.
#[derive(Debug)]
pub struct ScriptPackage {
    /// Package name (file stem as given by the caller).
    pub name: String,
    /// Parsed package tables.
    pub package: Package,
    /// File bytes.
    pub data: Vec<u8>,
    /// Decoded reflected exports by export index.
    pub objects: BTreeMap<u32, ScriptObject>,
    /// Reflected exports that failed to decode.
    pub errors: Vec<(u32, ScriptError)>,
    paths: HashMap<String, u32>,
}

impl ScriptPackage {
    /// Parses the package and decodes every reflected (Core) export. Decoding failures are
    /// collected in [`ScriptPackage::errors`]; table-level failures are returned.
    pub fn load(
        name: &str,
        data: Vec<u8>,
        limits: &ScriptLimits,
        package_limits: &Limits,
    ) -> Result<Self, PackageError> {
        let package = Package::parse(&data, package_limits)?;
        let mut objects = BTreeMap::new();
        let mut errors = Vec::new();
        let mut paths = HashMap::new();
        for (i, e) in package.exports().iter().enumerate() {
            if let Some(p) = package.object_path(ObjectRef::Export(i as u32)) {
                paths.entry(p.to_ascii_lowercase()).or_insert(i as u32);
            }
            let class = package.export_class_path(i).unwrap_or("?");
            if script_class_kind(class).is_none() || e.serial_size == 0 {
                continue;
            }
            match read_script_object(&package, &data, i, limits, package_limits) {
                Ok(o) => {
                    objects.insert(i as u32, o);
                }
                Err(err) => errors.push((i as u32, err)),
            }
        }
        Ok(Self {
            name: name.to_owned(),
            package,
            data,
            objects,
            errors,
            paths,
        })
    }

    /// Export index of an object path inside this package (case-insensitive).
    pub fn export_by_path(&self, path: &str) -> Option<u32> {
        self.paths.get(&path.to_ascii_lowercase()).copied()
    }

    /// Class path (`Package.Class`) recorded for an import in this package's import table. This
    /// is the class of the referenced object as the referencing package declares it (e.g.
    /// `Engine.Sound` for a sound-object import in `xiii.u`). `None` for a non-import reference.
    pub fn import_class_path(&self, r: ObjectRef) -> Option<String> {
        let ObjectRef::Import(i) = r else {
            return None;
        };
        let imp = self.package.imports().get(i as usize)?;
        Some(format!(
            "{}.{}",
            self.package.name(imp.class_package),
            self.package.name(imp.class_name)
        ))
    }

    /// Display path of a reference: exports as `Package.Path`, imports by their full path.
    pub fn ref_path(&self, r: ObjectRef) -> String {
        match r {
            ObjectRef::Null => "None".to_owned(),
            ObjectRef::Export(_) => format!(
                "{}.{}",
                self.name,
                self.package.object_path(r).unwrap_or("?")
            ),
            ObjectRef::Import(_) => self.package.object_path(r).unwrap_or("?").to_owned(),
        }
    }

    /// Last path component of a reference.
    pub fn ref_name(&self, r: ObjectRef) -> &str {
        match r {
            ObjectRef::Null => "None",
            _ => self.package.object_name(r).unwrap_or("?"),
        }
    }

    /// Name-table text.
    pub fn name_text(&self, index: u32) -> &str {
        self.package
            .names()
            .get(index as usize)
            .map_or("?", |n| n.text.as_str())
    }

    /// Function object at an export index.
    pub fn function(&self, export: u32) -> Option<&Function> {
        match self.objects.get(&export) {
            Some(ScriptObject::Function(f)) => Some(f),
            _ => None,
        }
    }
}

/// A package outside the loaded script set (e.g. a `.uax` sound package, `.utx` texture package)
/// that the runtime registered so references into it can be verified and their class recorded.
#[derive(Debug)]
pub struct ExternalPackage {
    /// Parsed tables when the installation contains and parses the package; `None` when the
    /// import names a package the installation does not contain (an explicit unresolved error).
    pub package: Option<Package>,
    /// Lowercase full object path -> export index, for the lazy verification lookup.
    paths: HashMap<String, u32>,
    /// Lowercase leaf object name -> export index, `None` when two exports share the leaf.
    /// UE2 `DynamicLoadObject` resolves a bare `Package.Name` by the object's name (ignoring its
    /// group outer), so a texture stored as `interface_home.continue01gris` is found as
    /// `XIIIMenuStart.continue01gris`.
    leaves: HashMap<String, Option<u32>>,
}

/// Result of lazily resolving a `Package.Object.Path` reference into an external package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalLookup {
    /// The package was not registered; the VM cannot verify the reference (it keeps the path as a
    /// lazy external object).
    Unknown,
    /// Registered, but the installation has no such package.
    MissingPackage,
    /// Registered and the package has no such export.
    MissingExport,
    /// Registered and the export exists; its class path.
    Found(String),
}

/// Global reference: package index in the set plus export index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlobalRef {
    /// Index into [`ScriptSet::packages`].
    pub package: usize,
    /// Export index in that package.
    pub export: u32,
}

/// Loaded packages plus a native-index table.
#[derive(Debug, Default)]
pub struct ScriptSet {
    /// Packages in load order.
    pub packages: Vec<ScriptPackage>,
    by_name: HashMap<String, usize>,
    natives: BTreeMap<u16, Vec<GlobalRef>>,
    /// Non-script packages registered for external-reference verification (lowercase root name).
    externals: HashMap<String, ExternalPackage>,
}

impl ScriptSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a decoded package; returns its index. Rebuilds the native-index table.
    pub fn add(&mut self, p: ScriptPackage) -> usize {
        let idx = self.packages.len();
        self.by_name.insert(p.name.to_ascii_lowercase(), idx);
        for (e, o) in &p.objects {
            if let ScriptObject::Function(f) = o
                && f.native_index != 0
            {
                self.natives
                    .entry(f.native_index)
                    .or_default()
                    .push(GlobalRef {
                        package: idx,
                        export: *e,
                    });
            }
        }
        self.packages.push(p);
        idx
    }

    /// Package index by name (case-insensitive).
    pub fn package_index(&self, name: &str) -> Option<usize> {
        self.by_name.get(&name.to_ascii_lowercase()).copied()
    }

    /// Functions registered for a native index (normally exactly one).
    pub fn native_functions(&self, index: u16) -> &[GlobalRef] {
        self.natives.get(&index).map_or(&[], Vec::as_slice)
    }

    /// All native indices with their functions.
    pub fn native_table(&self) -> &BTreeMap<u16, Vec<GlobalRef>> {
        &self.natives
    }

    /// Registers a non-script package (`.uax`, `.utx`, `.usx`, ...) from its bytes so object
    /// references into it can be verified and their class recorded. Table-level parse failures
    /// are returned; the caller decides whether that is fatal (the runtime keeps them
    /// non-fatal and records the package as missing).
    pub fn add_external_package(
        &mut self,
        name: &str,
        data: &[u8],
        limits: &Limits,
    ) -> Result<(), PackageError> {
        let package = Package::parse(data, limits)?;
        let mut paths = HashMap::new();
        let mut leaves: HashMap<String, Option<u32>> = HashMap::new();
        for i in 0..package.exports().len() {
            if let Some(p) = package.object_path(ObjectRef::Export(i as u32)) {
                paths.entry(p.to_ascii_lowercase()).or_insert(i as u32);
                let leaf = p.rsplit('.').next().unwrap_or(p).to_ascii_lowercase();
                leaves
                    .entry(leaf)
                    .and_modify(|e| *e = None)
                    .or_insert(Some(i as u32));
            }
        }
        self.externals.insert(
            name.to_ascii_lowercase(),
            ExternalPackage {
                package: Some(package),
                paths,
                leaves,
            },
        );
        Ok(())
    }

    /// Records that an import references a package the installation does not contain, so a
    /// reference into it is an explicit unresolved error (never `None`).
    pub fn add_missing_external(&mut self, name: &str) {
        self.externals
            .entry(name.to_ascii_lowercase())
            .or_insert(ExternalPackage {
                package: None,
                paths: HashMap::new(),
                leaves: HashMap::new(),
            });
    }

    /// True when `name` (a package root) has been registered as external or missing.
    pub fn has_external_package(&self, name: &str) -> bool {
        self.externals.contains_key(&name.to_ascii_lowercase())
    }

    /// Lazily resolves a `Package.Object.Path` reference against the registered external
    /// packages. `Unknown` when the package was never registered.
    pub fn external_lookup(&self, path: &str) -> ExternalLookup {
        let Some((root, object)) = path.split_once('.') else {
            return ExternalLookup::Unknown;
        };
        let Some(entry) = self.externals.get(&root.to_ascii_lowercase()) else {
            return ExternalLookup::Unknown;
        };
        let Some(package) = &entry.package else {
            return ExternalLookup::MissingPackage;
        };
        let object = object.to_ascii_lowercase();
        // Exact full path first; for a bare name (no group dot) fall back to UE2's
        // `StaticFindObject`-by-name behaviour, which ignores the group outer. Ambiguous leaves
        // are not guessed.
        let export = entry.paths.get(&object).copied().or_else(|| {
            if object.contains('.') {
                None
            } else {
                entry.leaves.get(&object).copied().flatten()
            }
        });
        match export {
            Some(export) => ExternalLookup::Found(
                package
                    .export_class_path(export as usize)
                    .unwrap_or("?")
                    .to_owned(),
            ),
            None => ExternalLookup::MissingExport,
        }
    }

    /// Resolves a reference made inside package `from` to the export that defines it.
    /// Imports are looked up by path in the named package, which must be loaded.
    pub fn resolve(&self, from: usize, r: ObjectRef) -> Option<GlobalRef> {
        let p = self.packages.get(from)?;
        match r {
            ObjectRef::Null => None,
            ObjectRef::Export(e) => Some(GlobalRef {
                package: from,
                export: e,
            }),
            ObjectRef::Import(_) => {
                let full = p.package.object_path(r)?;
                let (root, rest) = full.split_once('.')?;
                let target = self.package_index(root)?;
                let export = self.packages[target].export_by_path(rest)?;
                Some(GlobalRef {
                    package: target,
                    export,
                })
            }
        }
    }

    /// Decoded object for a global reference.
    pub fn object(&self, g: GlobalRef) -> Option<&ScriptObject> {
        self.packages.get(g.package)?.objects.get(&g.export)
    }

    /// `Package.Path` text for a global reference.
    pub fn path(&self, g: GlobalRef) -> String {
        self.packages.get(g.package).map_or_else(
            || "?".to_owned(),
            |p| p.ref_path(ObjectRef::Export(g.export)),
        )
    }

    /// Finds a function or state by name in a class or state scope, walking the struct
    /// chain (`SuperField`) across packages. `scope` must be a class or state export.
    pub fn find_field(&self, scope: GlobalRef, name: &str) -> Option<GlobalRef> {
        let mut cur = Some(scope);
        let mut guard = 0;
        while let Some(s) = cur {
            guard += 1;
            if guard > 256 {
                return None;
            }
            let p = self.packages.get(s.package)?;
            let header = p.objects.get(&s.export)?.struct_header()?;
            let mut child = header.children;
            let mut steps = 0;
            while let ObjectRef::Export(e) = child {
                steps += 1;
                if steps > 65_536 {
                    return None;
                }
                if p.ref_name(child).eq_ignore_ascii_case(name) {
                    return Some(GlobalRef {
                        package: s.package,
                        export: e,
                    });
                }
                child = p.objects.get(&e)?.field().next;
            }
            cur = self.resolve(s.package, header.field.super_field);
        }
        None
    }
}
