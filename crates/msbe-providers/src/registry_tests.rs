//! The registry's tests: manifests, programs, codecs and policy, against in-memory adapters and
//! the providers this build ships.

use std::{io::Write, path::Path};

use msbe_core::{config::Home, instance::NativeExtensionIdentity};
use msbe_plan_schema::Side;
use msbe_provider_api::{
    Adapter, AdapterError, ContainerKind, ExtensionCapability, ExtensionEnvelope, ExtensionProvide,
    HostApiRange, HttpClient, HttpError, PackCodec, PackCodecDescriptor, PackCodecError,
    PackCodecRegistration, PackDirections, PackEntry, PackExportContext, PackExportPlan,
    PackImportContext, PackImportPlan, PackInput, PackLayout, PackOptionSchema, PackOptions,
    PackProbe, PackageId, ProgramError, Provider, ProviderProgram, ProviderProgramEnvelope,
    Registration, SigningKey, SupportSet, Target, model::Request,
};
use serde_json::json;

use super::{ExtensionTrust, ProgramTrust, Providers, RegistryError, Routed};

struct SearchHttp;

impl HttpClient for SearchHttp {
    fn get(&self, _: &str, _: &[(&str, &str)], _: u64) -> Result<Vec<u8>, HttpError> {
        Ok(serde_json::to_vec(&json!({ "hits": [{
            "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
            "description": "A rendering engine", "downloads": 42,
            "client_side": "required", "server_side": "required"
        }] }))
        .unwrap())
    }

    fn post_json(&self, _: &str, _: &[u8], _: u64) -> Result<Vec<u8>, HttpError> {
        unreachable!("search never posts")
    }

    fn download(&self, _: &str, _: &mut dyn Write, _: u64) -> Result<u64, HttpError> {
        unreachable!("search never downloads")
    }
}

const EXAMPLE: &str = r#"
    schema = 1
    id = "example"
    name = "Example"
    [source]
    type = "prefixed"
    prefix = "example:"
    [acquisition]
    type = "direct_https"
    [policy]
    requires_auth = false
    respects_distribution_flag = false
    tos_url = ""
    ack_required = false
"#;

#[derive(Debug)]
struct ExampleAdapter {
    id: &'static str,
}

impl Adapter for ExampleAdapter {
    fn id(&self) -> &str {
        self.id
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        Ok(Request::Project {
            reference: reference.to_owned(),
            version: None,
        })
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builder must implement the fallible registration function pointer"
)]
fn build_example(_: &Provider) -> Result<Box<dyn Adapter>, msbe_provider_api::ManifestError> {
    Ok(Box::new(ExampleAdapter { id: "example" }))
}

#[derive(Debug)]
struct FakeCodec {
    descriptor: PackCodecDescriptor,
    confidence: u8,
}

impl PackCodec for FakeCodec {
    fn descriptor(&self) -> &PackCodecDescriptor {
        &self.descriptor
    }

    fn probe(&self, _: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
        Ok(PackProbe {
            confidence: self.confidence,
            reason: None,
        })
    }

    fn plan_import(
        &self,
        _: &dyn PackInput,
        _: &PackImportContext,
        _: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        Err(PackCodecError::UnsupportedDirection("import"))
    }

