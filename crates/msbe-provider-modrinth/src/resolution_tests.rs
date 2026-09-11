//! The provider-neutral resolver walking real Modrinth records through this adapter.

use msbe_plan_schema::Side;
use msbe_provider_api::{
    Adapter, Overlay, PackageId,
    model::Request,
    resolve::{
        InstallPlan, InstalledRelease, Only, ProjectRequest, Requirement, ResolveError, Resolver,
        Substitution,
    },
};
use serde_json::{Value, json};

use crate::{
    ID, Modrinth, REGISTRATION,
    test_support::{BASE, FakeHttp, catalogue, fabric_api_catalogue, target, version},
    wire,
};

fn request(modrinth: &Modrinth, raw: &str) -> ProjectRequest {
    match modrinth.request(raw) {
        Ok(Request::Project { reference, version }) => ProjectRequest {
            provider: ID.to_owned(),
            reference,
            version,
        },
        other => panic!("expected a project request for {raw}, got {other:?}"),
    }
}

/// Resolves `requests` against the fabric client target.
fn plan(
    http: &FakeHttp,
    overlay: &Overlay,
    requests: &[&str],
    with_dependencies: bool,
    installed: &[InstalledRelease],
) -> Result<InstallPlan, ResolveError> {
    let modrinth = Modrinth::new();
    let requests: Vec<ProjectRequest> =
        requests.iter().map(|raw| request(&modrinth, raw)).collect();
    Resolver {
        adapters: &Only(&modrinth),
        http,
        target: &target(),
        overlay,
    }
    .plan_install(&requests, with_dependencies, installed)
}

fn modrinth(project: &str) -> PackageId {
    PackageId {
        provider: ID.to_owned(),
        project: project.to_owned(),
    }
}

fn labels(plan: &InstallPlan) -> Vec<(&str, Option<&str>)> {
    plan.selections
        .iter()
        .map(|selection| (selection.project.label(), selection.required_by.as_deref()))
        .collect()
}

#[test]
fn required_dependencies_are_walked_only_when_asked() {
    let http = catalogue();
    let none = Overlay::default();

    let with = plan(&http, &none, &["iris"], true, &[]).unwrap();
    assert_eq!(labels(&with), [("iris", None), ("sodium", Some("iris"))]);
    assert!(with.unresolved.is_empty());

    let without = plan(&http, &none, &["iris"], false, &[]).unwrap();
    assert_eq!(labels(&without), [("iris", None)]);
    assert_eq!(
        without.unresolved,
        [Requirement {
            provider: ID.to_owned(),
            project_id: "AANobbMI".to_owned(),
            declared_by: "iris".to_owned(),
        }]
    );
}

#[test]
fn pins_name_a_release_by_id_or_version_number() {
    let http = catalogue();
    let none = Overlay::default();
    let chosen = |raw: &str| {
        plan(&http, &none, &[raw], false, &[]).map(|plan| {
            plan.selections
                .first()
                .map(|selection| selection.release.id.clone())
        })
    };
    // Unpinned, the newest compatible version wins, whatever its channel.
    assert_eq!(chosen("sodium").unwrap().as_deref(), Some("S2"));
    assert_eq!(chosen("sodium@0.8.12").unwrap().as_deref(), Some("S1"));
    assert_eq!(chosen("sodium@S1").unwrap().as_deref(), Some("S1"));
    assert!(matches!(
        chosen("sodium@9.9.9"),
        Err(ResolveError::Solver(_))
    ));
}

#[test]
fn dependency_solver_backtracks_to_satisfy_an_exact_transitive_release() {
    let mut http = catalogue();
    let versions = |http: &mut FakeHttp, project: &str| {
        http.json
            .get_mut(&format!("{BASE}/project/{project}/version"))
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap()
    };
    let mut sodium = versions(&mut http, "AANobbMI");
    sodium.push(version(
        "S3",
        "AANobbMI",
        "0.9.0",
        "release",
        "2026-09-01T00:00:00Z",
        "sodium-0.9.0.jar",
    ));
    http.route("/project/AANobbMI/version", Value::Array(sodium));
    let mut iris = versions(&mut http, "YL57xq9U");
    if let Some(iris) = iris.first_mut().and_then(Value::as_object_mut) {
        iris.insert(
            "dependencies".to_owned(),
            json!([{ "project_id": "AANobbMI", "version_id": "S1", "dependency_type": "required" }]),
        );
    }
    http.route("/project/YL57xq9U/version", Value::Array(iris));

    let resolved = plan(&http, &Overlay::default(), &["iris", "sodium"], true, &[]).unwrap();
    let sodium = resolved
        .selections
        .iter()
        .find(|selection| selection.project.id == modrinth("AANobbMI"))
        .unwrap();
    assert_eq!(sodium.release.id, "S1");
}

