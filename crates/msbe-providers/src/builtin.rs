//! What this build ships: its native provider exceptions and its provider programs.
//!
//! This is the one file outside `extensions/` that names a provider; the architecture guard
//! (`scripts/development/check-architecture.sh`) refuses a name anywhere else in the crates.

use msbe_provider_api::{ProgramRegistration, Registration, WasmPackCodecRegistration};

/// The native adapters MSBE ships, each a reviewed exception to provider programs.
pub const BUILTIN: &[Registration] = &[msbe_provider_local::REGISTRATION];

/// The providers MSBE ships as declarative programs. A new one is a program, its data, and one line
/// here.
pub const BUILTIN_PROGRAMS: &[ProgramRegistration] = &[DIRECT_URL, MODRINTH, THUNDERSTORE];

const DIRECT_URL: ProgramRegistration = ProgramRegistration {
    program: DIRECT_PROGRAM,
    version: env!("CARGO_PKG_VERSION"),
    overlay: &[],
    wasm_pack_codecs: &[],
};

pub(crate) const MODRINTH: ProgramRegistration = ProgramRegistration {
    program: include_str!("../../../extensions/providers/modrinth/program.toml"),
    version: env!("CARGO_PKG_VERSION"),
    overlay: &[
        include_str!("../../../extensions/providers/modrinth/overlays/Aqlf1Shp.toml"),
        include_str!("../../../extensions/providers/modrinth/overlays/qvIfYCYJ.toml"),
    ],
    wasm_pack_codecs: &[WasmPackCodecRegistration {
        id: "modrinth-mrpack",
        version: "1.0.0",
        module: include_bytes!("../../../extensions/providers/modrinth/modrinth-mrpack.wasm"),
    }],
};

const THUNDERSTORE: ProgramRegistration = ProgramRegistration {
    program: include_str!("../../../extensions/providers/thunderstore/program.toml"),
    version: env!("CARGO_PKG_VERSION"),
    overlay: &[],
    wasm_pack_codecs: &[],
};

const DIRECT_PROGRAM: &str = r#"
runtime = "direct-url-v1"

[provider]
schema = 1
id = "url"
name = "Direct URL"

[provider.source]
type = "https_url"

[provider.acquisition]
type = "direct_https"

[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = ""
ack_required = false
"#;
