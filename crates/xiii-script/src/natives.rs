//! Native-function catalog and bytecode call statistics.

use std::collections::{BTreeMap, BTreeSet};

use xiii_package::ObjectRef;

use crate::bytecode::{Script, TokenKind};
use crate::linker::{GlobalRef, ScriptSet};
use crate::reflect::{Function, PropertyKind, ScriptObject, function_flags, property_flags};

/// One function parameter (or return value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// Parameter name.
    pub name: String,
    /// Type name (`int`, `object`, `struct`, ...).
    pub type_name: String,
    /// Property flags.
    pub flags: u32,
}

impl Param {
    /// True for the return value.
    pub fn is_return(&self) -> bool {
        self.flags & property_flags::RETURN_PARM != 0
    }
}

/// One `native` function (flagged native or with a native index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEntry {
    /// Global reference.
    pub at: GlobalRef,
    /// Package name.
    pub package: String,
    /// Owner path inside the package (`Class` or `Class.State`).
    pub owner: String,
    /// Function name.
    pub name: String,
    /// Friendly name (operator symbol for operators).
    pub friendly_name: String,
    /// Native index (`iNative`, 0 = bound by name).
    pub native_index: u16,
    /// XIII function flags.
    pub flags: u32,
    /// Operator precedence.
    pub operator_precedence: u8,
    /// Parameters in declaration order (return value included, flagged).
    pub params: Vec<Param>,
}

impl NativeEntry {
    /// Script class owning the function (first component of [`NativeEntry::owner`]).
    pub fn class(&self) -> &str {
        self.owner.split('.').next().unwrap_or(&self.owner)
    }

    /// `Package.Owner.Name`.
    pub fn path(&self) -> String {
        format!("{}.{}.{}", self.package, self.owner, self.name)
    }
}

/// Parameters of a function: its child properties with the `Parm` flag.
pub fn function_params(set: &ScriptSet, at: GlobalRef, f: &Function) -> Vec<Param> {
    let mut out = Vec::new();
    let Some(p) = set.packages.get(at.package) else {
        return out;
    };
    let mut child = f.header.children;
    let mut guard = 0;
    while let ObjectRef::Export(e) = child {
        guard += 1;
        if guard > 4096 {
            break;
        }
        let Some(obj) = p.objects.get(&e) else { break };
        if let ScriptObject::Property(prop) = obj
            && prop.flags & property_flags::PARM != 0
        {
            let type_name = match prop.kind {
                PropertyKind::Object { class } => {
                    format!("object<{}>", p.ref_name(class))
                }
                PropertyKind::Class { meta_class, .. } => {
                    format!("class<{}>", p.ref_name(meta_class))
                }
                PropertyKind::Struct { strukt } => format!("struct<{}>", p.ref_name(strukt)),
                PropertyKind::Byte { enum_ref } if !enum_ref.is_null() => {
                    format!("byte<{}>", p.ref_name(enum_ref))
                }
                k => k.name().to_owned(),
            };
            out.push(Param {
                name: p.ref_name(child).to_owned(),
                type_name,
                flags: prop.flags,
            });
        }
        child = obj.field().next;
    }
    out
}

/// Every native function in the set.
pub fn native_catalog(set: &ScriptSet) -> Vec<NativeEntry> {
    let mut out = Vec::new();
    for (pi, p) in set.packages.iter().enumerate() {
        for (e, o) in &p.objects {
            let ScriptObject::Function(f) = o else {
                continue;
            };
            if !f.is_native() && f.native_index == 0 {
                continue;
            }
            let at = GlobalRef {
                package: pi,
                export: *e,
            };
            let r = ObjectRef::Export(*e);
            let owner = p
                .package
                .object_outer(r)
                .and_then(|o| p.package.object_path(o))
                .unwrap_or("")
                .to_owned();
            out.push(NativeEntry {
                at,
                package: p.name.clone(),
                owner,
                name: p.ref_name(r).to_owned(),
                friendly_name: p.name_text(f.header.friendly_name).to_owned(),
                native_index: f.native_index,
                flags: f.flags,
                operator_precedence: f.operator_precedence,
                params: function_params(set, at, f),
            });
        }
    }
    out
}

