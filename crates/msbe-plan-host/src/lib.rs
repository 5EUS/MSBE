//! Wasmtime host and capability enforcement for plan extensions.
//!
//! Extensions have no filesystem, network, process or clock. Their only effect on the
//! world is `emit-op`, and every emitted operation is re-validated here against the
//! plan's declared capabilities before it reaches the applier.
//!
//! See `docs/02-plan-system.md` §2.5.
