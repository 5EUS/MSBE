//! The RPC contract.
//!
//! Single source of truth for the daemon protocol. Generates Rust server traits, C#
//! DTOs with a source-generated `JsonSerializerContext`, and a JSON Schema for
//! third-party clients and contract tests.
//!
//! See `docs/03-architecture.md` §3.4.
