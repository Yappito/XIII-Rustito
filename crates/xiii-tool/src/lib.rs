//! Library half of `xiii-tool`: package reports and read-only corpus scanning/comparison.
//!
//! Installation directories are only ever read. Nothing here writes files.

pub mod corpus;
pub mod coverage;
pub mod deps;
pub mod props;
pub mod report;
