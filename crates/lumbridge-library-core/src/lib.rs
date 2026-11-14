//! lumbridge-library-core: the engines of Lumbridge Library (Lumbridge Docs + Lumbridge Warehouse) and
//! what the daemon `lumbridge-libraryd` builds on.
//!
//! - [`config`]: one TOML file plus env overrides; secrets come from env files, never the TOML.
//! - [`identity`]: the owner, the agent authors and the daemon's git identity, from the config.
//! - [`models`]: the embed + rerank endpoints (OpenAI-compatible), with the `query:`/`passage:`
//!   prefixes and a fingerprint so vectors from different models are never mixed.
//! - [`db`]: Postgres pools, embedded migrations, embed-model registration, halfvec literals.
//! - [`status`]: the counts both shelves report.
//! - [`rank`]: RRF fusion, rerank blending and token budgets shared by both shelves.
//! - [`ledger`]: lib.qa answers + verdicts, evals, hashed dataset exports for training.
//! - [`docs`]: Lumbridge Docs engine (`lumbridge-docs`). [`warehouse`]: Lumbridge Warehouse engine (`lumbridge-warehouse`).
//! - [`legacy`]: names earlier releases used that are still read, and the fixed wire names.

pub mod config;
pub mod db;
pub mod docs;
pub mod identity;
pub mod ledger;
pub mod librarian;
pub mod llm;
pub mod models;
pub mod rank;
pub mod status;
pub mod warehouse;

pub use lumbridge_library_legacy as legacy;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