    fn plan_export(
        &self,
        _: &PackExportContext<'_>,
        _: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError> {
        Err(PackCodecError::UnsupportedDirection("export"))
    }

    fn layout(&self, _: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
        Err(PackCodecError::UnsupportedDirection("export"))
    }
}

#[derive(Debug)]
struct EmptyInput;

impl PackInput for EmptyInput {
    fn container(&self) -> ContainerKind {
        ContainerKind::File
    }
    fn entries(&self) -> &[PackEntry] {
        &[]
    }
    fn read(&self, _: &msbe_fsops::RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
        Err(PackCodecError::FormatMismatch)
    }
}

fn fake_codec(id: &str, extension: &str, confidence: u8) -> Box<dyn PackCodec> {
    Box::new(FakeCodec {
        descriptor: PackCodecDescriptor {
            id: id.to_owned(),
            provider: Some("example".to_owned()),
            name: id.to_owned(),
            extensions: vec![extension.to_owned()],
            media_types: Vec::new(),
            directions: PackDirections {
                import: true,
                export: false,
            },
            supported_games: SupportSet::Universal,
            option_schema: PackOptionSchema {
                schema: 1,
                presets: Vec::new(),
                fields: Vec::new(),
                constraints: Vec::new(),
            },
        },
        confidence,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builder must implement the fallible codec function pointer"
)]
fn build_alpha() -> Result<Box<dyn PackCodec>, PackCodecError> {
    Ok(fake_codec("alpha", "alpha", 80))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builder must implement the fallible codec function pointer"
)]
fn build_beta() -> Result<Box<dyn PackCodec>, PackCodecError> {
    Ok(fake_codec("beta", "beta", 80))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builder must implement the fallible codec function pointer"
)]
fn build_duplicate_hint() -> Result<Box<dyn PackCodec>, PackCodecError> {
    Ok(fake_codec("beta", "alpha", 70))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builder must implement the fallible codec function pointer"
)]
fn build_mismatch() -> Result<Box<dyn PackCodec>, PackCodecError> {
    Ok(fake_codec("other", "other", 90))
}

const ALPHA: PackCodecRegistration = PackCodecRegistration {
    id: "alpha",
    identity: NativeExtensionIdentity {
        id: "alpha",
        version: "1.0.0",
        host_api_minimum: 1,
        host_api_maximum: 1,
        signer: "msbe-build",
    },
    build: build_alpha,
};
const BETA: PackCodecRegistration = PackCodecRegistration {
    id: "beta",
    identity: NativeExtensionIdentity {
        id: "beta",
        version: "1.0.0",
        host_api_minimum: 1,
        host_api_maximum: 1,
        signer: "msbe-build",
    },
    build: build_beta,
};
const DUPLICATE_HINT: PackCodecRegistration = PackCodecRegistration {
    id: "beta",
    identity: NativeExtensionIdentity {
        id: "beta",
        version: "1.0.0",
        host_api_minimum: 1,
        host_api_maximum: 1,
        signer: "msbe-build",
    },
    build: build_duplicate_hint,
};
const MISMATCH: PackCodecRegistration = PackCodecRegistration {
    id: "registered",
    identity: NativeExtensionIdentity {
        id: "registered",
        version: "1.0.0",
        host_api_minimum: 1,
        host_api_maximum: 1,
        signer: "msbe-build",
    },
    build: build_mismatch,
};

fn registration(codecs: &'static [PackCodecRegistration]) -> Registration {
    Registration {
        id: "example",
        identity: NativeExtensionIdentity {
            id: "example",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        manifest: EXAMPLE,
        overlay: &[],
        build: build_example,
        pack_codecs: codecs,
        wasm_pack_codecs: &[],
        exception_reason: "Test adapter.",
    }
}

fn program(body: &str) -> String {
    let payload = body.replacen("[program]", "", 1).replace("[program.", "[");
    let program: ProviderProgram = toml::from_str(&payload).unwrap();
    let mut envelope = ExtensionEnvelope {
        schema: 1,
        package_digest: ProviderProgramEnvelope::digest_for(&program).unwrap(),
        id: "test-provider".to_owned(),
        version: "1.0.0".to_owned(),
        provides: vec![ExtensionProvide::ProviderProgramV1],
        host_api: HostApiRange {
            minimum: 1,
            maximum: 1,
        },
        capabilities: vec![ExtensionCapability::Network],
        signer: "test-root".to_owned(),
        signature: "00".repeat(64),
        payload: program,
    };
    envelope.sign(&test_signing_key()).unwrap();
    toml::to_string(&ProviderProgramEnvelope(envelope)).unwrap()
}

fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn trust() -> ProgramTrust {
    ProgramTrust {
        trusted_keys: [("test-root".to_owned(), test_signing_key().verifying_key())].into(),
        revoked_signers: std::collections::BTreeSet::default(),
        revoked_digests: std::collections::BTreeSet::default(),
    }
}

const PACK_LIST: &[u8] = include_bytes!("../../msbe-wasm-codec/tests/fixtures/pack-list.wasm");

/// A WASM codec module whose descriptor names `provider`.
fn wasm_codec(id: &str, provider: Option<&str>) -> Vec<u8> {
    let descriptor = json!({ "ok": {
        "id": id, "provider": provider, "name": id, "extensions": [id], "media_types": [],
        "directions": { "import": true, "export": false },
        "supported_games": { "kind": "universal" }, "option_schema": { "schema": 1 }
    }})
    .to_string();
    let escaped = descriptor.replace('\\', "\\\\").replace('"', "\\\"");
    wat::parse_str(format!(
        r#"(module (memory (export "memory") 1) (data (i32.const 0) "{escaped}")
            (func (export "msbe_abi_version") (result i32) i32.const 1)
            (func (export "msbe_descriptor") (result i64) i64.const {}))"#,
        descriptor.len()
    ))
    .unwrap()
}

