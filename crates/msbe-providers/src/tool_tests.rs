//! External tools through the reviewed `tool-v1` runtime: an item resolves to one release for the
//! target's game, and the registered tool fetches it through the host the caller supplies.

use std::{
    cell::RefCell,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use msbe_plan_schema::Side;
use msbe_provider_api::{
    Adapter, AdapterError, HttpClient, HttpError, HttpRequest, HttpResponse, ProviderProgram,
    Target, ToolArgument, ToolError, ToolHost, ToolInvocation,
    model::{Download, ReleaseFile, Request},
};

use crate::runtime;

const PROGRAM: &str = r#"
runtime = "tool-v1"
capabilities = []

[games]
example = "123456"

[provider]
schema = 1
id = "example-tool"
name = "Example tool"
[provider.source]
type = "prefixed"
prefix = "example-tool:"
[provider.acquisition]
type = "external_tool"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = "https://www.example.test/terms"
ack_required = true

[tool]
arguments = ["fetch", "--game", "{game}", "--item", "{item}", "--into", "{output}"]
output = ["content", "{game}", "{item}"]
timeout = 1800
"#;

fn adapter() -> Box<dyn Adapter> {
    let program: ProviderProgram = toml::from_str(PROGRAM).unwrap();
    program.validate().unwrap();
    runtime::build(program)
}

fn target(game: &str) -> Target {
    Target {
        game: game.to_owned(),
        edition: None,
        storefront: None,
        loader: "native".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: None,
        side: Side::Client,
    }
}

fn tool_file(game: &str, item: &str) -> ReleaseFile {
    ReleaseFile {
        download: Download::Tool {
            game: game.to_owned(),
            item: item.to_owned(),
        },
        name: item.to_owned(),
        size: None,
        limit: None,
        md5: None,
        sha1: None,
        sha256: None,
        sha512: None,
        primary: true,
    }
}

/// A tool runtime makes no request.
struct NoNetwork;

impl HttpClient for NoNetwork {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        panic!("a tool runtime made a request to {}", request.url);
    }

    fn download(&self, request: &HttpRequest<'_>, _sink: &mut dyn Write) -> Result<u64, HttpError> {
        panic!("a tool runtime downloaded {}", request.url);
    }
}

/// A host that records each run and writes `files` beneath the output directory it creates.
#[derive(Default)]
struct FakeHost {
    files: Vec<(&'static str, &'static str)>,
    runs: RefCell<Vec<ToolInvocation>>,
}

impl ToolHost for FakeHost {
    fn run(&self, invocation: &ToolInvocation, dir: &Path) -> Result<PathBuf, ToolError> {
        self.runs.borrow_mut().push(invocation.clone());
        let output = dir.join("output");
        fs::create_dir(&output).unwrap();
        for (path, content) in &self.files {
            let path = output.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        Ok(output)
    }
}

#[test]
fn an_item_resolves_to_one_release_the_registered_tool_fetches_for_the_game() {
    let adapter = adapter();
    assert_eq!(
        adapter.request("987").unwrap(),
        Request::Project {
            reference: "987".to_owned(),
            version: None
        }
    );
    assert!(adapter.request("-rf").is_err());
    let releases = adapter.as_releases().unwrap();
    assert_eq!(
        releases
            .project(&NoNetwork, "987", &target("example"))
            .unwrap()
            .id
            .project,
        "987"
    );
    assert!(
        releases
            .project(&NoNetwork, "987", &target("other"))
            .is_err()
    );
    let release = releases
        .releases(&NoNetwork, "987", &target("example"))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let file = release.primary_file().unwrap();
    assert_eq!(file, &tool_file("123456", "987"));

    let host = FakeHost {
        files: vec![
            ("content/123456/987/mod.pak", "pak"),
            ("content/123456/987/readme.txt", "hi"),
        ],
        ..FakeHost::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let acquired = adapter
        .acquire(&NoNetwork, &host, file, dir.path())
        .unwrap();
    assert_eq!(acquired.path, dir.path().join("output/content/123456/987"));
    assert_eq!(acquired.size, 5);
    let runs = host.runs.borrow();
    let invocation = runs.first().unwrap();
    assert_eq!(invocation.tool, "example-tool");
    assert_eq!(invocation.arguments.last(), Some(&ToolArgument::Output));
    assert!(
        invocation
            .arguments
            .contains(&ToolArgument::Literal("987".to_owned()))
    );
}

#[test]
fn a_tool_that_leaves_nothing_where_the_item_lands_is_unavailable() {
    let host = FakeHost {
        files: vec![("elsewhere/mod.pak", "pak")],
        ..FakeHost::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let error = adapter()
        .acquire(&NoNetwork, &host, &tool_file("123456", "987"), dir.path())
        .unwrap_err();
    assert!(
        matches!(error, AdapterError::Tool(ToolError::Empty { .. })),
        "{error}"
    );
    assert!(
        error.to_string().contains("Complete its own sign-in"),
        "{error}"
    );
}

#[test]
fn a_game_or_item_the_program_cannot_pass_never_reaches_the_tool() {
    let host = FakeHost::default();
    let dir = tempfile::tempdir().unwrap();
    let adapter = adapter();
    for file in [tool_file("999", "987"), tool_file("123456", "--help")] {
        assert!(
            adapter
                .acquire(&NoNetwork, &host, &file, dir.path())
                .is_err()
        );
    }
    assert!(host.runs.borrow().is_empty());
}
