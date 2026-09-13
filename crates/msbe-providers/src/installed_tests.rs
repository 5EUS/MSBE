//! Provider programs installed in the data directory: served under the local trust root, each
//! admitted or refused on its own, pinned by their real signer, and never replacing what MSBE
//! ships.

use std::{cell::RefCell, collections::BTreeMap, io::Write, path::Path};

use msbe_core::config::Home;
use msbe_fsops::Digest;
use msbe_plan_schema::Side;
use msbe_provider_api::{
    ExtensionCapability, ExtensionEnvelope, ExtensionProvide, HostApiRange, HttpClient, HttpError,
    HttpRequest, HttpResponse, ProviderProgram, ProviderProgramEnvelope, SigningKey, Target, hex,
};
use serde_json::json;

use crate::{ExtensionKind, ExtensionStatus, Providers, RegistryError, installed};

/// A catalog no build ships.
const PROGRAM: &str = r#"
runtime = "catalog-v1"
capabilities = ["project", "releases"]

[games]
game = "game"

[provider]
schema = 1
id = "installed"
name = "Installed catalog"
[provider.source]
type = "prefixed"
prefix = "installed:"
[provider.metadata]
api_base = "https://api.installed.test"
[provider.acquisition]
type = "direct_https"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = ""
ack_required = false

[routes]
project = "/projects/{reference}"
releases = "/projects/{project}/releases"

[releases]
order = "newest-first"

[mappings.project]
id = "/id"
title = "/title"

[mappings.release]
id = "/id"
number = "/number"
published = "/published"
files = { single = "" }

[mappings.release.file]
url = "/url"
name = "/name"
"#;

/// A browser-assisted catalog whose links use the `handoff` scheme.
const ASSISTED: &str = r#"
runtime = "catalog-v1"

[games]
game = "game"

[provider]
schema = 1
id = "assisted-a"
name = "Assisted"
[provider.source]
type = "prefixed"
prefix = "assisted-a:"
[provider.metadata]
api_base = "https://api.assisted.test"
[provider.acquisition]
type = "browser_assisted"
scheme = "handoff"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = ""
ack_required = false

[handoff]
host = "game"
path = ["files", "{project}", "{release}"]
redeem = "/links/{project}/{release}"

[mappings.handoff]
urls = "/url"
"#;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// [`PROGRAM`] as provider `id`, with the source prefix `<id>:`.
fn variant(id: &str) -> String {
    PROGRAM
        .replace("\"installed\"", &format!("\"{id}\""))
        .replace("installed:", &format!("{id}:"))
}

fn digest(program: &str) -> String {
    ProviderProgramEnvelope::digest_for(&toml::from_str(program).unwrap()).unwrap()
}

/// `program` in an envelope signed as `signer` with the key from `seed`.
fn envelope(program: &str, signer: &str, seed: u8) -> String {
    let payload: ProviderProgram = toml::from_str(program).unwrap();
    let mut envelope = ExtensionEnvelope {
        schema: 1,
        package_digest: ProviderProgramEnvelope::digest_for(&payload).unwrap(),
        id: payload.provider.id.clone(),
        version: "0.1.0".to_owned(),
        provides: vec![ExtensionProvide::ProviderProgramV1],
        host_api: HostApiRange {
            minimum: 1,
            maximum: 1,
        },
        capabilities: vec![ExtensionCapability::Network],
        signer: signer.to_owned(),
        signature: "00".repeat(64),
        payload,
    };
    envelope.sign(&key(seed)).unwrap();
    toml::to_string(&ProviderProgramEnvelope(envelope)).unwrap()
}

/// A trust entry for the key from `seed` as `id`, granted `programs` and codecs for `providers`.
fn signer(id: &str, seed: u8, programs: &[&str], providers: &[&str]) -> String {
    let list = |ids: &[&str]| {
        ids.iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "[[signer]]\nid = \"{id}\"\nkey = \"{}\"\nprograms = [{}]\nproviders = [{}]\n\n",
        hex(key(seed).verifying_key().as_bytes()),
        list(programs),
        list(providers)
    )
}

