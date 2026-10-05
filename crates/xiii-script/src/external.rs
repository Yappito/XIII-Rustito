//! Host data for object references into packages outside the loaded script set.
//!
//! `xiii-script` is filesystem-free and has no decoders. When script reads a
//! property of an object that lives in a `.utx`/`.uax`/... package (for example
//! `XIIIBaseHud.FondMsg.USize`, where `FondMsg` is a `Texture` in
//! `XIIIMenu.utx`), the host can supply the value through
//! [`ExternalObjectData`]. Without a provider the access stays the explicit
//! [`crate::vm::VmErrorKind::UnsupportedValue`] error it already was, never a
//! silent zero.

use crate::value::Value;

/// Host resolver for a property of an object in a non-script package.
pub trait ExternalObjectData {
    /// `path` is the `Package.Outer.Object` path as the referencing package spells it, and
    /// `property` is the lowercase property name. `None` = the host does not know the value
    /// (the VM keeps its explicit unsupported error).
    fn property(&self, path: &str, property: &str) -> Option<Value>;
}
