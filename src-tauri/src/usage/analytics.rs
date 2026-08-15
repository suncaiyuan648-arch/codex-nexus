//! Public analytics entry points. SQL/aggregation stays in the repository;
//! this module is the stable boundary used by Tauri commands.

pub use super::repository::{app_daily_model_usage, app_legacy_query, app_query};