fn signed_codec(id: &str, module: Vec<u8>) -> ExtensionEnvelope<Vec<u8>> {
    let mut envelope = ExtensionEnvelope {
        schema: 1,
        package_digest: ExtensionEnvelope::package_digest_for(&module).unwrap(),
        id: id.to_owned(),
        version: "1.0.0".to_owned(),
        provides: vec![ExtensionProvide::PackCodecV1],
        host_api: HostApiRange {
            minimum: 1,
            maximum: 1,
        },
        capabilities: Vec::new(),
        signer: "test-root".to_owned(),
        signature: "00".repeat(64),
        payload: module,
    };
    envelope.sign(&test_signing_key()).unwrap();
    envelope
}

/// Installs `envelope` in the data directory `home`, as an envelope document beside its module.
fn install(home: &Path, name: &str, envelope: &ExtensionEnvelope<Vec<u8>>) {
    let codecs = home.join("extensions/codecs");
    std::fs::create_dir_all(&codecs).unwrap();
    std::fs::write(codecs.join(format!("{name}.wasm")), &envelope.payload).unwrap();
    std::fs::write(
        codecs.join(format!("{name}.toml")),
        format!(
            "schema = 1\nid = \"{}\"\nversion = \"{}\"\nmodule = \"{name}.wasm\"\npackage_digest = \"{}\"\nprovides = [\"pack-codec-v1\"]\nhost_api = {{ minimum = 1, maximum = 1 }}\nsigner = \"{}\"\nsignature = \"{}\"\n",
            envelope.id,
            envelope.version,
            envelope.package_digest,
            envelope.signer,
            envelope.signature
        ),
    )
    .unwrap();
}

fn trust_root(home: &Path, providers: &str) {
    let key = msbe_provider_api::hex(test_signing_key().verifying_key().as_bytes());
    std::fs::create_dir_all(home.join("extensions")).unwrap();
    std::fs::write(
        home.join("extensions/trust.toml"),
        format!("[[signer]]\nid = \"test-root\"\nkey = \"{key}\"\nproviders = [{providers}]\n"),
    )
    .unwrap();
}

#[test]
fn installed_codecs_load_from_the_data_directory_under_its_trust_root() -> Result<(), RegistryError>
{
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path());
    assert!(
        Providers::installed(&home)?
            .pack_codec("pack-list")
            .is_err()
    );

    install(
        dir.path(),
        "pack-list",
        &signed_codec("pack-list", PACK_LIST.to_vec()),
    );
    assert!(
        matches!(
            Providers::installed(&home),
            Err(RegistryError::InstalledCodec { .. })
        ),
        "a codec from an untrusted signer is refused"
    );

    trust_root(dir.path(), "");
    let providers = Providers::installed(&home)?;
    assert!(
        providers
            .pack_codecs()
            .iter()
            .any(|descriptor| descriptor.id == "pack-list")
    );
    assert_eq!(
        providers.extension_pins(),
        Providers::builtins()?.extension_pins(),
        "installed codecs are not native build pins"
    );

    std::fs::write(
        dir.path().join("extensions/codecs/escape.toml"),
        "schema = 1\nid = \"escape\"\nversion = \"1\"\nmodule = \"../escape.wasm\"\npackage_digest = \"\"\nprovides = []\nhost_api = { minimum = 1, maximum = 1 }\nsigner = \"test-root\"\nsignature = \"\"\n",
    )
    .unwrap();
    assert!(matches!(
        Providers::installed(&home),
        Err(RegistryError::InstalledExtension { .. })
    ));
    Ok(())
}