/// Call statistics over scripts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStats {
    /// Opcode histogram (natives counted under their first byte).
    pub opcodes: BTreeMap<u8, u64>,
    /// Native calls by index.
    pub native_indices: BTreeMap<u16, u64>,
    /// Final calls by resolved target path (`Package.Class.Function`), with a native flag.
    pub final_calls: BTreeMap<String, (bool, u64)>,
    /// Final calls whose target package is not loaded.
    pub final_unresolved: BTreeMap<String, u64>,
    /// Virtual calls by name.
    pub virtual_calls: BTreeMap<String, u64>,
    /// Global calls by name.
    pub global_calls: BTreeMap<String, u64>,
    /// Total tokens.
    pub tokens: u64,
}

impl CallStats {
    /// Adds the calls of one script decoded in package `from`.
    pub fn add_script(&mut self, set: &ScriptSet, from: usize, script: &Script) {
        let p = &set.packages[from];
        script.walk(&mut |t| {
            self.tokens += 1;
            *self.opcodes.entry(t.opcode).or_default() += 1;
            if let TokenKind::VirtualFunction { call, .. }
            | TokenKind::FinalFunction { call, .. }
            | TokenKind::GlobalFunction { call, .. }
            | TokenKind::DelegateFunction { call, .. }
            | TokenKind::NativeCall { call, .. } = &t.kind
            {
                // The EndFunctionParms terminator is consumed into `Call`; count it here so
                // the histogram covers every serialized token.
                let _ = call;
                self.tokens += 1;
                *self.opcodes.entry(0x16).or_default() += 1;
            }
            match &t.kind {
                TokenKind::NativeCall { index, .. } => {
                    *self.native_indices.entry(*index).or_default() += 1;
                }
                TokenKind::FinalFunction { function, .. } => match set.resolve(from, *function) {
                    Some(g) => {
                        let native = matches!(set.object(g), Some(ScriptObject::Function(f)) if f.is_native());
                        let e = self.final_calls.entry(set.path(g)).or_insert((native, 0));
                        e.1 += 1;
                    }
                    None => {
                        *self
                            .final_unresolved
                            .entry(p.ref_path(*function))
                            .or_default() += 1;
                    }
                },
                TokenKind::VirtualFunction { name, .. } => {
                    *self
                        .virtual_calls
                        .entry(p.name_text(*name).to_owned())
                        .or_default() += 1;
                }
                TokenKind::GlobalFunction { name, .. } => {
                    *self
                        .global_calls
                        .entry(p.name_text(*name).to_owned())
                        .or_default() += 1;
                }
                _ => {}
            }
        });
    }

    /// Adds every script (functions, states, class code) of a package.
    pub fn add_package(&mut self, set: &ScriptSet, from: usize) {
        for o in set.packages[from].objects.values() {
            if let Some(h) = o.struct_header() {
                self.add_script(set, from, &h.script);
            }
        }
    }
}

/// Cross-reference of native functions with DLL `?exec` symbols.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DllMatch {
    /// Natives with a matching `?exec<Name>@[AU]<Class>@@` symbol.
    pub matched: BTreeSet<String>,
    /// Natives without one.
    pub unmatched: BTreeSet<String>,
    /// `exec` symbols with no native script function of that class/name.
    pub symbols_without_function: BTreeSet<String>,
}

/// Matches natives to `(script class, function)` pairs parsed from DLL `?exec` symbols.
/// Operators and other natives implemented by generic thunks have no per-function symbol;
/// they are reported as unmatched.
pub fn match_dll_symbols(
    natives: &[NativeEntry],
    symbols: &BTreeSet<(String, String)>,
) -> DllMatch {
    let mut m = DllMatch::default();
    let lower: BTreeSet<(String, String)> = symbols
        .iter()
        .map(|(c, f)| (c.to_ascii_lowercase(), f.to_ascii_lowercase()))
        .collect();
    let mut used = BTreeSet::new();
    for n in natives {
        let key = (n.class().to_ascii_lowercase(), n.name.to_ascii_lowercase());
        if lower.contains(&key) {
            m.matched.insert(n.path());
            used.insert(key);
        } else {
            m.unmatched.insert(n.path());
        }
    }
    for (c, f) in symbols {
        if !used.contains(&(c.to_ascii_lowercase(), f.to_ascii_lowercase())) {
            m.symbols_without_function.insert(format!("{c}.{f}"));
        }
    }
    m
}

/// True when a function is `latent` (needs a resumable frame in an interpreter).
pub fn is_latent(flags: u32) -> bool {
    flags & function_flags::LATENT != 0
}