/// A data directory whose trust root is `trust`, with `programs` installed by file name.
fn home(dir: &Path, trust: &str, programs: &[(&str, &str)]) -> Home {
    let providers = dir.join("extensions/providers");
    std::fs::create_dir_all(&providers).unwrap();
    std::fs::write(dir.join("extensions/trust.toml"), trust).unwrap();
    for (name, document) in programs {
        std::fs::write(providers.join(name), document).unwrap();
    }
    Home::at(dir)
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// Answers [`PROGRAM`]'s routes for project `tool`.
#[derive(Default)]
struct Catalog {
    requests: RefCell<Vec<String>>,
}

impl HttpClient for Catalog {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        self.requests.borrow_mut().push(request.url.to_owned());
        let body = match request.url {
            "https://api.installed.test/projects/tool" => json!({ "id": "tool", "title": "Tool" }),
            "https://api.installed.test/projects/tool/releases" => json!([{
                "id": "r1", "number": "1.0.0", "published": "2026-09-01",
                "url": "https://files.installed.test/tool.zip", "name": "tool.zip"
            }]),
            other => {
                return Err(HttpError::Status {
                    url: other.to_owned(),
                    status: 404,
                });
            }
        };
        Ok(serde_json::to_vec(&body).unwrap().into())
    }

    fn download(&self, request: &HttpRequest<'_>, _: &mut dyn Write) -> Result<u64, HttpError> {
        panic!("listing never downloads: {}", request.url)
    }
}

fn target() -> Target {
    Target {
        game: "game".to_owned(),
        edition: None,
        storefront: None,
        loader: "loader".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: None,
        side: Side::Client,
    }
}

#[test]
fn a_granted_signed_program_serves_requests_in_a_build_that_does_not_ship_it()
-> Result<(), RegistryError> {
    assert!(Providers::builtins()?.adapter("installed").is_err());
    let dir = tempfile::tempdir().unwrap();
    let home = home(
        dir.path(),
        &signer("publisher", 5, &["installed"], &[]),
        &[("installed.toml", &envelope(PROGRAM, "publisher", 5))],
    );

    let providers = Providers::installed(&home)?;
    assert_eq!(providers.request("installed:tool")?.provider, "installed");
    let http = Catalog::default();
    let releases = providers
        .adapter("installed")?
        .as_releases()
        .unwrap()
        .releases(&http, "tool", &target())?;
    assert_eq!(
        releases
            .iter()
            .map(|release| release.id.as_str())
            .collect::<Vec<_>>(),
        ["r1"]
    );
    assert_eq!(
        http.requests.borrow().as_slice(),
        ["https://api.installed.test/projects/tool/releases"]
    );

    let [listed] = providers.installed_extensions() else {
        panic!("{:?}", providers.installed_extensions());
    };
    assert_eq!(
        (
            listed.kind,
            listed.status,
            listed.id.as_deref(),
            listed.version.as_deref(),
            listed.signer.as_deref(),
            listed.reason.as_deref(),
        ),
        (
            ExtensionKind::Program,
            ExtensionStatus::Active,
            Some("installed"),
            Some("0.1.0"),
            Some("publisher"),
            None
        )
    );
    assert_eq!(listed.digest.as_deref(), Some(digest(PROGRAM).as_str()));
    assert_eq!(
        providers.extension_pins(),
        Providers::builtins()?.extension_pins(),
        "an installed program is not a build pin"
    );
    Ok(())
}

