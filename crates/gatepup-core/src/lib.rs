//! Application use cases. `gatepup-core` orchestrates the domain crates;
//! transport adapters (`gatepup-cli`, `gatepup-admin`) call into it and never
//! reach across into the domain crates directly.

mod app;
mod error;

pub use app::{load_validated, print_config, serve};
pub use error::CoreError;
