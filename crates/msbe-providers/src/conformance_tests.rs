//! The codec conformance suite, run against every codec this build ships.

use std::path::Path;

use msbe_provider_api::{PackCodec, conformance};
use msbe_wasm_codec::WasmPackCodec;

use crate::{Providers, RegistryError};

const MRPACK: &str = "../../extensions/codecs/modrinth-mrpack/conformance";

/// Checks `codec` against the golden transcript of the suite in `directory`, first rewriting the
/// transcript when `MSBE_BLESS` is set.
fn check_golden(codec: &dyn PackCodec, directory: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(directory);
    let suite = std::fs::read_to_string(directory.join("cases.json")).unwrap();
    let golden = directory.join("transcript.json");
    #[expect(
        clippy::disallowed_methods,
        reason = "a test switch that rewrites golden transcripts, not configuration"
    )]
    let bless = std::env::var_os("MSBE_BLESS").is_some();
    if bless {
        std::fs::write(&golden, conformance::transcript(codec, &suite).unwrap()).unwrap();
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|error| panic!("{}: {error}; run with MSBE_BLESS=1", golden.display()));
    if let Err(error) = conformance::check(codec, &suite, &expected) {
        panic!("{error}");
    }
}

#[test]
fn every_shipped_codec_keeps_the_rules_every_codec_must() -> Result<(), RegistryError> {
    let providers = Providers::builtins()?;
    for descriptor in providers.pack_codecs() {
        let codec = providers.pack_codec(&descriptor.id)?;
        if let Err(error) = conformance::check_invariants(codec) {
            panic!("{error}");
        }
    }
    let example = WasmPackCodec::load(include_bytes!(
        "../../msbe-wasm-codec/tests/fixtures/pack-list.wasm"
    ))?;
    if let Err(error) = conformance::check_invariants(&example) {
        panic!("{error}");
    }
    Ok(())
}

#[test]
fn the_modrinth_pack_codec_reproduces_its_golden_transcript() -> Result<(), RegistryError> {
    let providers = Providers::builtins()?;
    check_golden(providers.pack_codec("modrinth-mrpack")?, MRPACK);
    Ok(())
}
