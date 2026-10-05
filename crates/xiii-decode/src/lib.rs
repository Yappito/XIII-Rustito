//! Payload decoders producing normalized, renderer-independent asset data.
//!
//! Module ownership during the M2 spikes: `common`, `texture`, `static_mesh`, `model`, `terrain`
//! (world/collision, M2a) and `skeletal` (skeleton/animation, M2b). Decoders take a parsed
//! `xiii_package::Package` and an export; they never touch the filesystem.

pub mod common;
pub mod font;
pub mod model;
pub mod skeletal;
pub mod static_mesh;
pub mod static_mesh_instance;
pub mod terrain;
pub mod texture;
