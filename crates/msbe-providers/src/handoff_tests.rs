//! Handoff links through the reviewed catalog runtime: a link read against the structure its
//! program declares, and redeemed for a file MSBE downloads itself.

use std::{cell::RefCell, collections::BTreeMap, io::Write};

use msbe_provider_api::{
    Adapter, HttpClient, HttpError, HttpRequest, HttpResponse, Method, ProviderProgram,
    model::{Download, HandoffTicket},
};
use serde_json::{Value, json};

use crate::runtime;

const PROGRAM: &str = r#"
runtime = "catalog-v1"
capabilities = []

[games]
game = "game-domain"
saga = { id = "saga", editions = { remaster = "saga-remaster" } }

[provider]
schema = 1
id = "linked"
name = "Linked"
[provider.source]
type = "prefixed"
prefix = "linked:"
[provider.metadata]
api_base = "https://api.linked.test"
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
path = ["mods", "{project}", "files", "{release}"]
query = { key = "key", expires = "expires" }
redeem = "/v1/games/{game}/mods/{project}/files/{release}/link.json"

[mappings.handoff]
urls = { each = "", value = "/URI" }
"#;

const NOW: u64 = 1_700_000_000;
const LINK: &str =
    "handoff://game-domain/mods/1234/files/5678?user_id=42&key=secret%2Bkey&expires=1700000600";
const REDEEM: &str = "https://api.linked.test/v1/games/game-domain/mods/1234/files/5678/link.json";

fn adapter() -> Box<dyn Adapter> {
    let program: ProviderProgram = toml::from_str(PROGRAM).unwrap();
    program.validate().unwrap();
    runtime::build(program)
}

fn parse(adapter: &dyn Adapter, link: &str) -> Result<HandoffTicket, String> {
    adapter
        .as_handoff()
        .unwrap()
        .parse(link, NOW)
        .map_err(|error| error.to_string())
}

/// A request as sent: its URL and its query parameters.
type Sent = (String, Vec<(String, String)>);

/// Canned JSON by URL, answering 403 otherwise, and every request sent.
#[derive(Default)]
struct Answers {
    json: BTreeMap<String, Value>,
    sent: RefCell<Vec<Sent>>,
}

impl HttpClient for Answers {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        assert_eq!(request.method, Method::Get);
        self.sent.borrow_mut().push((
            request.url.to_owned(),
            request
                .query
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        ));
        self.json
            .get(request.url)
            .map(|body| serde_json::to_vec(body).unwrap().into())
            .ok_or_else(|| HttpError::Status {
                url: request.url.to_owned(),
                status: 403,
            })
    }

    fn download(&self, request: &HttpRequest<'_>, _: &mut dyn Write) -> Result<u64, HttpError> {
        panic!("redeeming never downloads: {}", request.url)
    }
}

#[test]
fn a_link_is_read_against_the_declared_structure_and_keeps_only_declared_parameters() {
    let adapter = adapter();
    assert_eq!(adapter.as_handoff().unwrap().scheme(), "handoff");
    let ticket = parse(adapter.as_ref(), LINK).unwrap();
    assert_eq!(
        (
            ticket.provider.as_str(),
            ticket.game.as_str(),
            ticket.catalog_game.as_str(),
            ticket.project.as_str(),
            ticket.release.as_str(),
            ticket.expires,
        ),
        (
            "linked",
            "game",
            "game-domain",
            "1234",
            "5678",
            Some(1_700_000_600)
        )
    );
    assert_eq!(
        ticket.query,
        [
            ("key".to_owned(), "secret+key".to_owned()),
            ("expires".to_owned(), "1700000600".to_owned()),
        ]
    );
    let shown = format!("{ticket:?}");
    assert!(!shown.contains("secret"), "{shown}");

    let edition = parse(
        adapter.as_ref(),
        "HANDOFF://saga-remaster/mods/1/files/2?expires=1700000001&key=k#fragment",
    )
    .unwrap();
    assert_eq!(
        (edition.game.as_str(), edition.release.as_str()),
        ("saga", "2")
    );
}