#[test]
fn provider_bound_wasm_codecs_need_a_signer_granted_that_provider() -> Result<(), RegistryError> {
    let key = test_signing_key().verifying_key();
    let bound = signed_codec("bound", wasm_codec("bound", Some("example")));
    let mut providers = Providers::new(&[registration(&[])], &[])?;

    let neutral_only = ExtensionTrust::default().with_signer("test-root", key, Vec::new());
    assert!(matches!(
        providers.register_wasm_codec(&bound, &neutral_only),
        Err(RegistryError::WasmCodecProviderNotGranted { provider, .. }) if provider == "example"
    ));

    let granted = ExtensionTrust::default().with_signer(
        "test-root",
        key,
        ["example".to_owned(), "nobody".to_owned()],
    );
    let orphan = signed_codec("orphan", wasm_codec("orphan", Some("nobody")));
    assert!(matches!(
        providers.register_wasm_codec(&orphan, &granted),
        Err(RegistryError::Manifest(_))
    ));

    providers.register_wasm_codec(&bound, &granted)?;
    assert!(providers.pack_codec("bound").is_ok());
    assert!(matches!(
        providers.register_wasm_codec(&bound, &granted),
        Err(RegistryError::DuplicateCodec(id)) if id == "bound"
    ));
    Ok(())
}

#[test]
fn builtins_route_sources_only_through_their_reviewed_adapters() -> Result<(), RegistryError> {
    let providers = Providers::builtins()?;
    let Routed { provider, request } = providers.request("modrinth:sodium")?;
    assert_eq!(provider, "modrinth");
    assert_eq!(
        request,
        Request::Project {
            reference: "sodium".to_owned(),
            version: None
        }
    );
    let url = providers.request("https://example.test/mod.jar")?;
    assert_eq!(url.provider, "url");
    assert!(matches!(url.request, Request::File(_)));
    assert_eq!(providers.searchable(), ["modrinth"]);
    Ok(())
}

#[test]
fn builtins_load_the_overlay_their_adapters_ship() -> Result<(), RegistryError> {
    let fabric_api = PackageId {
        provider: "modrinth".to_owned(),
        project: "P7dR8mSH".to_owned(),
    };
    assert_eq!(
        Providers::builtins()?
            .overlay()
            .suppliers(&fabric_api)
            .count(),
        2
    );
    Ok(())
}

#[test]
fn builtins_pin_the_programs_and_modules_they_ship_as_part_of_the_build()
-> Result<(), RegistryError> {
    let pins = Providers::builtins()?.extension_pins();
    for id in ["url", "modrinth", "modrinth-mrpack", "local", "msbe-native"] {
        assert!(
            pins.iter()
                .any(|pin| pin.id == id && pin.signer == "msbe-build"),
            "{id} is not pinned: {pins:?}"
        );
    }
    Ok(())
}

#[test]
fn declared_provider_without_an_adapter_is_rejected() {
    let providers = Providers::new(&[], &[EXAMPLE]).unwrap();
    assert!(matches!(
        providers.request("example:mod"),
        Err(RegistryError::UnavailableAdapter(id)) if id == "example"
    ));
}

#[test]
fn acknowledgement_required_providers_are_blocked_before_use() {
    let manifest = EXAMPLE.replace("ack_required = false", "ack_required = true");
    let providers = Providers::new(&[], &[&manifest]).unwrap();
    assert!(matches!(
        providers.request("example:mod"),
        Err(RegistryError::AcknowledgementRequired { .. })
    ));
}