#[test]
fn a_project_reached_twice_is_selected_once() {
    let http = catalogue();
    let resolved = plan(&http, &Overlay::default(), &["sodium", "iris"], true, &[]).unwrap();
    let names: Vec<&str> = resolved
        .selections
        .iter()
        .map(|selection| selection.project.label())
        .collect();
    assert_eq!(names, ["sodium", "iris"]);
}

#[test]
fn a_project_that_does_not_support_the_target_side_is_refused() {
    let mut http = catalogue();
    if let Some(project) = http
        .json
        .get_mut(&format!("{BASE}/project/sodium"))
        .and_then(Value::as_object_mut)
    {
        project.insert("server_side".to_owned(), json!("unsupported"));
    }
    let modrinth = Modrinth::new();
    let mut server = target();
    server.side = Side::Server;
    let resolved = Resolver {
        adapters: &Only(&modrinth),
        http: &http,
        target: &server,
        overlay: &Overlay::default(),
    }
    .plan_install(&[request(&modrinth, "sodium")], false, &[]);
    assert!(
        matches!(
            &resolved,
            Err(ResolveError::UnsupportedSide {
                side: Side::Server,
                ..
            })
        ),
        "{resolved:?}"
    );
}

#[test]
fn an_installed_stand_in_meets_requirements_and_excludes_the_original() {
    let http = fabric_api_catalogue();
    let overlay = Overlay::from_toml(REGISTRATION.overlay).unwrap();

    // Nothing stands in yet, so Fabric API is preferred over the fork that provides it, and the
    // stand-in Modrinth no longer has does not fail resolution.
    let fresh = plan(&http, &overlay, &["sodium"], true, &[]).unwrap();
    assert_eq!(
        labels(&fresh),
        [("sodium", None), ("fabric-api", Some("sodium"))]
    );
    assert!(fresh.substitutions.is_empty());

    let quilted = [InstalledRelease {
        package: modrinth("qvIfYCYJ"),
        release: "qsl-1".to_owned(),
    }];
    let substitution = Substitution {
        requirement: Requirement {
            provider: ID.to_owned(),
            project_id: "P7dR8mSH".to_owned(),
            declared_by: "sodium".to_owned(),
        },
        supplied_by: modrinth("qvIfYCYJ"),
    };
    let kept = plan(&http, &overlay, &["sodium"], true, &quilted).unwrap();
    assert_eq!(labels(&kept), [("sodium", None), ("qsl", Some("sodium"))]);
    assert_eq!(kept.substitutions, std::slice::from_ref(&substitution));

    // Without walking dependencies, the installed fork is never fetched, yet it still meets the
    // requirement instead of leaving it unresolved.
    let bare = plan(&http, &overlay, &["sodium"], false, &quilted).unwrap();
    assert_eq!(labels(&bare), [("sodium", None)]);
    assert!(bare.unresolved.is_empty(), "{:?}", bare.unresolved);
    assert_eq!(bare.substitutions, [substitution]);

    assert!(matches!(
        plan(&http, &overlay, &["P7dR8mSH"], false, &quilted),
        Err(ResolveError::Solver(_))
    ));
}

#[test]
fn relationships_resolve_dependencies_that_name_only_a_version() {
    fn ids(list: &[Requirement]) -> Vec<&str> {
        list.iter()
            .map(|requirement| requirement.project_id.as_str())
            .collect()
    }

    let mut http = FakeHttp::default();
    http.route(
        "/version/V9",
        version(
            "V9",
            "QQQ",
            "2.0",
            "release",
            "2026-01-01T00:00:00Z",
            "q.jar",
        ),
    );
    let mut iris = version(
        "I1",
        "YL57xq9U",
        "1.8.0",
        "release",
        "2026-08-01T00:00:00Z",
        "iris.jar",
    );
    iris.as_object_mut().unwrap().insert(
        "dependencies".to_owned(),
        json!([
            { "project_id": "AANobbMI", "dependency_type": "required" },
            { "version_id": "V9", "dependency_type": "required" },
            { "project_id": "XXX", "dependency_type": "incompatible" },
            { "project_id": "YYY", "dependency_type": "optional" }
        ]),
    );
    let release = serde_json::from_value::<wire::Version>(iris)
        .unwrap()
        .into_model();

    let modrinth = Modrinth::new();
    let found = Resolver {
        adapters: &Only(&modrinth),
        http: &http,
        target: &target(),
        overlay: &Overlay::default(),
    }
    .relationships(&release, "iris")
    .unwrap();
    assert_eq!(ids(&found.required), ["AANobbMI", "QQQ"]);
    assert_eq!(ids(&found.incompatible), ["XXX"]);
    assert!(found.required.iter().all(|r| r.declared_by == "iris"));
}
