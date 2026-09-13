//! Modrinth, as MSBE ships it.
//!
//! None of Modrinth's protocol is native code. `program.toml` describes its API in the reviewed
//! `catalog-v1` vocabulary: target-filtered search, projects, releases, the project a release
//! belongs to, and update checks through its bulk hash lookups, which keep each mod on its release
//! channel. This crate packages that program with the overlay entries MSBE ships about Modrinth
//! projects and Modrinth's sandboxed `.mrpack` codec, built from `extensions/codecs/modrinth-mrpack`
//! (`docs/06-providers-and-policy.md` §6.4). Its test feature serves an in-memory Modrinth.

#[cfg(feature = "test")]
pub mod cli_test_support;

use msbe_provider_api::{ProgramRegistration, WasmPackCodecRegistration};

/// How Modrinth joins MSBE.
pub const PROGRAM: ProgramRegistration = ProgramRegistration {
    program: include_str!("../program.toml"),
    version: env!("CARGO_PKG_VERSION"),
    overlay: &[
        include_str!("../overlays/Aqlf1Shp.toml"),
        include_str!("../overlays/qvIfYCYJ.toml"),
    ],
    wasm_pack_codecs: &[WasmPackCodecRegistration {
        id: "modrinth-mrpack",
        // The version of extensions/codecs/modrinth-mrpack.
        version: "1.0.0",
        module: include_bytes!("../codecs/modrinth-mrpack.wasm"),
    }],
};