#[test]
fn links_that_do_not_fit_are_refused_without_repeating_the_link() {
    let adapter = adapter();
    let long = format!("{}{}", LINK, "a".repeat(2048));
    for (link, expected) in [
        (
            "other://game-domain/mods/1/files/2?key=secretkey&expires=1700000600",
            "scheme",
        ),
        ("handoff:game-domain/mods/1/files/2?key=secretkey", "scheme"),
        (
            "handoff://unknown/mods/1/files/2?key=secretkey&expires=1700000600",
            "game",
        ),
        (
            "handoff://game-domain/mod/1/files/2?key=secretkey&expires=1700000600",
            "path",
        ),
        (
            "handoff://game-domain/mods/1/files/2/extra?key=secretkey&expires=1700000600",
            "path",
        ),
        (
            "handoff://game-domain/mods/1/files/2/?key=secretkey&expires=1700000600",
            "path",
        ),
        (
            "handoff://game-domain/mods/1%2F3/files/2?key=secretkey&expires=1700000600",
            "reference",
        ),
        (
            "handoff://game-domain/mods/../files/2?key=secretkey&expires=1700000600",
            "reference",
        ),
        (
            "handoff://game-domain/mods/1/files/2?expires=1700000600",
            "query",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=secretkey&key=secretkey&expires=1700000600",
            "query",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=&expires=1700000600",
            "query",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=secret%20key&expires=1700000600",
            "query",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=secretkey&expires=soon",
            "expiry",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=secretkey&expires=1699999999",
            "expired",
        ),
        (
            "handoff://game-domain/mods/1/files/2?key=secretkey&expires=1700000000",
            "expired",
        ),
        (long.as_str(), "too long"),
    ] {
        let refused = parse(adapter.as_ref(), link).unwrap_err();
        assert!(refused.contains(expected), "{link}: {refused}");
        assert!(!refused.contains("secret"), "{link}: {refused}");
    }
}

/// A small deterministic generator, so a failing case reproduces.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }
}

#[test]
fn parsing_never_panics_and_never_yields_a_reference_with_a_slash() {
    const ALPHABET: &[u8] = b"handoff:/?&=#%.-_+~ 0123456789abcdefgmsAZ\\@";
    let adapter = adapter();
    let handoff = adapter.as_handoff().unwrap();
    let mut random = XorShift(0x2545_F491_4F6C_DD1D);
    let mut accepted = 0;
    for round in 0..5000 {
        let mut link: Vec<u8> = if round % 2 == 0 {
            LINK.as_bytes().to_vec()
        } else {
            (0..random.below(96))
                .map(|_| *ALPHABET.get(random.below(ALPHABET.len())).unwrap())
                .collect()
        };
        for _ in 0..random.below(4) {
            if link.is_empty() {
                break;
            }
            let at = random.below(link.len());
            let byte = *ALPHABET.get(random.below(ALPHABET.len())).unwrap();
            match random.below(3) {
                0 => *link.get_mut(at).unwrap() = byte,
                1 => link.insert(at, byte),
                _ => {
                    link.remove(at);
                }
            }
        }
        let link = String::from_utf8(link).unwrap();
        if let Ok(ticket) = handoff.parse(&link, NOW) {
            accepted += 1;
            for reference in [&ticket.project, &ticket.release, &ticket.catalog_game] {
                assert!(
                    !reference.is_empty() && !reference.contains(['/', '\\', '%', '?', '#']),
                    "{link:?} gave {reference:?}"
                );
            }
        }
    }
    assert!(accepted > 0, "some mutated links still fit");
}

