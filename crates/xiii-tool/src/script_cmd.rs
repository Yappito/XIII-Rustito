//! `xiii-tool script ...`: reflected script objects, bytecode disassembly, native catalog and
//! corpus coverage (M2c). Installation files are only read.
//!
//! Disassembly output is derived from proprietary packages: keep it local (scratch
//! directories), never in the repository. `coverage --json-out` writes metadata only
//! (class/function names, native indices, counts).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use serde_json::{Value, json};
use xiii_package::{Limits, ObjectRef};
use xiii_script::bytecode::opcode_name;
use xiii_script::disasm::Disasm;
use xiii_script::natives::{CallStats, NativeEntry, match_dll_symbols, native_catalog};
use xiii_script::reflect::{ScriptObject, function_flags, script_class_kind};
use xiii_script::{GlobalRef, ScriptClassKind, ScriptLimits, ScriptPackage, ScriptSet};

use crate::corpus::tagged_files;

/// Usage text for `xiii-tool script`.
pub const USAGE: &str = "\
xiii-tool script: compiled UnrealScript (M2c)

  xiii-tool script classes <package.u> [--game-dir <root>]
      List classes: super, flags, within, config, functions/states, defaults.
  xiii-tool script functions <package.u> [--class <name>] [--game-dir <root>]
      List functions/states: native index, flags, memory/file script size, tokens.
  xiii-tool script disasm <package.u> <object-path> [--game-dir <root>] [--tokens]
      Disassemble one function/state, or every function/state of a class.
      --game-dir loads all .u packages of the installation to name natives and
      resolve imports. OUTPUT CONTAINS PROPRIETARY CODE: keep it local.
  xiii-tool script defaults <package.u> <Package.Class> [--game-dir <root>] [--name <prop>]
      Print the resolved (inherited) default values of a class: walks the class chain
      root-to-leaf and applies each class's own defaults, so the printed value is the
      effective default (e.g. CollisionRadius on a player pawn). --name filters by
      case-insensitive property name; repeatable. Read-only.
  xiii-tool script natives <root> [--json]
      Catalog of native functions (package, owner, name, index, flags, params).
  xiii-tool script coverage <root> [--json-out <file>] [--dll-dir <dir>]
      Decode every reflected export of every .u package; token/native statistics;
      DLL ?exec symbol cross-reference (default dll dir: <root>/system).
      Exits 1 if any reflected export fails to decode.
  xiii-tool script run --game-dir <root> [--map Plage00] [--touch TouchTrigger2]
                       [--ticks 60] [--dt 0.0333] [--touch-tick 1] [--trace]
                       [--active TouchTrigger,XIIIDispatcher] [--no-natives] [--budget N]
      Headless interpreter harness: load the map's actors, run PostBeginPlay and
      SetInitialState for the executed scope, deliver Touch(synthetic player) to
      the touched actor, tick at a fixed step and print the behaviour trace.
      Calls into actors outside the scope are reported as DEFERRED (not run).
      Exits 1 on a script error (printed with its script stack).";

