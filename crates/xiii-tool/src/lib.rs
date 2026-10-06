//! Library half of `xiii-tool`: package reports, read-only corpus scanning/comparison,
//! property decoding reports, property coverage and map dependency checks.
//!
//! Installation directories are only ever read. Nothing here writes files.

pub mod campaign_cmd;
pub mod corpus;
pub mod coverage;
pub mod deps;
pub mod props;
pub mod report;
pub mod script_cmd;
pub mod script_run;
pub mod video_cmd;
pub mod world_cmd;
