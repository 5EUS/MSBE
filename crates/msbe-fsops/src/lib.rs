//! Content-addressed store, materialization backends and the transactional journal.
//!
//! The only crate that writes to the game directory. Every mutation is journalled
//! before it happens, so a crash mid-deploy is a resumable state rather than a
//! corrupted install.
//!
//! The pieces, bottom-up:
//!
//! - [`Store`]: one content-addressed shard per volume. Blobs are read-only and named
//!   by their [`Digest`].
//! - [`Capabilities`]: a real probe for reflink and hardlink support between a shard and
//!   an instance root, because filesystem type is not a reliable proxy.
//! - [`Journal`]: an append-only write-ahead log, flushed before every mutation.
//! - [`Applier`]: executes [`Operation`]s against an instance root. A failed or
//!   interrupted transaction is left open and undone by [`Applier::recover`], so a crash
//!   and an error take the same path.
//!
//! See `docs/04-deployment-engine.md`.

mod sys;

pub mod applier;
pub mod atomic;
pub mod digest;
pub mod error;
pub mod journal;
pub mod materialize;
pub mod ops;
pub mod probe;
pub mod relpath;
pub mod store;

#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod test_support;

pub use applier::{Applier, Checkpoint, NoopObserver, Observer, TxnReport};
pub use digest::Digest;
pub use error::{Error, Result};
pub use journal::{Journal, Record, TxnId};
pub use materialize::Backend;
pub use ops::{Operation, Prior};
pub use probe::Capabilities;
pub use relpath::RelPath;
pub use store::Store;
