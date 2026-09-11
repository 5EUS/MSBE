//! Hardened archive extraction.
//!
//! The only crate permitted to read archive input. Every entry is canonicalised
//! against the target root before use; zip-slip, absolute paths, symlink escape,
//! decompression bombs, case collisions and reserved names are rejected here so no
//! other crate has to think about them.
//!
//! See `docs/11-security.md` §11.2.
#![forbid(unsafe_code)]