/// A data directory beneath `dir` with two admissible programs, and one refused for each reason a
/// program can be.
fn refusal_fixture(dir: &Path) -> Home {
    let revoked_digest = variant("revoked-digest");
    let tampered = envelope(&variant("tampered"), "publisher", 5)
        .replace("Installed catalog", "Tampered catalog");
    let granted = [
        "installed",
        "revoked-digest",
        "url",
        "prefixed",
        "assisted-a",
        "assisted-b",
        "named",
        "tampered",
    ];
    let trust = format!(
        "{}{}[revoked]\nsigners = [\"old\"]\ndigests = [\"{}\"]\n",
        signer("publisher", 5, &granted, &[]),
        signer("old", 6, &["revoked-signer"], &[]),
        digest(&revoked_digest)
    );
    let signed = |program: &str| envelope(program, "publisher", 5);
    home(
        dir,
        &trust,
        &[
            ("installed.toml", &signed(PROGRAM)),
            (
                "untrusted.toml",
                &envelope(&variant("untrusted"), "stranger", 9),
            ),
            (
                "revoked-signer.toml",
                &envelope(&variant("revoked-signer"), "old", 6),
            ),
            ("revoked-digest.toml", &signed(&revoked_digest)),
            ("ungranted.toml", &signed(&variant("ungranted"))),
            ("tampered.toml", &tampered),
            ("url.toml", &signed(&variant("url"))),
            (
                "prefixed.toml",
                &signed(&PROGRAM.replace("\"installed\"", "\"prefixed\"")),
            ),
            ("assisted-a.toml", &signed(ASSISTED)),
            (
                "assisted-b.toml",
                &signed(&ASSISTED.replace("assisted-a", "assisted-b")),
            ),
            ("other-name.toml", &signed(&variant("named"))),
            ("broken.toml", "not an envelope"),
        ],
    )
}

#[test]
fn refused_programs_are_skipped_with_their_reason_and_never_stop_the_others()
-> Result<(), RegistryError> {
    let dir = tempfile::tempdir().unwrap();
    let home = refusal_fixture(dir.path());

    let providers = Providers::installed(&home)?;
    let found: BTreeMap<String, (ExtensionStatus, String)> = providers
        .installed_extensions()
        .iter()
        .map(|listed| {
            (
                file_name(&listed.path),
                (listed.status, listed.reason.clone().unwrap_or_default()),
            )
        })
        .collect();
    assert_eq!(found.len(), 12, "{found:?}");
    for (file, refusal) in [
        ("installed.toml", None),
        ("assisted-a.toml", None),
        ("untrusted.toml", Some("stranger")),
        ("revoked-signer.toml", Some("revoked")),
        ("revoked-digest.toml", Some("revoked")),
        ("ungranted.toml", Some("does not grant")),
        ("tampered.toml", Some("digest")),
        ("url.toml", Some("duplicate provider id")),
        ("prefixed.toml", Some("overlap")),
        ("assisted-b.toml", Some("both claim handoff://")),
        ("other-name.toml", Some("must be installed as named.toml")),
        ("broken.toml", Some("")),
    ] {
        let (status, reason) = found.get(file).unwrap();
        match refusal {
            None => assert_eq!(*status, ExtensionStatus::Active, "{file}: {reason}"),
            Some(words) => {
                assert_eq!(*status, ExtensionStatus::Refused, "{file}");
                assert!(
                    !reason.is_empty() && reason.contains(words),
                    "{file}: {reason}"
                );
            }
        }
    }

    assert_eq!(providers.request("installed:tool")?.provider, "installed");
    assert_eq!(
        providers.request("https://example.test/tool.zip")?.provider,
        "url",
        "the provider an installed program collided with still serves"
    );
    assert!(providers.adapter("ungranted").is_err());
    assert!(providers.handoff("handoff://game/files/1/2").is_ok());
    Ok(())
}

#[test]
fn an_unreadable_trust_root_trusts_nothing_and_the_shipped_providers_still_serve()
-> Result<(), RegistryError> {
    let dir = tempfile::tempdir().unwrap();
    let home = home(
        dir.path(),
        "[[signer]]\nid = \"publisher\"\n",
        &[("installed.toml", &envelope(PROGRAM, "publisher", 5))],
    );
    let providers = Providers::installed(&home)?;
    let listed: Vec<(ExtensionKind, ExtensionStatus)> = providers
        .installed_extensions()
        .iter()
        .map(|listed| (listed.kind, listed.status))
        .collect();
    assert_eq!(
        listed,
        [
            (ExtensionKind::TrustRoot, ExtensionStatus::Refused),
            (ExtensionKind::Program, ExtensionStatus::Refused),
        ]
    );
    assert!(providers.adapter("installed").is_err());
    assert_eq!(
        providers.request("https://example.test/tool.zip")?.provider,
        "url"
    );
    Ok(())
}