const GAME_PACKAGES: &[&str] = &["xidmaps", "xiii", "xidcine"];

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn emit(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// Entry point for `xiii-tool script <sub> ...`.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("classes") => classes_cmd(&args[1..]),
        Some("functions") => functions_cmd(&args[1..]),
        Some("disasm") => disasm_cmd(&args[1..]),
        Some("natives") => natives_cmd(&args[1..]),
        Some("defaults") => defaults_cmd(&args[1..]),
        Some("coverage") => coverage_cmd(&args[1..]),
        Some("run") => crate::script_run::run_cmd(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown script command '{other}'")),
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Lists every `.u` package of an installation (relative path, full path).
pub fn script_packages(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    Ok(tagged_files(root)?
        .into_iter()
        .filter(|(rel, _)| rel.to_ascii_lowercase().ends_with(".u"))
        .collect())
}

/// Loads every `.u` package of an installation into a set. Table-level failures are returned
/// as `(relative path, error)`.
pub fn load_install(root: &Path) -> std::io::Result<(ScriptSet, Vec<(String, String)>)> {
    let mut set = ScriptSet::new();
    let mut failures = Vec::new();
    for (rel, path) in script_packages(root)? {
        let data = std::fs::read(&path)?;
        match ScriptPackage::load(
            &stem(&path),
            data,
            &ScriptLimits::default(),
            &Limits::default(),
        ) {
            Ok(p) => {
                set.add(p);
            }
            Err(e) => failures.push((rel, e.to_string())),
        }
    }
    Ok((set, failures))
}

/// Loads one package file, plus every `.u` of `game_dir` if given (the file's own package is
/// not loaded twice). Returns the set and the index of the requested package.
fn load_target(file: &Path, game_dir: Option<&Path>) -> Result<(ScriptSet, usize), String> {
    let name = stem(file);
    let mut set = ScriptSet::new();
    if let Some(root) = game_dir {
        let (s, failures) =
            load_install(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
        if let Some((rel, e)) = failures.first() {
            return Err(format!("{rel}: {e}"));
        }
        set = s;
    }
    if let Some(i) = set.package_index(&name) {
        return Ok((set, i));
    }
    let data = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let p = ScriptPackage::load(&name, data, &ScriptLimits::default(), &Limits::default())
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let i = set.add(p);
    Ok((set, i))
}

struct Common {
    file: Option<PathBuf>,
    game_dir: Option<PathBuf>,
    class: Option<String>,
    rest: Vec<String>,
    tokens: bool,
    json: bool,
    json_out: Option<PathBuf>,
    dll_dir: Option<PathBuf>,
    names: Vec<String>,
}

fn parse(args: &[String]) -> Result<Common, String> {
    let mut c = Common {
        file: None,
        game_dir: None,
        class: None,
        rest: Vec::new(),
        tokens: false,
        json: false,
        json_out: None,
        dll_dir: None,
        names: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match a.as_str() {
            "--game-dir" => c.game_dir = Some(PathBuf::from(val("--game-dir")?)),
            "--class" => c.class = Some(val("--class")?),
            "--json-out" => c.json_out = Some(PathBuf::from(val("--json-out")?)),
            "--dll-dir" => c.dll_dir = Some(PathBuf::from(val("--dll-dir")?)),
            "--name" => c.names.push(val("--name")?),
            "--tokens" => c.tokens = true,
            "--json" => c.json = true,
            s if s.starts_with("--") => return Err(format!("unknown option '{s}'")),
            s if c.file.is_none() => c.file = Some(PathBuf::from(s)),
            s => c.rest.push(s.to_owned()),
        }
    }
    Ok(c)
}

fn hex_flags(flags: u32) -> String {
    function_flags::names(flags).join(" ")
}

fn classes_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = c.file else {
        return usage_error("classes needs a package file");
    };
    let (set, pi) = match load_target(&file, c.game_dir.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let p = &set.packages[pi];
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {} reflected exports decoded, {} failed",
        file.display(),
        p.objects.len(),
        p.errors.len()
    );
    for (e, o) in &p.objects {
        let ScriptObject::Class(cl) = o else { continue };
        let r = ObjectRef::Export(*e);
        let (mut funcs, mut states) = (0, 0);
        for (ce, co) in &p.objects {
            let outer = p.package.object_outer(ObjectRef::Export(*ce));
            if outer == Some(r) {
                match co {
                    ScriptObject::Function(_) => funcs += 1,
                    ScriptObject::State(_) => states += 1,
                    _ => {}
                }
            }
        }
        let _ = writeln!(
            out,
            "{:<32} super {:<28} flags 0x{:04X} within {:<12} config {:<10} funcs {:>3} states {:>2} code {:>5}B defaults {:>3} props ({} B)",
            p.ref_name(r),
            p.ref_name(cl.header.field.super_field),
            cl.class_flags,
            p.ref_name(cl.within),
            p.name_text(cl.config_name),
            funcs,
            states,
            cl.header.script.memory_size,
            cl.defaults.properties.len(),
            cl.defaults.span.len()
        );
    }
    for (e, err) in &p.errors {
        let _ = writeln!(out, "FAILED {}: {err}", p.ref_path(ObjectRef::Export(*e)));
    }
    emit(&out);
    if p.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn functions_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = c.file else {
        return usage_error("functions needs a package file");
    };
    let (set, pi) = match load_target(&file, c.game_dir.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let p = &set.packages[pi];
    let mut out = String::new();
    for (e, o) in &p.objects {
        let path = p.package.object_path(ObjectRef::Export(*e)).unwrap_or("?");
        if let Some(cls) = &c.class {
            let first = path.split('.').next().unwrap_or("");
            if !first.eq_ignore_ascii_case(cls) {
                continue;
            }
        }
        match o {
            ScriptObject::Function(f) => {
                let _ = writeln!(
                    out,
                    "function {:<48} native {:>4} prec {:>2} code {:>5}/{:>5}B tokens {:>5} [{}]",
                    path,
                    f.native_index,
                    f.operator_precedence,
                    f.header.script.memory_size,
                    f.header.script.file_span.len(),
                    f.header.script.token_count,
                    hex_flags(f.flags)
                );
            }
            ScriptObject::State(s) => {
                let _ = writeln!(
                    out,
                    "state    {:<48} labels@0x{:04X} flags 0x{:04X} code {:>5}/{:>5}B tokens {:>5}",
                    path,
                    s.state.label_table_offset,
                    s.state.state_flags,
                    s.header.script.memory_size,
                    s.header.script.file_span.len(),
                    s.header.script.token_count
                );
            }
            _ => {}
        }
    }
    emit(&out);
    ExitCode::SUCCESS
}

/// Disassembly of a function/state (or all of a class's) for local use.
pub fn disassemble(set: &ScriptSet, pi: usize, path: &str, tokens: bool) -> Result<String, String> {
    let p = &set.packages[pi];
    let e = p
        .export_by_path(path)
        .ok_or_else(|| format!("no export with path '{path}' in {}", p.name))?;
    let d = Disasm::new(set, pi);
    let mut out = String::new();
    let emit_one = |e: u32, out: &mut String| {
        let r = ObjectRef::Export(e);
        let Some(o) = p.objects.get(&e) else { return };
        let Some(h) = o.struct_header() else { return };
        let what = match o {
            ScriptObject::Function(f) => format!(
                "function native={} flags=[{}]",
                f.native_index,
                hex_flags(f.flags)
            ),
            ScriptObject::State(s) => format!(
                "state labels@0x{:04X} flags=0x{:X}",
                s.state.label_table_offset, s.state.state_flags
            ),
            ScriptObject::Class(_) => "class code".to_owned(),
            _ => "struct".to_owned(),
        };
        let _ = writeln!(
            out,
            "== {} ({what}) code {} B memory / {} B file @{}..{} tokens {}",
            p.ref_path(r),
            h.script.memory_size,
            h.script.file_span.len(),
            h.script.file_span.start,
            h.script.file_span.end,
            h.script.token_count
        );
        if let ScriptObject::Function(f) = o {
            let params = xiii_script::natives::function_params(
                set,
                GlobalRef {
                    package: pi,
                    export: e,
                },
                f,
            );
            if !params.is_empty() {
                let ps: Vec<String> = params
                    .iter()
                    .map(|q| {
                        format!(
                            "{}{} {}",
                            if q.is_return() { "return " } else { "" },
                            q.type_name,
                            q.name
                        )
                    })
                    .collect();
                let _ = writeln!(out, "   params: {}", ps.join(", "));
            }
        }
        out.push_str(&if tokens {
            d.token_tree(&h.script)
        } else {
            d.listing(&h.script)
        });
    };
    match p.objects.get(&e) {
        Some(ScriptObject::Class(cl)) => {
            emit_one(e, &mut out);
            let _ = writeln!(
                out,
                "   defaults ({} properties, file {}..{}):",
                cl.defaults.properties.len(),
                cl.defaults.span.start,
                cl.defaults.span.end
            );
            for q in &cl.defaults.properties {
                let idx = if q.array_index != 0 {
                    format!("[{}]", q.array_index)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "     {}{idx} = {}",
                    p.package.property_name(q),
                    crate::props::value_text(&p.package, q)
                );
            }
            // Functions and states owned by the class, and functions inside its states.
            let owned = |x: u32, owner: u32| {
                p.package.object_outer(ObjectRef::Export(x)) == Some(ObjectRef::Export(owner))
            };
            for (ce, co) in &p.objects {
                if owned(*ce, e) && matches!(co, ScriptObject::Function(_)) {
                    emit_one(*ce, &mut out);
                }
            }
            for (se, so) in &p.objects {
                if owned(*se, e) && matches!(so, ScriptObject::State(_)) {
                    emit_one(*se, &mut out);
                    for (fe, fo) in &p.objects {
                        if owned(*fe, *se) && matches!(fo, ScriptObject::Function(_)) {
                            emit_one(*fe, &mut out);
                        }
                    }
                }
            }
        }
        Some(_) => emit_one(e, &mut out),
        None => return Err(format!("'{path}' is not a decoded script object")),
    }
    Ok(out)
}

fn disasm_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let (Some(file), Some(path)) = (c.file, c.rest.first()) else {
        return usage_error("disasm needs a package file and an object path");
    };
    let (set, pi) = match load_target(&file, c.game_dir.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    match disassemble(&set, pi, path, c.tokens) {
        Ok(s) => {
            emit(&s);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Walks a class chain root-to-leaf applying each class's own defaults and prints the
/// resulting (inherited/effective) values. Read-only.
fn defaults_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(file) = c.file else {
        return usage_error("defaults needs a package file");
    };
    let Some(class_path) = c.rest.first() else {
        return usage_error("defaults needs a class path (Package.Class or Class)");
    };
    let (set, pi) = match load_target(&file, c.game_dir.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let p = &set.packages[pi];
    let path = class_path
        .split_once('.')
        .map(|(_, c)| c)
        .unwrap_or(class_path);
    let Some(e) = p.export_by_path(path) else {
        eprintln!("error: no class '{class_path}' in {}", p.name);
        return ExitCode::from(1);
    };
    let class = GlobalRef {
        package: pi,
        export: e,
    };
    let mut vm = xiii_script::Vm::new(&set, xiii_script::VmLimits::default());
    let layout = match vm.class_layout(class) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(1);
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {} (chain: {})",
        file.display(),
        set.path(class),
        layout.chain_names.join(" <- ")
    );
    let filter: Vec<String> = c.names.iter().map(|s| s.to_ascii_lowercase()).collect();
    // Layout defaults are ordered by slot; `slot_by_name` gives the first (most-derived is
    // inserted first, then overridden by root-to-leaf application in `class_layout`).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for slot in &layout.slots {
        if seen.contains(&slot.name) {
            continue;
        }
        seen.insert(slot.name.clone());
        if !filter.is_empty() && !filter.iter().any(|f| slot.name.contains(f)) {
            continue;
        }
        let value = layout
            .defaults
            .get(slot.base)
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|| "<missing>".into());
        let _ = writeln!(out, "  {} = {}", slot.name, value);
    }
    emit(&out);
    ExitCode::SUCCESS
}

fn param_text(n: &NativeEntry) -> String {
    n.params
        .iter()
        .map(|q| {
            format!(
                "{}{} {}",
                if q.is_return() { "return " } else { "" },
                q.type_name,
                q.name
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn natives_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(root) = c.file else {
        return usage_error("natives needs an installation root");
    };
    let (set, failures) = match load_install(&root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let cat = native_catalog(&set);
    if c.json {
        let v: Vec<Value> = cat
            .iter()
            .map(|n| {
                json!({
                    "package": n.package, "owner": n.owner, "name": n.name,
                    "friendly_name": n.friendly_name, "native_index": n.native_index,
                    "flags": function_flags::names(n.flags), "params": param_text(n),
                })
            })
            .collect();
        emit(&(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"));
    } else {
        let mut out = String::new();
        for n in &cat {
            let _ = writeln!(
                out,
                "{:>5} {:<52} [{}] ({})",
                n.native_index,
                n.path(),
                hex_flags(n.flags),
                param_text(n)
            );
        }
        let _ = writeln!(out, "{} native functions", cat.len());
        emit(&out);
    }
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Per-package coverage numbers.
#[derive(Debug, Clone, Default)]
pub struct PackageCoverage {
    /// Reflected exports attempted per kind.
    pub attempted: BTreeMap<String, u64>,
    /// Decoded per kind.
    pub decoded: BTreeMap<String, u64>,
    /// Failures (export path, error).
    pub failures: Vec<(String, String)>,
    /// Scripts (functions/states/classes/structs) with non-empty code.
    pub scripts_with_code: u64,
    /// Tokens.
    pub tokens: u64,
    /// Memory bytes of code.
    pub code_memory: u64,
    /// File bytes of code.
    pub code_file: u64,
    /// Class defaults decoded to the payload end.
    pub class_defaults_exact: u64,
    /// Default properties decoded.
    pub default_properties: u64,
}

fn kind_key(k: ScriptClassKind) -> &'static str {
    match k {
        ScriptClassKind::Function => "Function",
        ScriptClassKind::State => "State",
        ScriptClassKind::Class => "Class",
        ScriptClassKind::Struct => "Struct",
        ScriptClassKind::Const => "Const",
        ScriptClassKind::Enum => "Enum",
        ScriptClassKind::Property => "Property",
    }
}

/// Coverage of one loaded package.
pub fn package_coverage(p: &ScriptPackage) -> PackageCoverage {
    let mut c = PackageCoverage::default();
    for (i, e) in p.package.exports().iter().enumerate() {
        let class = p.package.export_class_path(i).unwrap_or("?");
        let Some(k) = script_class_kind(class) else {
            continue;
        };
        if e.serial_size == 0 {
            continue;
        }
        *c.attempted.entry(kind_key(k).to_owned()).or_default() += 1;
    }
    for o in p.objects.values() {
        let k = match o {
            ScriptObject::Function(_) => ScriptClassKind::Function,
            ScriptObject::State(_) => ScriptClassKind::State,
            ScriptObject::Class(cl) => {
                c.class_defaults_exact += 1;
                c.default_properties += cl.defaults.properties.len() as u64;
                ScriptClassKind::Class
            }
            ScriptObject::Struct(_) => ScriptClassKind::Struct,
            ScriptObject::Const(_) => ScriptClassKind::Const,
            ScriptObject::Enum(_) => ScriptClassKind::Enum,
            ScriptObject::Property(_) => ScriptClassKind::Property,
        };
        *c.decoded.entry(kind_key(k).to_owned()).or_default() += 1;
        if let Some(h) = o.struct_header()
            && h.script.memory_size > 0
        {
            c.scripts_with_code += 1;
            c.tokens += u64::from(h.script.token_count);
            c.code_memory += u64::from(h.script.memory_size);
            c.code_file += h.script.file_span.len() as u64;
        }
    }
    for (e, err) in &p.errors {
        c.failures
            .push((p.ref_path(ObjectRef::Export(*e)), err.to_string()));
    }
    c
}

/// `(script class, function) -> dll` for every `?exec` export of the DLLs in `dir`.
pub fn dll_exec_symbols(dir: &Path) -> std::io::Result<BTreeMap<(String, String), String>> {
    let mut out = BTreeMap::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
        .collect();
    entries.sort();
    for path in entries {
        let data = std::fs::read(&path)?;
        let Ok(names) = xiii_script::pe::export_names(&data) else {
            continue;
        };
        let dll = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for n in names {
            if let Some(k) = xiii_script::pe::parse_exec_symbol(&n) {
                out.entry(k).or_insert_with(|| dll.clone());
            }
        }
    }
    Ok(out)
}

fn native_name(set: &ScriptSet, index: u16) -> String {
    match set.native_functions(index) {
        [] => "<unregistered>".to_owned(),
        [g] => set.path(*g),
        many => many
            .iter()
            .map(|g| set.path(*g))
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

/// Builds the metadata-only coverage report.
pub fn coverage_report(
    set: &ScriptSet,
    failures: &[(String, String)],
    dll_symbols: Option<&BTreeMap<(String, String), String>>,
    label: &str,
) -> Value {
    let mut packages = serde_json::Map::new();
    let mut totals = PackageCoverage::default();
    let mut all = CallStats::default();
    for (pi, p) in set.packages.iter().enumerate() {
        let c = package_coverage(p);
        all.add_package(set, pi);
        for (k, v) in &c.attempted {
            *totals.attempted.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &c.decoded {
            *totals.decoded.entry(k.clone()).or_default() += v;
        }
        totals.failures.extend(c.failures.iter().cloned());
        totals.scripts_with_code += c.scripts_with_code;
        totals.tokens += c.tokens;
        totals.code_memory += c.code_memory;
        totals.code_file += c.code_file;
        totals.class_defaults_exact += c.class_defaults_exact;
        totals.default_properties += c.default_properties;
        packages.insert(
            p.name.clone(),
            json!({
                "licensee": p.package.summary().licensee,
                "attempted": c.attempted, "decoded": c.decoded,
                "failures": c.failures.len(),
                "first_failures": c.failures.iter().take(5).map(|(a,b)| format!("{a}: {b}")).collect::<Vec<_>>(),
                "scripts_with_code": c.scripts_with_code, "tokens": c.tokens,
                "code_memory_bytes": c.code_memory, "code_file_bytes": c.code_file,
                "class_defaults_exact": c.class_defaults_exact,
                "default_properties": c.default_properties,
            }),
        );
    }
    let opcodes: Vec<Value> = all
        .opcodes
        .iter()
        .filter(|(op, _)| **op < 0x60)
        .map(|(op, n)| json!({"opcode": format!("0x{op:02X}"), "name": opcode_name(*op), "count": n}))
        .collect();
    let native_tokens: u64 = all.native_indices.values().sum();

    // Native catalog.
    let cat = native_catalog(set);
    let indexed: BTreeSet<u16> = cat
        .iter()
        .filter(|n| n.native_index != 0)
        .map(|n| n.native_index)
        .collect();
    let dup: Vec<Value> = set
        .native_table()
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(i, v)| json!({"index": i, "functions": v.iter().map(|g| set.path(*g)).collect::<Vec<_>>()}))
        .collect();
    let mut by_pkg: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for n in &cat {
        let e = by_pkg.entry(n.package.clone()).or_default();
        e.0 += 1;
        if n.native_index != 0 {
            e.1 += 1;
        }
    }
    let dll = dll_symbols.map(|syms| {
        let keys: BTreeSet<(String, String)> = syms.keys().cloned().collect();
        let m = match_dll_symbols(&cat, &keys);
        let mut by_dll: BTreeMap<String, u64> = BTreeMap::new();
        for v in syms.values() {
            *by_dll.entry(v.clone()).or_default() += 1;
        }
        let unmatched_ops = cat
            .iter()
            .filter(|n| m.unmatched.contains(&n.path()) && n.flags & function_flags::OPERATOR != 0)
            .count();
        json!({
            "exec_symbols": syms.len(),
            "exec_symbols_by_dll": by_dll,
            "natives_matched": m.matched.len(),
            "natives_unmatched": m.unmatched.len(),
            "natives_unmatched_operators": unmatched_ops,
            "unmatched_non_operator_examples": m.unmatched.iter().filter(|p| {
                cat.iter().any(|n| &n.path() == *p && n.flags & function_flags::OPERATOR == 0)
            }).take(40).collect::<Vec<_>>(),
            "symbols_without_script_function": m.symbols_without_function.len(),
            "symbols_without_script_function_examples": m.symbols_without_function.iter().take(40).collect::<Vec<_>>(),
        })
    });

    // Game packages: native histograms and final native calls.
    let mut game = serde_json::Map::new();
    for g in GAME_PACKAGES {
        let Some(pi) = set.package_index(g) else {
            continue;
        };
        let mut s = CallStats::default();
        s.add_package(set, pi);
        let mut hist: Vec<(u16, u64)> = s.native_indices.iter().map(|(a, b)| (*a, *b)).collect();
        hist.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let finals_native: Vec<Value> = {
            let mut v: Vec<(&String, u64)> = s
                .final_calls
                .iter()
                .filter(|(_, (n, _))| *n)
                .map(|(k, (_, c))| (k, *c))
                .collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            v.into_iter()
                .map(|(k, c)| json!({"function": k, "count": c}))
                .collect()
        };
        let finals_script: u64 = s
            .final_calls
            .values()
            .filter(|(n, _)| !n)
            .map(|(_, c)| c)
            .sum();
        game.insert(
            (*g).to_owned(),
            json!({
                "tokens": s.tokens,
                "native_call_tokens": s.native_indices.values().sum::<u64>(),
                "distinct_native_indices": s.native_indices.len(),
                "native_index_histogram": hist.iter().map(|(i, c)| json!({"index": i, "function": native_name(set, *i), "count": c})).collect::<Vec<_>>(),
                "final_calls_to_native_functions": finals_native,
                "final_calls_to_script_functions": finals_script,
                "final_calls_unresolved": s.final_unresolved.values().sum::<u64>(),
                "virtual_calls": s.virtual_calls.values().sum::<u64>(),
                "distinct_virtual_names": s.virtual_calls.len(),
                "global_calls": s.global_calls.values().sum::<u64>(),
            }),
        );
    }
    let unregistered: Vec<u16> = all
        .native_indices
        .keys()
        .copied()
        .filter(|i| set.native_functions(*i).is_empty())
        .collect();
    json!({
        "schema": 1,
        "label": label,
        "tool": "xiii-tool script coverage (crates/xiii-script)",
        "method": "Every Core.Function/State/Class/Struct/Const/Enum/*Property export of every .u package decoded with the XIII v100/licensee-58 layout; bytecode decoded token by token until the in-memory script size is reached exactly; each payload must be consumed to its last byte. Metadata only: no bytecode bodies, strings or property values.",
        "references": {
            "uelib": "EliotVU/Unreal-Library@3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c (UStruct/UFunction/UState/UClass/UProperty and token deserializers)",
            "property_tags": "xiii-package (UModel a0bfb468 / UELib 3207a17e)",
        },
        "package_table_failures": failures.iter().map(|(a,b)| format!("{a}: {b}")).collect::<Vec<_>>(),
        "packages_loaded": set.packages.len(),
        "totals": {
            "attempted": totals.attempted, "decoded": totals.decoded,
            "failures": totals.failures.len(),
            "first_failures": totals.failures.iter().take(20).map(|(a,b)| format!("{a}: {b}")).collect::<Vec<_>>(),
            "scripts_with_code": totals.scripts_with_code,
            "tokens": totals.tokens,
            "code_memory_bytes": totals.code_memory, "code_file_bytes": totals.code_file,
            "class_defaults_exact": totals.class_defaults_exact,
            "default_properties": totals.default_properties,
            "unknown_tokens": 0,
        },
        "opcode_histogram": opcodes,
        "native_call_tokens": native_tokens,
        "native_indices_called_but_unregistered": unregistered,
        "native_catalog": {
            "native_functions": cat.len(),
            "with_native_index": cat.iter().filter(|n| n.native_index != 0).count(),
            "distinct_native_indices": indexed.len(),
            "bound_by_name": cat.iter().filter(|n| n.native_index == 0).count(),
            "latent": cat.iter().filter(|n| n.flags & function_flags::LATENT != 0).count(),
            "iterator": cat.iter().filter(|n| n.flags & function_flags::ITERATOR != 0).count(),
            "operators": cat.iter().filter(|n| n.flags & (function_flags::OPERATOR | function_flags::PRE_OPERATOR) != 0).count(),
            "events": cat.iter().filter(|n| n.flags & function_flags::EVENT != 0).count(),
            "duplicate_indices": dup,
            "by_package": by_pkg.iter().map(|(k,(a,b))| (k.clone(), json!({"natives": a, "indexed": b}))).collect::<serde_json::Map<_,_>>(),
            "dll_cross_reference": dll,
        },
        "game_packages": game,
        "packages": packages,
    })
}

fn coverage_cmd(args: &[String]) -> ExitCode {
    let c = match parse(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(root) = c.file else {
        return usage_error("coverage needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    if let Some(out) = &c.json_out {
        let abs_out = std::path::absolute(out).unwrap_or_else(|_| out.clone());
        let abs_root = std::path::absolute(&root).unwrap_or_else(|_| root.clone());
        if abs_out.starts_with(&abs_root) {
            eprintln!(
                "error: refusing to write {} inside the installation",
                out.display()
            );
            return ExitCode::from(2);
        }
    }
    let started = Instant::now();
    let (set, failures) = match load_install(&root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let dll_dir = c.dll_dir.clone().unwrap_or_else(|| root.join("system"));
    let dll = dll_exec_symbols(&dll_dir).ok();
    let report = coverage_report(&set, &failures, dll.as_ref(), &root.display().to_string());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "script coverage: {} ({:.2} s), {} packages",
        root.display(),
        started.elapsed().as_secs_f64(),
        set.packages.len()
    );
    let mut bad = !failures.is_empty();
    for p in &set.packages {
        let pc = package_coverage(p);
        let att: u64 = pc.attempted.values().sum();
        let dec: u64 = pc.decoded.values().sum();
        bad |= !pc.failures.is_empty();
        let _ = writeln!(
            out,
            "{:<14} reflected {:>6}/{:<6} functions {:>5} states {:>4} classes {:>4} (defaults exact {:>4}) code {:>8} B mem {:>8} B file tokens {:>7}",
            p.name,
            dec,
            att,
            pc.decoded.get("Function").copied().unwrap_or(0),
            pc.decoded.get("State").copied().unwrap_or(0),
            pc.decoded.get("Class").copied().unwrap_or(0),
            pc.class_defaults_exact,
            pc.code_memory,
            pc.code_file,
            pc.tokens
        );
        for (a, b) in pc.failures.iter().take(5) {
            let _ = writeln!(out, "  FAILED {a}: {b}");
        }
    }
    let cat = &report["native_catalog"];
    let _ = writeln!(
        out,
        "natives: {} functions, {} indexed ({} distinct), {} bound by name; native call tokens {}",
        cat["native_functions"],
        cat["with_native_index"],
        cat["distinct_native_indices"],
        cat["bound_by_name"],
        report["native_call_tokens"]
    );
    if let Some(d) = cat.get("dll_cross_reference").filter(|d| !d.is_null()) {
        let _ = writeln!(
            out,
            "dll ?exec symbols {}: natives matched {}, unmatched {} ({} operators), symbols without script function {}",
            d["exec_symbols"],
            d["natives_matched"],
            d["natives_unmatched"],
            d["natives_unmatched_operators"],
            d["symbols_without_script_function"]
        );
    }
    for g in GAME_PACKAGES {
        if let Some(v) = report["game_packages"].get(*g) {
            let _ = writeln!(
                out,
                "{g}: tokens {} native calls {} ({} distinct indices), final->native {} kinds, virtual calls {}",
                v["tokens"],
                v["native_call_tokens"],
                v["distinct_native_indices"],
                v["final_calls_to_native_functions"]
                    .as_array()
                    .map_or(0, Vec::len),
                v["virtual_calls"]
            );
        }
    }
    emit(&out);
    if let Some(path) = &c.json_out {
        let text = serde_json::to_string_pretty(&report).unwrap_or_default() + "\n";
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("error: cannot write {}: {e}", path.display());
            return ExitCode::from(2);
        }
    }
    if bad {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
