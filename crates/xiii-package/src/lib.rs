//! Bounded reader for XIII Classic package files (`.u`, `.unr`, `.utx`, `.usx`, `.uax`, `.ukx`).
//!
//! Receives bytes and explicit [`Limits`]; never touches the filesystem. Only the measured
//! dialect (file version 100; licensee 50/56/57/58) is accepted. This crate reads the package
//! summary, GUID/generations, name/import/export tables and validates the object graph
//! (reference ranges, outer cycles/depth, export payload bounds). The [`object`] module slices
//! export payloads and decodes the state-frame prefix and UE1/UE2 tagged properties; the
//! class-native data after the properties (meshes, textures, bytecode, ...) is not decoded.
//!
//! ```
//! # fn demo(bytes: &[u8]) -> Result<(), xiii_package::PackageError> {
//! let package = xiii_package::Package::parse(bytes, &xiii_package::Limits::default())?;
//! for (i, export) in package.exports().iter().enumerate() {
//!     let path = package.object_path(xiii_package::ObjectRef::Export(i as u32));
//!     let class = package.export_class_path(i);
//!     println!("{path:?} {class:?} {} bytes", export.serial_size);
//! }
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod cursor;
mod error;
pub mod object;
mod package;

pub use cursor::Cursor;
pub use error::{ErrorKind, PackageError, Result, Table};
pub use object::{
    ObjectProperties, Property, PropertyBlock, PropertyType, PropertyValue, RF_HAS_STACK,
    RawReason, StateFrame, StructValue,
};
pub use package::{
    Export, Generation, Import, Limits, NULL_CLASS_PATH, NameEntry, NameIndex, ObjectRef,
    PACKAGE_TAG, Package, SUPPORTED_VERSION, Span, Summary, TableLocation, TableSpans,
};

/// True when `prefix` starts with the Unreal package tag. Use on the first bytes of a file to
/// decide whether to parse it.
pub fn has_package_tag(prefix: &[u8]) -> bool {
    prefix.len() >= 4 && prefix[..4] == PACKAGE_TAG.to_le_bytes()
}

#[cfg(test)]
mod object_tests;
#[cfg(test)]
mod tests;
