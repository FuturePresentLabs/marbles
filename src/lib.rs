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

pub mod api;
pub mod auth;
pub mod client;
pub mod config;
pub mod db;
pub mod import;
pub mod jsonl;
pub mod oidc;
pub mod setup;
pub mod time;
pub mod types;
pub mod webhook;

pub use db::{Db, Error};