#[test]
fn a_redeemed_link_falls_back_to_its_url_when_the_catalog_lists_nothing() {
    let adapter = adapter();
    let handoff = adapter.as_handoff().unwrap();
    let ticket = parse(adapter.as_ref(), LINK).unwrap();
    let mut http = Answers::default();
    http.json.insert(
        REDEEM.to_owned(),
        json!([{ "name": "Mirror", "short_name": "mirror",
                 "URI": "https://files.linked.test/cdn/Some%20Mod-1234.zip?md5=abc&expires=1" }]),
    );

    let redeemed = handoff.redeem(&http, &ticket).unwrap();
    let file = &redeemed.file;
    assert_eq!(
        file.download,
        Download::Direct {
            url: "https://files.linked.test/cdn/Some%20Mod-1234.zip?md5=abc&expires=1".to_owned()
        }
    );
    assert_eq!(file.name, "Some Mod-1234.zip");
    assert_eq!(
        (file.size, file.limit, file.sha512.as_deref()),
        (None, None, None)
    );
    // This program declares no project or releases route, so nothing is looked up and nothing but
    // the redeem request is sent.
    assert_eq!(redeemed.title, None);
    assert_eq!(
        http.sent.borrow().as_slice(),
        [(
            REDEEM.to_owned(),
            vec![
                ("key".to_owned(), "secret+key".to_owned()),
                ("expires".to_owned(), "1700000600".to_owned()),
            ]
        )]
    );

    let mut foreign = ticket.clone();
    foreign.provider = "other".to_owned();
    let mut tampered = ticket.clone();
    tampered.project = "../1234".to_owned();
    let mut elsewhere = ticket.clone();
    elsewhere.game = "saga".to_owned();
    for (refused, expected) in [
        (foreign, "another provider"),
        (tampered, "reference"),
        (elsewhere, "reference"),
    ] {
        let error = handoff.redeem(&http, &refused).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
    assert_eq!(
        http.sent.borrow().len(),
        1,
        "a refused ticket sends nothing"
    );

    http.json.insert(REDEEM.to_owned(), json!([]));
    let empty = handoff.redeem(&http, &ticket).unwrap_err().to_string();
    assert!(empty.contains("without a download URL"), "{empty}");

    http.json.clear();
    let denied = handoff.redeem(&http, &ticket).unwrap_err().to_string();
    assert!(
        denied.contains("403") && !denied.contains("secret"),
        "{denied}"
    );
}

/// A program shaped like Nexus Mods: the link is redeemed for a CDN URL that describes nothing,
/// while the catalog lists the file's real name and the project's own.
const LISTING_PROGRAM: &str = r#"
runtime      = "catalog-v1"
capabilities = ["project", "releases"]

[games]
game = "game-domain"

[provider]
schema = 1
id     = "linked"
name   = "Linked"
[provider.source]
type   = "prefixed"
prefix = "linked:"
[provider.metadata]
api_base = "https://api.linked.test"
[provider.acquisition]
type   = "browser_assisted"
scheme = "handoff"
[provider.policy]
requires_auth              = false
respects_distribution_flag = false
tos_url                    = ""
ack_required               = false

[handoff]
host   = "game"
path   = ["mods", "{project}", "files", "{release}"]
query  = { key = "key", expires = "expires" }
redeem = "/v1/games/{game}/mods/{project}/files/{release}/link.json"

[routes]
project  = "/v1/games/{game}/mods/{reference}.json"
releases = "/v1/games/{game}/mods/{project}/files.json"

[pages]
release = "https://linked.test/{game}/mods/{project}?file_id={release}"

[mappings.handoff]
urls = { each = "", value = "/URI" }

[mappings.project]
id    = "/mod_id"
title = "/name"

[mappings]
releases = "/files"

[mappings.release]
id        = "/file_id"
number    = "/version"
published = "/uploaded_timestamp"
files     = { single = "" }

[mappings.release.file]
name     = "/file_name"
size_kib = "/size_kb"
primary  = "/is_primary"
"#;

const PROJECT: &str = "https://api.linked.test/v1/games/game-domain/mods/1234.json";
const RELEASES: &str = "https://api.linked.test/v1/games/game-domain/mods/1234/files.json";

#[test]
fn a_redeemed_link_takes_its_name_and_project_from_the_catalog_listing() {
    let program: ProviderProgram = toml::from_str(LISTING_PROGRAM).unwrap();
    program.validate().unwrap();
    let adapter = runtime::build(program);
    let handoff = adapter.as_handoff().unwrap();
    let ticket = parse(adapter.as_ref(), LINK).unwrap();
    let mut http = Answers::default();
    // What a content delivery network answers with: an opaque id, no extension, no mod name.
    http.json.insert(
        REDEEM.to_owned(),
        json!([{ "URI": "https://files.linked.test/87dc822d-196e-46ea-93de-14ea9aec0c26?expires=1" }]),
    );
    http.json.insert(
        RELEASES.to_owned(),
        json!({ "files": [
            { "file_id": 5678, "file_name": "Some Mod-1234-1-0.zip", "version": "1.0",
              "uploaded_timestamp": 1_700_000_000u64, "size_kb": 760, "is_primary": true },
            { "file_id": 999, "file_name": "Some Mod-old.zip", "version": "0.9",
              "uploaded_timestamp": 1_600_000_000u64, "size_kb": 10, "is_primary": false },
        ] }),
    );
    http.json.insert(
        PROJECT.to_owned(),
        json!({ "mod_id": 1234, "name": "Some Mod" }),
    );

    let redeemed = handoff.redeem(&http, &ticket).unwrap();
    // The download still goes to the redeemed URL: only the description comes from the catalog.
    assert_eq!(
        redeemed.file.download,
        Download::Direct {
            url: "https://files.linked.test/87dc822d-196e-46ea-93de-14ea9aec0c26?expires=1"
                .to_owned()
        }
    );
    assert_eq!(redeemed.file.name, "Some Mod-1234-1-0.zip");
    assert_eq!(redeemed.file.limit, Some(761 * 1024));
    assert_eq!(redeemed.title.as_deref(), Some("Some Mod"));

    // A listing that cannot be reached leaves the link to describe its own file, rather than
    // failing a download the user is waiting on.
    http.json.remove(RELEASES);
    http.json.remove(PROJECT);
    let bare = handoff.redeem(&http, &ticket).unwrap();
    assert_eq!(bare.file.name, "87dc822d-196e-46ea-93de-14ea9aec0c26");
    assert_eq!(bare.title, None);
}
