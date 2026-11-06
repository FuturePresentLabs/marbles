//! Marbles: a small issue tracker for fleets of coding agents.
//!
//! The thesis (full argument in the README): a swarm does not need a distributed,
//! merge-replicated work database — it needs one owner per lineage. Dolt, git-backed JSONL, and
//! per-clone embedded stores all spend their complexity on letting *every* writer keep a copy.
//! Marbles refuses that bargain: one server, one database, one claim race at a time; agents and
//! humans talk HTTP with an identity, and nothing they do locally can fork the truth.
//!
//! The interface — command names, JSON shapes, short suffix ids, the `alfalfa:stage:<s>` style
//! label discipline — deliberately mirrors [Beads](https://github.com/gastownhall/beads), whose
//! design made this shape worth copying. Attribution lives in the README, not the comments, but
//! the debt is real.

pub mod adapter;
#[cfg(not(target_arch = "wasm32"))]
pub mod api;
#[cfg(not(target_arch = "wasm32"))]
pub mod assertion_cache;
#[cfg(not(target_arch = "wasm32"))]
pub mod auth;
#[cfg(not(target_arch = "wasm32"))]
pub mod cache;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod config;
#[cfg(not(target_arch = "wasm32"))]
pub mod db;
#[cfg(not(target_arch = "wasm32"))]
pub mod identity;
#[cfg(not(target_arch = "wasm32"))]
pub mod import;
#[cfg(not(target_arch = "wasm32"))]
pub mod jsonl;
#[cfg(not(target_arch = "wasm32"))]
pub mod oidc;
#[cfg(not(target_arch = "wasm32"))]
pub mod plan;
#[cfg(not(target_arch = "wasm32"))]
pub mod postgres_migration;
#[cfg(not(target_arch = "wasm32"))]
pub mod postgres_store;
pub mod records;
#[cfg(not(target_arch = "wasm32"))]
pub mod refs;
#[cfg(not(target_arch = "wasm32"))]
pub mod setup;
#[cfg(all(feature = "soil", target_arch = "wasm32"))]
pub mod soil_engine;
pub mod store;
pub mod time;
pub mod types;
#[cfg(not(target_arch = "wasm32"))]
pub mod webhook;

#[cfg(not(target_arch = "wasm32"))]
pub use db::Db;
pub use store::Error;

#[cfg(not(target_arch = "wasm32"))]
pub mod report;