#[test]
fn authentication_required_providers_are_blocked_before_use() {
    let manifest = EXAMPLE.replace("requires_auth = false", "requires_auth = true");
    let providers = Providers::new(&[], &[&manifest]).unwrap();
    assert!(matches!(
        providers.request("example:mod"),
        Err(RegistryError::AuthenticationRequired(id)) if id == "example"
    ));
}

#[test]
fn native_registrations_must_explain_their_exception() {
    let mut registration = registration(&[]);
    registration.exception_reason = " ";
    assert!(matches!(
        Providers::new(&[registration], &[]),
        Err(RegistryError::MissingNativeException(id)) if id == "example"
    ));
}

#[test]
fn native_extension_pins_must_match_the_running_build() -> Result<(), RegistryError> {
    let providers = Providers::new(&[registration(&[])], &[])?;
    let pins = providers.extension_pins();
    assert!(providers.accepts_extension_pins(&pins));

    let mut mismatched = pins;
    let Some(pin) = mismatched.first_mut() else {
        panic!("the native registration pins at least one extension");
    };
    pin.version = "2.0.0".to_owned();
    assert!(!providers.accepts_extension_pins(&mismatched));
    assert!(providers.accepts_extension_pins(&[]));
    Ok(())
}

#[test]
fn programs_fail_closed_for_unknown_signers_and_revocations() {
    let document = program(
        r#"
        [program]
        runtime = "direct-url-v1"
        [program.provider]
        schema = 1
        id = "program-url"
        name = "Program URL"
        [program.provider.source]
        type = "https_url"
        [program.provider.acquisition]
        type = "direct_https"
        [program.provider.policy]
        requires_auth = false
        respects_distribution_flag = false
        tos_url = ""
        ack_required = false
    "#,
    );
    assert!(matches!(
        Providers::new_with_programs(&[], &[], &[&document], &ProgramTrust::default()),
        Err(RegistryError::Program(ProgramError::Envelope(_)))
    ));
    let digest = ProviderProgramEnvelope::from_toml(&document)
        .unwrap()
        .0
        .package_digest;
    let mut revoked = trust();
    revoked.revoked_digests.insert(digest);
    assert!(matches!(
        Providers::new_with_programs(&[], &[], &[&document], &revoked),
        Err(RegistryError::RevokedProgram(_))
    ));
}

#[test]
fn trusted_direct_program_routes_and_parses_a_pinned_url() -> Result<(), RegistryError> {
    let document = program(
        r#"
        [program]
        runtime = "direct-url-v1"
        [program.provider]
        schema = 1
        id = "program-url"
        name = "Program URL"
        [program.provider.source]
        type = "https_url"
        [program.provider.acquisition]
        type = "direct_https"
        [program.provider.policy]
        requires_auth = false
        respects_distribution_flag = false
        tos_url = ""
        ack_required = false
    "#,
    );
    let providers = Providers::new_with_programs(&[], &[], &[&document], &trust())?;
    let routed = providers.request(&format!(
        "https://example.test/mod.jar#sha256={}",
        "a".repeat(64)
    ))?;
    assert_eq!(routed.provider, "program-url");
    assert!(matches!(routed.request, Request::File(_)));
    Ok(())
}

#[test]
fn trusted_catalog_program_maps_a_bounded_search_fixture() -> Result<(), RegistryError> {
    let document = program(
        r#"
        [program]
        runtime = "catalog-v1"
        capabilities = ["search"]
        [program.games]
        game = "game"
        [program.provider]
        schema = 1
        id = "catalog"
        name = "Catalog"
        [program.provider.source]
        type = "prefixed"
        prefix = "catalog:"
        [program.provider.metadata]
        api_base = "https://api.example.test"
        [program.provider.acquisition]
        type = "direct_https"
        [program.provider.policy]
        requires_auth = false
        respects_distribution_flag = false
        tos_url = ""
        ack_required = false
        [program.routes]
        search = "/search"
        project = "/project/{reference}"
        releases = "/project/{project}/releases"
        [program.mappings]
        search_items = "/hits"
        [program.mappings.project]
        id = "/project_id"
        title = "/title"
        slug = "/slug"
        description = "/description"
        downloads = "/downloads"
        [program.mappings.release]
        id = "/id"
        number = "/number"
        published = "/published"
        files = "/files"
    "#,
    );
    let providers = Providers::new_with_programs(&[], &[], &[&document], &trust())?;
    let target = Target {
        game: "game".to_owned(),
        edition: None,
        storefront: None,
        loader: "loader".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: Some("1.0".to_owned()),
        side: Side::Client,
    };
    let hits = providers.search("catalog", &SearchHttp, "test", &target, 5)?;
    assert_eq!(hits.first().map(|hit| hit.title.as_str()), Some("Sodium"));
    Ok(())
}

