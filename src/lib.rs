//! railgun-broadcaster-monitor: read-only dashboard for RAILGUN broadcaster operators.
//!
//! The binary is `rbm`; the library exists so integration tests can drive the pipeline.

pub mod app;
pub mod config;
pub mod db;
pub mod enrich;
pub mod ingest;
pub mod market;
pub mod parse;
pub mod redact;
pub mod rpc;
pub mod sources;
pub mod units;
pub mod waku;
pub mod web;
