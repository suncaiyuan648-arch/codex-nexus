//! Public analytics entry points. SQL/aggregation stays in the repository;
//! this module is the stable boundary used by Tauri commands.

pub use super::category_usage::app_category_usage;
pub use super::repository::{app_legacy_query, app_query};