#[test]
fn signed_programs_are_pinned_by_their_signer_only_for_content_that_uses_them()
-> Result<(), RegistryError> {
    let dir = tempfile::tempdir().unwrap();
    let home = home(
        dir.path(),
        &signer("publisher", 5, &["installed"], &[]),
        &[("installed.toml", &envelope(PROGRAM, "publisher", 5))],
    );
    let providers = Providers::installed(&home)?;
    let build = providers.extension_pins();
    assert_eq!(providers.extension_pins_for(["url"]), build);

    let mut locked = providers.extension_pins_for(["installed", "url"]);
    locked.sort_by(|left, right| left.id.cmp(&right.id));
    let signed: Vec<_> = locked
        .iter()
        .filter(|pin| pin.signer != "msbe-build")
        .collect();
    let [pin] = signed.as_slice() else {
        panic!("{locked:?}");
    };
    assert_eq!(
        (pin.id.as_str(), pin.version.as_str(), pin.signer.as_str()),
        ("installed", "0.1.0", "publisher")
    );
    assert_eq!(
        pin.digest,
        format!("sha256:{}", digest(PROGRAM))
            .parse::<Digest>()
            .unwrap()
    );

    providers.check_extension_pins(&locked)?;
    providers.check_extension_pins(&[])?;
    assert!(matches!(
        Providers::builtins()?.check_extension_pins(&locked),
        Err(RegistryError::MissingExtension { id, signer, .. }) if id == "installed" && signer == "publisher"
    ));
    let mut different = locked.clone();
    for pin in &mut different {
        if pin.id == "installed" {
            pin.digest = Digest::of_bytes(b"other content");
        }
    }
    assert!(matches!(
        providers.check_extension_pins(&different),
        Err(RegistryError::MissingExtension { .. })
    ));
    let without_build: Vec<_> = locked
        .iter()
        .filter(|pin| pin.signer != "msbe-build")
        .cloned()
        .collect();
    assert!(matches!(
        providers.check_extension_pins(&without_build),
        Err(RegistryError::ExtensionPinsDiffer)
    ));
    Ok(())
}

/// A pack codec module whose descriptor binds it to the provider `installed`.
fn bound_codec() -> Vec<u8> {
    let descriptor = json!({ "ok": {
        "id": "bound", "provider": "installed", "name": "Bound", "extensions": ["bound"],
        "media_types": [], "directions": { "import": true, "export": false },
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

#[test]
fn a_codec_can_bind_to_a_provider_an_installed_program_introduces() -> Result<(), RegistryError> {
    let dir = tempfile::tempdir().unwrap();
    let module = bound_codec();
    let mut codec = ExtensionEnvelope {
        schema: 1,
        package_digest: ExtensionEnvelope::package_digest_for(&module).unwrap(),
        id: "bound".to_owned(),
        version: "1.0.0".to_owned(),
        provides: vec![ExtensionProvide::PackCodecV1],
        host_api: HostApiRange {
            minimum: 1,
            maximum: 1,
        },
        capabilities: Vec::new(),
        signer: "publisher".to_owned(),
        signature: "00".repeat(64),
        payload: module,
    };
    codec.sign(&key(5)).unwrap();
    let codecs = dir.path().join("extensions/codecs");
    std::fs::create_dir_all(&codecs).unwrap();
    std::fs::write(codecs.join("bound.wasm"), &codec.payload).unwrap();
    std::fs::write(
        codecs.join("bound.toml"),
        installed::envelope_document(&codec, "bound.wasm").unwrap(),
    )
    .unwrap();

    let trust = signer("publisher", 5, &["installed"], &["installed"]);
    let without_program = Providers::installed(&home(dir.path(), &trust, &[]))?;
    assert!(without_program.pack_codec("bound").is_err());

    let home = home(
        dir.path(),
        &trust,
        &[("installed.toml", &envelope(PROGRAM, "publisher", 5))],
    );
    let providers = Providers::installed(&home)?;
    assert!(
        providers
            .pack_codecs()
            .iter()
            .any(|descriptor| descriptor.id == "bound"),
        "{:?}",
        providers.installed_extensions()
    );
    Ok(())
}
