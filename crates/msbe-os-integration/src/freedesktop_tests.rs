//! Registration against freedesktop.org directories in a temporary tree.

use std::{
    fs,
    path::{Path, PathBuf},
};

use msbe_core::config::Freedesktop;
use tempfile::TempDir;

use crate::{Error, Handlers, Owner, Scheme};

struct Fixture {
    root: TempDir,
    directories: Freedesktop,
    program: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_desktops(&[])
    }

    fn with_desktops(desktops: &[&str]) -> Self {
        let root = TempDir::new().unwrap();
        let directories = Freedesktop {
            config_home: root.path().join("config"),
            config_dirs: vec![root.path().join("etc")],
            data_home: root.path().join("data"),
            data_dirs: vec![root.path().join("system")],
            desktops: desktops
                .iter()
                .map(|desktop| (*desktop).to_owned())
                .collect(),
        };
        let program = executable(root.path(), "MSBE $HOME%dir\\x");
        Self {
            root,
            directories,
            program,
        }
    }

    fn handlers(&self) -> Handlers {
        Handlers::freedesktop(self.directories.clone())
    }

    fn user_list(&self) -> PathBuf {
        self.directories.config_home.join("mimeapps.list")
    }

    fn entry(&self) -> PathBuf {
        self.directories
            .data_home
            .join("applications/msbe-handler.desktop")
    }
}

/// Installs a desktop entry beneath `data`'s `applications` directory.
fn app(data: &Path, path: &str, name: &str, types: &str) {
    let file = data.join("applications").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=app %u\nMimeType={types}\n"),
    )
    .unwrap();
}

fn executable(root: &Path, directory: &str) -> PathBuf {
    let program = root.join(directory).join("msbe");
    fs::create_dir_all(program.parent().unwrap()).unwrap();
    fs::write(&program, b"").unwrap();
    program
}

fn scheme(text: &str) -> Scheme {
    Scheme::parse(text).unwrap()
}

#[test]
fn an_unclaimed_scheme_gets_a_quoted_entry_and_the_user_default_then_is_released() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");

    let before = handlers.status(&link, &fixture.program).unwrap();
    assert_eq!(before.owner, Owner::Nobody);
    assert!(!before.current);

    let registered = handlers.register(&link, &fixture.program, false).unwrap();
    assert_eq!(registered.owner, Owner::Msbe);
    assert!(registered.current);
    assert_eq!(registered.previous, None);
    let entry = fs::read_to_string(fixture.entry()).unwrap();
    let root = fixture.root.path().display();
    assert!(
        entry.contains(&format!(
            "\nExec=\"{root}/MSBE \\\\$HOME%%dir\\\\\\\\x/msbe\" handoff %u\n"
        )),
        "{entry}"
    );
    assert!(
        entry.contains("\nMimeType=x-scheme-handler/handoff;\n"),
        "{entry}"
    );
    assert_eq!(
        fs::read_to_string(fixture.user_list()).unwrap(),
        "[Default Applications]\nx-scheme-handler/handoff=msbe-handler.desktop\n"
    );

    let released = handlers.unregister(&link, &fixture.program).unwrap();
    assert_eq!(released.owner, Owner::Nobody);
    assert!(!fixture.entry().exists());
    assert_eq!(
        fs::read_to_string(fixture.user_list()).unwrap(),
        "[Default Applications]\n"
    );
}

