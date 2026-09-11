//! Crash injection: interrupt a transaction at every checkpoint, restart, recover, and
//! require the instance to match its original state exactly.
//!
//! `docs/12-testing-and-release.md` names this as the project's highest-value suite.

use crate::{
    Applier, Checkpoint, Error, NoopObserver, Observer, Operation, Result,
    test_support::{Fixture, rel, snapshot},
};

/// Collects every checkpoint of an uninterrupted run.
#[derive(Default)]
struct Recorder(Vec<Checkpoint>);

impl Observer for Recorder {
    fn checkpoint(&mut self, at: Checkpoint) -> Result<()> {
        self.0.push(at);
        Ok(())
    }
}

/// Stops the transaction at one checkpoint, as if the process died there.
struct CrashAt(Checkpoint);

impl Observer for CrashAt {
    fn checkpoint(&mut self, at: Checkpoint) -> Result<()> {
        if at == self.0 {
            Err(Error::Aborted(at))
        } else {
            Ok(())
        }
    }
}

/// One transaction covering every operation kind and every prior state: a new file in new
/// nested directories, overwrites of a vanilla file, an executable and a read-only file, a
/// removal, a mutable (copied) file, and removing an empty directory.
fn scenario(applier: &Applier) -> Vec<Operation> {
    let store = applier.store();
    let module = store.put_bytes(b"new module").unwrap();
    let replacement = store.put_bytes(b"replacement pack").unwrap();
    let settings = store.put_bytes(b"enabled = true\n").unwrap();
    vec![
        Operation::Materialize {
            path: rel("mods/new/nested/module.pak"),
            blob: module,
            mutable: false,
        },
        Operation::Materialize {
            path: rel("data/packs/base.pak"),
            blob: replacement,
            mutable: false,
        },
        Operation::Materialize {
            path: rel("bin/run.sh"),
            blob: replacement,
            mutable: false,
        },
        Operation::Materialize {
            path: rel("data/packs/locked.pak"),
            blob: module,
            mutable: false,
        },
        Operation::Remove {
            path: rel("data/packs/extra.pak"),
        },
        Operation::Materialize {
            path: rel("mods/new/settings.cfg"),
            blob: settings,
            mutable: true,
        },
        Operation::RemoveDir { path: rel("logs") },
    ]
}

#[test]
fn every_checkpoint_recovers_to_the_exact_original_state() {
    let reference = Fixture::new();
    let mut uninterrupted = reference.applier();
    let operations = scenario(&uninterrupted);
    let mut recorder = Recorder::default();
    uninterrupted.apply(&operations, &mut recorder).unwrap();
    let checkpoints = recorder.0;
    let finished = snapshot(&reference.root);
    // Implicit directories and Staged checkpoints mean far more checkpoints than operations.
    assert!(checkpoints.len() > operations.len() * 2);

    for crash in checkpoints {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.root);
        {
            let mut applier = fixture.applier();
            let operations = scenario(&applier);
            let error = applier.apply(&operations, &mut CrashAt(crash)).unwrap_err();
            assert!(matches!(error, Error::Aborted(at) if at == crash));
        }

        // A new process starts, sees the open transaction, and recovers.
        let mut restarted = fixture.applier();
        assert_eq!(restarted.recover().unwrap().len(), 1, "crash at {crash:?}");
        assert_eq!(
            snapshot(&fixture.root),
            original,
            "crash at {crash:?} did not restore the original state"
        );
        assert!(
            restarted.recover().unwrap().is_empty(),
            "recovery after {crash:?} is not idempotent"
        );

        // The store and journal are still fully usable afterwards.
        let operations = scenario(&restarted);
        restarted.apply(&operations, &mut NoopObserver).unwrap();
        assert_eq!(
            snapshot(&fixture.root),
            finished,
            "re-applying after a crash at {crash:?} diverged"
        );
    }
}
