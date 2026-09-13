//! Thunderstore's shipped provider program, run through the reviewed catalog runtime against
//! responses in the shape Thunderstore's experimental package API returns.

use std::{cell::RefCell, collections::BTreeMap, io::Write};

use msbe_plan_schema::Side;
use msbe_provider_api::{
    HttpClient, HttpError, HttpRequest, HttpResponse, Method, Overlay, Provenance, Target,
    UpdateCheck,
    model::{Download, Request},
    resolve::{Only, ProjectRequest, Resolver},
};
use serde_json::{Value, json};

use crate::Providers;

const ID: &str = "thunderstore";
const API: &str = "https://thunderstore.io/api/experimental/package";

#[derive(Default)]
struct FakeHttp {
    json: BTreeMap<String, Value>,
    requests: RefCell<Vec<String>>,
}

impl HttpClient for FakeHttp {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let url = request.url;
        assert_eq!(
            request.method,
            Method::Get,
            "Thunderstore is never posted to: {url}"
        );
        self.requests.borrow_mut().push(url.to_owned());
        self.json
            .get(url)
            .map(|body| serde_json::to_vec(body).unwrap().into())
            .ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
    }

    fn download(&self, request: &HttpRequest<'_>, _: &mut dyn Write) -> Result<u64, HttpError> {
        panic!("these tests never download: {}", request.url)
    }
}

/// A package as `/api/experimental/package/{namespace}/{name}/` returns it, trimmed.
fn package(
    namespace: &str,
    name: &str,
    version: &str,
    dependencies: &[&str],
    community: &str,
) -> Value {
    json!({
        "namespace": namespace,
        "name": name,
        "full_name": format!("{namespace}-{name}"),
        "owner": namespace,
        "date_created": "2021-04-30T20:59:52.524322Z",
        "is_deprecated": false,
        "latest": {
            "namespace": namespace,
            "name": name,
            "version_number": version,
            "full_name": format!("{namespace}-{name}-{version}"),
            "dependencies": dependencies,
            "download_url": format!("https://thunderstore.io/package/download/{namespace}/{name}/{version}/"),
            "downloads": 115_608,
            "date_created": "2026-09-09T21:49:37.646168Z",
            "is_active": true
        },
        "community_listings": [{ "has_nsfw_content": false, "categories": ["Libraries"], "community": community, "review_status": "approved" }]
    })
}

fn catalogue() -> FakeHttp {
    let mut http = FakeHttp::default();
    for (path, body) in [
        (
            "ValheimModding/Jotunn",
            package(
                "ValheimModding",
                "Jotunn",
                "2.30.0",
                &["denikson-BepInExPack_Valheim-5.4.2333"],
                "valheim",
            ),
        ),
        (
            "denikson/BepInExPack_Valheim",
            package(
                "denikson",
                "BepInExPack_Valheim",
                "5.4.2350",
                &[],
                "valheim",
            ),
        ),
        (
            "Elsewhere/Pack",
            package("Elsewhere", "Pack", "1.0.0", &[], "riskofrain2"),
        ),
    ] {
        http.json.insert(format!("{API}/{path}/"), body);
    }
    http
}

fn target() -> Target {
    Target {
        game: "valheim".to_owned(),
        edition: None,
        storefront: None,
        loader: "none".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: None,
        side: Side::Client,
    }
}

#[test]
fn references_are_namespace_and_name() {
    let providers = Providers::builtins().unwrap();
    let adapter = providers.adapter(ID).unwrap();
    assert!(matches!(
        adapter.request("ValheimModding-Jotunn@2.30.0"),
        Ok(Request::Project { reference, version: Some(version) })
            if reference == "ValheimModding-Jotunn" && version == "2.30.0"
    ));
    for malformed in ["Jotunn", "a-b-c", "../x-y", "Valheim Modding-Jotunn"] {
        assert!(adapter.request(malformed).is_err(), "{malformed}");
    }
}

#[test]
fn a_package_resolves_with_its_text_dependencies_as_zip_downloads() {
    let providers = Providers::builtins().unwrap();
    let adapter = providers.adapter(ID).unwrap();
    let http = catalogue();
    let overlay = Overlay::from_toml(&[]).unwrap();
    let plan = Resolver {
        adapters: &Only(adapter),
        http: &http,
        target: &target(),
        overlay: &overlay,
    }
    .plan_install(
        &[ProjectRequest {
            provider: ID.to_owned(),
            reference: "ValheimModding-Jotunn".to_owned(),
            version: None,
        }],
        true,
        &[],
    )
    .unwrap();
    let selected: Vec<(&str, Option<&str>)> = plan
        .selections
        .iter()
        .map(|selection| (selection.project.label(), selection.required_by.as_deref()))
        .collect();
    assert_eq!(
        selected,
        [
            ("ValheimModding-Jotunn", None),
            (
                "denikson-BepInExPack_Valheim",
                Some("ValheimModding-Jotunn")
            ),
        ]
    );
    let file = &plan.selections.first().unwrap().file;
    assert_eq!(file.name, "ValheimModding-Jotunn-2.30.0.zip");
    assert_eq!(
        file.download,
        Download::Direct {
            url: "https://thunderstore.io/package/download/ValheimModding/Jotunn/2.30.0/"
                .to_owned()
        }
    );
    assert!(
        http.requests
            .borrow()
            .iter()
            .all(|url| url.starts_with(API))
    );
}

#[test]
fn packages_are_served_only_for_the_communities_they_are_listed_in() {
    let providers = Providers::builtins().unwrap();
    let releases = providers.adapter(ID).unwrap().as_releases().unwrap();
    let http = catalogue();
    assert!(
        releases
            .project(&http, "Elsewhere-Pack", &target())
            .is_err()
    );
    let mut unmapped = target();
    unmapped.game = "unmapped".to_owned();
    let refused = releases
        .project(&http, "ValheimModding-Jotunn", &unmapped)
        .unwrap_err();
    assert!(
        refused.to_string().contains("does not serve unmapped"),
        "{refused}"
    );
}

#[test]
fn installed_packages_update_to_the_latest_listed_version() {
    let providers = Providers::builtins().unwrap();
    let updates = providers.adapter(ID).unwrap().as_updates().unwrap();
    let http = catalogue();
    let installed = |number: &str| Provenance {
        provider: ID.to_owned(),
        project: "ValheimModding-Jotunn".to_owned(),
        version: format!("ValheimModding-Jotunn-{number}"),
        version_number: number.to_owned(),
        hashes: BTreeMap::new(),
    };
    let checks = updates
        .check(
            &http,
            &[&installed("2.20.0"), &installed("2.30.0")],
            &target(),
        )
        .unwrap();
    assert!(matches!(
        checks.first(),
        Some(UpdateCheck::Available(update)) if update.release.number == "2.30.0"
    ));
    assert_eq!(checks.get(1), Some(&UpdateCheck::Current));
}
