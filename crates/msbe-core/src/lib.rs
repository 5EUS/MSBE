//! Plan engine, solver and the resolve/apply pipeline.
//!
//! Knows nothing about any game. If a game name, store name, loader or file format
//! appears in this crate, that is a design bug: the test is that this crate compiles
//! and passes its suite with zero plans installed.
//!
//! See `docs/00-overview.md` and `docs/02-plan-system.md`.
