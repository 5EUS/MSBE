//! Content-addressed store, materialization backends and the transactional journal.
//!
//! The only crate that writes to the game directory. Every mutation is journalled
//! before it happens, so a crash mid-deploy is a resumable state rather than a
//! corrupted install.
//!
//! See `docs/04-deployment-engine.md`.
