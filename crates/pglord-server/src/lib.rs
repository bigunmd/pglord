//! pglord-server library surface: exposed so integration tests and the binary
//! share the exact same composition seam.

pub mod api;
pub mod app;
pub mod config;
pub mod error;
pub mod health;
pub mod kube_client;
pub mod orchestrator;
pub mod primary_record;
pub mod reconciler;
pub mod spec_store;
pub mod ui_assets;
