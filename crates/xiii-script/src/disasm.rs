//! Readable disassembly of decoded scripts.
//!
//! Output derived from proprietary packages must stay local (see the project guidelines);
//! these functions only format what the decoder produced.

use std::fmt::Write as _;

use xiii_package::ObjectRef;

use crate::bytecode::{Call, Script, Token, TokenKind};
use crate::linker::ScriptSet;
use crate::reflect::ScriptObject;

/// Formatting context: the set and the package the script was decoded from.
pub struct Disasm<'s> {
    set: &'s ScriptSet,
    from: usize,
}

impl<'s> Disasm<'s> {
    /// New formatter for scripts of package `from`.
    pub fn new(set: &'s ScriptSet, from: usize) -> Self {
        Self { set, from }
    }

    fn pkg(&self) -> &'s crate::linker::ScriptPackage {
        &self.set.packages[self.from]
    }

    fn obj_name(&self, r: ObjectRef) -> String {
        self.pkg().ref_name(r).to_owned()
    }

    fn obj_path(&self, r: ObjectRef) -> String {
        self.pkg().ref_path(r)
    }

    fn name(&self, i: u32) -> String {
        self.pkg().name_text(i).to_owned()
    }

    /// Describes a native index: `#idx Package.Class.Function "sym"`.
    pub fn native_label(&self, index: u16) -> String {
        match self.set.native_functions(index) {
            [] => format!("native#{index}<unregistered>"),
            [g, ..] => {
                let p = &self.set.packages[g.package];
                let friendly = match p.objects.get(&g.export) {
                    Some(ScriptObject::Function(f)) => p.name_text(f.header.friendly_name),
                    _ => "",
                };
                let path = self.set.path(*g);
                let short = path.rsplit('.').next().unwrap_or(&path).to_owned();
                if !friendly.is_empty() && friendly != short {
                    format!("native#{index} {path} \"{friendly}\"")
                } else {
                    format!("native#{index} {path}")
                }
            }
        }
    }

    fn args(&self, c: &Call) -> String {
        c.args
            .iter()
            .map(|a| self.expr(a))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A member expression inside a context: instance variables print without `self.`.
    fn member(&self, t: &Token) -> String {
        match &t.kind {
            TokenKind::InstanceVariable(r) => self.obj_name(*r),
            TokenKind::BoolVariable(e) => self.member(e),
            _ => self.expr(t),
        }
    }

    /// One expression as pseudo-source.
    pub fn expr(&self, t: &Token) -> String {
        use TokenKind as K;
        match &t.kind {
            K::LocalVariable(r) => self.obj_name(*r),
            K::InstanceVariable(r) => format!("self.{}", self.obj_name(*r)),
            K::DefaultVariable(r) => format!("default.{}", self.obj_name(*r)),
            K::NativeParm(r) => format!("nativeparm {}", self.obj_name(*r)),
            K::Return(e) => match e.kind {
                K::Nothing => "return".to_owned(),
                _ => format!("return {}", self.expr(e)),
            },
            K::Switch { size, expr } => format!("switch[{size}] ({})", self.expr(expr)),
            K::Jump { target } => format!("goto 0x{target:04X}"),
            K::JumpIfNot { target, cond } => {
                format!("if !({}) goto 0x{target:04X}", self.expr(cond))
            }
            K::Stop => "stop".to_owned(),
            K::Assert { line, cond } => format!("assert({}) /*line {line}*/", self.expr(cond)),
            K::Case { target, value } => match value {
                Some(v) => format!("case {}: /*next 0x{target:04X}*/", self.expr(v)),
                None => "default:".to_owned(),
            },
            K::Nothing => "nothing".to_owned(),
            K::LabelTable { labels, .. } => {
                let l: Vec<String> = labels
                    .iter()
                    .map(|l| format!("{}@0x{:04X}", self.name(l.name), l.offset))
                    .collect();
                format!("labeltable [{}]", l.join(", "))
            }
            K::GotoLabel(e) => format!("gotolabel {}", self.expr(e)),
            K::EatString(e) => format!("eatstring {}", self.expr(e)),
            K::Let { lhs, rhs } | K::LetBool { lhs, rhs } | K::LetDelegate { lhs, rhs } => {
                format!("{} = {}", self.expr(lhs), self.expr(rhs))
            }
            K::DynArrayElement { index, array } | K::ArrayElement { index, array } => {
                format!("{}[{}]", self.expr(array), self.expr(index))
            }
            K::New {
                outer,
                name,
                flags,
                class,
            } => format!(
                "new({}, {}, {}) {}",
                self.expr(outer),
                self.expr(name),
                self.expr(flags),
                self.expr(class)
            ),
            K::ClassContext(c) => format!(
                "{}.static.{} /*skip {} size {}*/",
                self.expr(&c.object),
                self.member(&c.member),
                c.skip,
                c.size
            ),
            K::Context(c) => format!("{}.{}", self.expr(&c.object), self.member(&c.member)),
            K::MetaCast { class, expr } => {
                format!("class<{}>({})", self.obj_name(*class), self.expr(expr))
            }
            K::EndFunctionParms => "endparms".to_owned(),
            K::SelfRef => "self".to_owned(),
            K::Skip { skip, expr } => format!("{} /*skip {skip}*/", self.expr(expr)),
            K::VirtualFunction { name, call } => {
                format!("{}({})", self.name(*name), self.args(call))
            }
            K::FinalFunction { function, call } => {
                let mark = match self.set.resolve(self.from, *function) {
                    Some(g) => match self.set.object(g) {
                        Some(ScriptObject::Function(f)) if f.is_native() => "final native ",
                        Some(_) => "final ",
                        None => "final ",
                    },
                    None => "final? ",
                };
                format!("{mark}{}({})", self.obj_path(*function), self.args(call))
            }
            K::GlobalFunction { name, call } => {
                format!("global.{}({})", self.name(*name), self.args(call))
            }
            K::DelegateFunction {
                property,
                name,
                call,
            } => format!(
                "delegate {}:{}({})",
                self.obj_name(*property),
                self.name(*name),
                self.args(call)
            ),
            K::NativeCall { index, call } => {
                format!("{}({})", self.native_label(*index), self.args(call))
            }
            K::IntConst(v) => v.to_string(),
            K::FloatConst(v) => format!("{v:?}"),
            K::StringConst(b) => {
                let s: String = b.iter().map(|&c| char::from(c)).collect();
                format!("{s:?}")
            }
            K::UnicodeStringConst(u) => format!("{:?}", String::from_utf16_lossy(u)),
            K::ObjectConst(r) => format!("obj'{}'", self.obj_path(*r)),
            K::NameConst(n) => format!("'{}'", self.name(*n)),
            K::RotationConst([p, y, r]) => format!("rot({p},{y},{r})"),
            K::VectorConst([x, y, z]) => format!("vect({x:?},{y:?},{z:?})"),
            K::ByteConst(b) => format!("{b}b"),
            K::IntConstByte(b) => format!("{b}"),
            K::IntZero => "0".to_owned(),
            K::IntOne => "1".to_owned(),
            K::True => "true".to_owned(),
            K::False => "false".to_owned(),
            K::NoObject => "None".to_owned(),
            K::BoolVariable(e) => self.expr(e),
            K::DynamicCast { class, expr } => {
                format!("{}({})", self.obj_name(*class), self.expr(expr))
            }
            K::Iterator { expr, end } => {
                format!("foreach {} /*end 0x{end:04X}*/", self.expr(expr))
            }
            K::IteratorPop => "iteratorpop".to_owned(),
            K::IteratorNext => "iteratornext".to_owned(),
            K::StructCmpEq { strukt, a, b } => format!(
                "({} == {}) /*struct {}*/",
                self.expr(a),
                self.expr(b),
                self.obj_name(*strukt)
            ),
            K::StructCmpNe { strukt, a, b } => format!(
                "({} != {}) /*struct {}*/",
                self.expr(a),
                self.expr(b),
                self.obj_name(*strukt)
            ),
            K::StructMember { property, expr } => {
                format!("{}.{}", self.expr(expr), self.obj_name(*property))
            }
            K::DynArrayLength(e) => format!("{}.length", self.expr(e)),
            K::PrimitiveCast { cast, expr } => {
                format!("cast#0x{cast:02X}({})", self.expr(expr))
            }
            K::DelegateCompare { a, b, .. } => format!(
                "{}({}, {})",
                crate::bytecode::opcode_name(t.opcode),
                self.expr(a),
                self.expr(b)
            ),
            K::EmptyDelegate => "emptydelegate".to_owned(),
            K::DynArrayInsert {
                array,
                index,
                count,
            } => format!(
                "{}.insert({}, {})",
                self.expr(array),
                self.expr(index),
                self.expr(count)
            ),
            K::DynArrayRemove {
                array,
                index,
                count,
            } => format!(
                "{}.remove({}, {})",
                self.expr(array),
                self.expr(index),
                self.expr(count)
            ),
            K::DebugInfo {
                version, line, op, ..
            } => format!("debuginfo v{version} line {line} op {op}"),
            K::DelegateProperty(n) => format!("delegateprop {}", self.name(*n)),
            K::Conditional { cond, a, b, .. } => {
                format!(
                    "({} ? {} : {})",
                    self.expr(cond),
                    self.expr(a),
                    self.expr(b)
                )
            }
        }
    }

    /// Statement listing: `offset: pseudo-source`, plus label markers.
    pub fn listing(&self, script: &Script) -> String {
        let labels = script.labels();
        let mut out = String::new();
        for s in &script.statements {
            for l in labels.iter().filter(|l| l.offset == s.offset) {
                let _ = writeln!(out, "{}:", self.name(l.name));
            }
            let _ = writeln!(out, "  {:04X}: {}", s.offset, self.expr(s));
        }
        out
    }

    /// Full token tree: memory offset, file offset, memory size, mnemonic.
    pub fn token_tree(&self, script: &Script) -> String {
        fn rec(d: &Disasm<'_>, t: &Token, depth: usize, out: &mut String) {
            let detail = match &t.kind {
                TokenKind::NativeCall { index, .. } => d.native_label(*index),
                TokenKind::LocalVariable(r)
                | TokenKind::InstanceVariable(r)
                | TokenKind::DefaultVariable(r)
                | TokenKind::NativeParm(r)
                | TokenKind::ObjectConst(r) => d.obj_path(*r),
                TokenKind::FinalFunction { function, .. } => d.obj_path(*function),
                TokenKind::VirtualFunction { name, .. }
                | TokenKind::GlobalFunction { name, .. }
                | TokenKind::NameConst(name) => d.name(*name),
                TokenKind::Jump { target } => format!("-> 0x{target:04X}"),
                TokenKind::JumpIfNot { target, .. } => format!("-> 0x{target:04X}"),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "{:04X} @{:<8} +{:<4} {}{:02X} {} {}",
                t.offset,
                t.file_offset,
                t.memory_size,
                "  ".repeat(depth),
                t.opcode,
                t.mnemonic(),
                detail
            );
            for c in t.children() {
                rec(d, c, depth + 1, out);
            }
        }
        let mut out = String::new();
        for s in &script.statements {
            rec(self, s, 0, &mut out);
        }
        out
    }
}
