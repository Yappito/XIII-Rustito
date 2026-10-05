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