#[test]
fn another_owner_is_refused_then_replaced_and_given_back_exactly() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    app(
        &fixture.directories.data_home,
        "other.desktop",
        "Other App",
        "x-scheme-handler/handoff;",
    );
    let original = "# kept\n[Default Applications]\ntext/plain=editor.desktop\nx-scheme-handler/handoff=other.desktop;\n\n[Added Associations]\ntext/plain=editor.desktop;\n";
    fs::create_dir_all(&fixture.directories.config_home).unwrap();
    fs::write(fixture.user_list(), original).unwrap();
    let other = Owner::Other("Other App (other.desktop)".to_owned());
    assert_eq!(
        handlers.status(&link, &fixture.program).unwrap().owner,
        other
    );

    let refused = handlers.register(&link, &fixture.program, false);
    assert!(
        matches!(&refused, Err(Error::Owned { owner, .. }) if owner == "Other App (other.desktop)"),
        "{refused:?}"
    );
    assert!(!fixture.entry().exists());
    assert_eq!(fs::read_to_string(fixture.user_list()).unwrap(), original);

    let replaced = handlers.register(&link, &fixture.program, true).unwrap();
    assert_eq!(replaced.owner, Owner::Msbe);
    assert_eq!(
        replaced.previous.as_deref(),
        Some("Other App (other.desktop)")
    );
    let list = fs::read_to_string(fixture.user_list()).unwrap();
    assert!(list.starts_with("# kept\n[Default Applications]\ntext/plain=editor.desktop\nx-scheme-handler/handoff=msbe-handler.desktop\n"), "{list}");

    // The program moves: the old registration is no longer current, and registering again keeps
    // the application it replaced.
    let moved = executable(fixture.root.path(), "moved");
    assert!(!handlers.status(&link, &moved).unwrap().current);
    let updated = handlers.register(&link, &moved, false).unwrap();
    assert!(updated.current);
    assert_eq!(
        updated.previous.as_deref(),
        Some("Other App (other.desktop)")
    );
    assert!(!handlers.status(&link, &fixture.program).unwrap().current);

    let released = handlers.unregister(&link, &moved).unwrap();
    assert_eq!(released.owner, other);
    assert_eq!(fs::read_to_string(fixture.user_list()).unwrap(), original);
    assert!(!fixture.entry().exists());
}

#[test]
fn an_application_found_by_its_declared_type_opens_the_links_again_after_unregistering() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    app(
        fixture.directories.data_dirs.first().unwrap(),
        "vendor/tool.desktop",
        "Tool",
        "text/plain;x-scheme-handler/handoff;",
    );
    let tool = Owner::Other("Tool (vendor-tool.desktop)".to_owned());
    assert_eq!(
        handlers.status(&link, &fixture.program).unwrap().owner,
        tool
    );

    let replaced = handlers.register(&link, &fixture.program, true).unwrap();
    assert_eq!(replaced.owner, Owner::Msbe);
    assert_eq!(
        replaced.previous.as_deref(),
        Some("Tool (vendor-tool.desktop)")
    );

    let released = handlers.unregister(&link, &fixture.program).unwrap();
    assert_eq!(released.owner, tool);
    assert!(
        !fs::read_to_string(fixture.user_list())
            .unwrap()
            .contains("x-scheme-handler/handoff")
    );
}

#[test]
fn a_desktop_specific_list_is_where_the_default_is_replaced_and_restored() {
    let fixture = Fixture::with_desktops(&["kde"]);
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    app(
        &fixture.directories.data_home,
        "other.desktop",
        "Other App",
        "",
    );
    let desktop_list = fixture.directories.config_home.join("kde-mimeapps.list");
    let original = "[Default Applications]\nx-scheme-handler/handoff=other.desktop\n";
    fs::create_dir_all(&fixture.directories.config_home).unwrap();
    fs::write(&desktop_list, original).unwrap();

    handlers.register(&link, &fixture.program, true).unwrap();
    assert_eq!(
        fs::read_to_string(&desktop_list).unwrap(),
        "[Default Applications]\nx-scheme-handler/handoff=msbe-handler.desktop\n"
    );
    assert!(!fixture.user_list().exists());
    assert_eq!(
        handlers.status(&link, &fixture.program).unwrap().owner,
        Owner::Msbe
    );

    handlers.unregister(&link, &fixture.program).unwrap();
    assert_eq!(fs::read_to_string(&desktop_list).unwrap(), original);
}