#[test]
fn codecs_are_validated_looked_up_and_detected() -> Result<(), RegistryError> {
    let providers = Providers::new(&[registration(&[ALPHA])], &[])?;
    assert_eq!(
        providers
            .pack_codecs()
            .first()
            .map(|codec| codec.id.as_str()),
        Some("alpha")
    );
    assert_eq!(providers.pack_codec("alpha")?.descriptor().id, "alpha");
    assert_eq!(
        providers
            .detect_pack_codec(&EmptyInput)?
            .map(|codec| codec.descriptor().id.as_str()),
        Some("alpha")
    );
    assert!(matches!(
        providers.pack_codec("missing"),
        Err(RegistryError::UnknownCodec(id)) if id == "missing"
    ));
    Ok(())
}

#[test]
fn codec_registration_rejects_mismatches_and_duplicate_hints() {
    assert!(matches!(
        Providers::new(&[registration(&[MISMATCH])], &[]),
        Err(RegistryError::MismatchedCodec { .. })
    ));
    assert!(matches!(
        Providers::new(&[registration(&[ALPHA, DUPLICATE_HINT])], &[]),
        Err(RegistryError::DuplicateCodecHint { .. })
    ));
    assert!(matches!(
        Providers::new(&[registration(&[ALPHA, ALPHA])], &[]),
        Err(RegistryError::DuplicateCodec(id)) if id == "alpha"
    ));
}

#[test]
fn codec_policy_and_ambiguous_detection_fail_closed() {
    let manifest = EXAMPLE.replace("requires_auth = false", "requires_auth = true");
    let mut blocked = registration(&[ALPHA]);
    blocked.manifest = Box::leak(manifest.into_boxed_str());
    let providers = Providers::new(&[blocked], &[]).unwrap();
    assert!(matches!(
        providers.pack_codec("alpha"),
        Err(RegistryError::AuthenticationRequired(id)) if id == "example"
    ));

    let providers = Providers::new(&[registration(&[ALPHA, BETA])], &[]).unwrap();
    assert!(matches!(
        providers.detect_pack_codec(&EmptyInput),
        Err(RegistryError::AmbiguousCodec(ids)) if ids == ["alpha", "beta"]
    ));
}

#[test]
fn plain_http_reaches_the_direct_adapter_security_error() {
    let providers = Providers::builtins().unwrap();
    match providers.request("http://example.test/mod.jar") {
        Err(RegistryError::Adapter(AdapterError::Specific(error))) => {
            assert!(error.to_string().contains("insecure URL"), "{error}");
        }
        other => panic!("expected the direct adapter's refusal, got {other:?}"),
    }
}

#[test]
fn search_returns_provider_neutral_records() -> Result<(), RegistryError> {
    let results = Providers::builtins()?.search(
        "modrinth",
        &SearchHttp,
        "rendering",
        &Target {
            game: "minecraft".to_owned(),
            edition: None,
            storefront: None,
            loader: "fabric".to_owned(),
            provides: Vec::new(),
            loader_version: None,
            game_version: Some("1.21.1".to_owned()),
            side: Side::Client,
        },
        10,
    )?;
    let [result] = results.as_slice() else {
        panic!("expected one result, got {results:?}");
    };
    assert_eq!(result.provider, "modrinth");
    assert_eq!(result.project, "AANobbMI");
    assert_eq!(result.reference, "sodium");
    Ok(())
}
