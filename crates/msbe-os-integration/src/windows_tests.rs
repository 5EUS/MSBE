//! Registration against a test key beneath the current user's hive, never `Software\Classes`.

use std::{fs, path::PathBuf};

use tempfile::TempDir;
use windows_registry::CURRENT_USER;

use crate::{Error, Handlers, Owner, Scheme};

/// A classes key of its own beneath `Software\MSBE Tests`, removed when dropped.
struct TestClasses {
    root: String,
    _directory: TempDir,
    program: PathBuf,
}

impl TestClasses {
    fn new(name: &str) -> Self {
        let root = format!(r"Software\MSBE Tests\{}-{name}\Classes", std::process::id());
        drop(CURRENT_USER.remove_tree(&root));
        let directory = TempDir::new().unwrap();
        let program = directory.path().join("msbe.exe");
        fs::write(&program, b"").unwrap();
        Self {
            root,
            _directory: directory,
            program,
        }
    }
}

impl Drop for TestClasses {
    fn drop(&mut self) {
        drop(CURRENT_USER.remove_tree(&self.root));
    }
}

#[test]
fn an_owned_scheme_is_refused_then_replaced_and_its_key_put_back() {
    let classes = TestClasses::new("owned");
    let handlers = Handlers::windows_classes(&classes.root);
    let link = Scheme::parse("handoff").unwrap();

    let other = CURRENT_USER
        .create(format!(r"{}\handoff", classes.root))
        .unwrap();
    other.set_string("", "URL:Other").unwrap();
    other.set_string("URL Protocol", "").unwrap();
    let command = other.create(r"shell\open\command").unwrap();
    command
        .set_string("", r#""C:\Other\other.exe" "%1""#)
        .unwrap();
    other
        .create("DefaultIcon")
        .unwrap()
        .set_u32("Size", 32)
        .unwrap();
    drop((command, other));
    let owner = Owner::Other(r#""C:\Other\other.exe" "%1""#.to_owned());
    assert_eq!(
        handlers.status(&link, &classes.program).unwrap().owner,
        owner
    );

    assert!(matches!(
        handlers.register(&link, &classes.program, false),
        Err(Error::Owned { .. })
    ));
    let replaced = handlers.register(&link, &classes.program, true).unwrap();
    assert_eq!(replaced.owner, Owner::Msbe);
    assert!(replaced.current);
    let registered = CURRENT_USER
        .open(format!(r"{}\handoff\shell\open\command", classes.root))
        .unwrap();
    assert_eq!(
        registered.get_string("").unwrap(),
        format!("\"{}\" handoff \"%1\"", classes.program.display())
    );

    let released = handlers.unregister(&link, &classes.program).unwrap();
    assert_eq!(released.owner, owner);
    let restored = CURRENT_USER
        .open(format!(r"{}\handoff", classes.root))
        .unwrap();
    assert_eq!(restored.get_string("").unwrap(), "URL:Other");
    assert_eq!(
        restored
            .open("DefaultIcon")
            .unwrap()
            .get_u32("Size")
            .unwrap(),
        32
    );
    assert!(restored.open("MSBE").is_err());
}

#[test]
fn an_unclaimed_scheme_is_registered_and_removed() {
    let classes = TestClasses::new("unclaimed");
    let handlers = Handlers::windows_classes(&classes.root);
    let link = Scheme::parse("handoff").unwrap();

    assert_eq!(
        handlers.status(&link, &classes.program).unwrap().owner,
        Owner::Nobody
    );
    handlers.register(&link, &classes.program, false).unwrap();
    assert!(handlers.status(&link, &classes.program).unwrap().current);
    handlers.unregister(&link, &classes.program).unwrap();
    assert_eq!(
        handlers.status(&link, &classes.program).unwrap().owner,
        Owner::Nobody
    );
    assert!(
        CURRENT_USER
            .open(format!(r"{}\handoff", classes.root))
            .is_err()
    );
}