#[test]
fn defaults_naming_missing_or_hidden_entries_fall_through_to_less_important_lists() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    let system = fixture.directories.data_dirs.first().unwrap();
    app(
        system,
        "hidden.desktop",
        "Hidden",
        "x-scheme-handler/handoff;",
    );
    fs::write(
        system.join("applications/hidden.desktop"),
        "[Desktop Entry]\nName=Hidden\nHidden=true\n",
    )
    .unwrap();
    app(system, "fallback.desktop", "Fallback", "");
    fs::create_dir_all(&fixture.directories.config_home).unwrap();
    fs::write(
        fixture.user_list(),
        "[Default Applications]\nx-scheme-handler/handoff=gone.desktop;hidden.desktop;\n",
    )
    .unwrap();
    let etc = fixture.directories.config_dirs.first().unwrap();
    fs::create_dir_all(etc).unwrap();
    fs::write(
        etc.join("mimeapps.list"),
        "[Default Applications]\nx-scheme-handler/handoff=fallback.desktop\n",
    )
    .unwrap();

    assert_eq!(
        handlers.status(&link, &fixture.program).unwrap().owner,
        Owner::Other("Fallback (fallback.desktop)".to_owned())
    );
}

#[test]
fn schemes_share_one_entry_and_unregistering_one_keeps_the_other() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let (first, second) = (scheme("first"), scheme("second"));
    handlers.register(&first, &fixture.program, false).unwrap();
    handlers.register(&second, &fixture.program, false).unwrap();
    let entry = fs::read_to_string(fixture.entry()).unwrap();
    assert!(
        entry.contains("\nMimeType=x-scheme-handler/first;x-scheme-handler/second;\n"),
        "{entry}"
    );

    assert_eq!(
        handlers.unregister(&first, &fixture.program).unwrap().owner,
        Owner::Nobody
    );
    let second_status = handlers.status(&second, &fixture.program).unwrap();
    assert_eq!(second_status.owner, Owner::Msbe);
    assert!(second_status.current);

    handlers.unregister(&second, &fixture.program).unwrap();
    assert!(!fixture.entry().exists());
}

#[test]
fn a_scheme_another_application_took_since_is_left_with_it() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    handlers.register(&link, &fixture.program, false).unwrap();
    app(&fixture.directories.data_home, "later.desktop", "Later", "");
    let taken = "[Default Applications]\nx-scheme-handler/handoff=later.desktop\n";
    fs::write(fixture.user_list(), taken).unwrap();

    let released = handlers.unregister(&link, &fixture.program).unwrap();
    assert_eq!(
        released.owner,
        Owner::Other("Later (later.desktop)".to_owned())
    );
    assert_eq!(fs::read_to_string(fixture.user_list()).unwrap(), taken);
    assert!(!fixture.entry().exists());
}

#[test]
fn a_linked_list_is_written_where_it_is_kept() {
    let fixture = Fixture::new();
    let kept = fixture.root.path().join("dotfiles/mimeapps.list");
    fs::create_dir_all(kept.parent().unwrap()).unwrap();
    fs::write(&kept, "[Default Applications]\n").unwrap();
    fs::create_dir_all(&fixture.directories.config_home).unwrap();
    std::os::unix::fs::symlink(&kept, fixture.user_list()).unwrap();

    fixture
        .handlers()
        .register(&scheme("handoff"), &fixture.program, false)
        .unwrap();
    assert!(
        fs::symlink_metadata(fixture.user_list())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&kept)
            .unwrap()
            .contains("x-scheme-handler/handoff=msbe-handler.desktop")
    );
}

#[test]
fn programs_a_registration_cannot_name_are_refused() {
    let fixture = Fixture::new();
    let handlers = fixture.handlers();
    let link = scheme("handoff");
    let quoted = executable(fixture.root.path(), "a\"b");
    for program in [
        PathBuf::from("relative/msbe"),
        fixture.root.path().join("missing"),
        quoted,
    ] {
        assert!(
            matches!(
                handlers.register(&link, &program, false),
                Err(Error::Program { .. })
            ),
            "{}",
            program.display()
        );
    }
    assert!(!fixture.entry().exists());
}
