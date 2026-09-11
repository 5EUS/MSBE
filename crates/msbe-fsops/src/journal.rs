//! The append-only write-ahead journal, one JSON record per line.

use std::{
    collections::BTreeSet,
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    atomic,
    error::{Error, IoResultExt, Result},
    ops::{Operation, Prior},
};

/// Identifies one transaction. Assigned in increasing order and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TxnId(u64);

impl TxnId {
    /// The numeric id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// A transaction started.
    Begin {
        /// The transaction.
        txn: TxnId,
    },
    /// An operation is about to run. This record is on disk before the filesystem changes.
    Op {
        /// The transaction.
        txn: TxnId,
        /// The operation's position within the transaction.
        index: usize,
        /// The operation.
        operation: Operation,
        /// What rollback restores.
        prior: Prior,
    },
    /// Every operation in the transaction completed.
    Commit {
        /// The transaction.
        txn: TxnId,
    },
    /// The transaction was undone.
    RolledBack {
        /// The transaction.
        txn: TxnId,
    },
}

impl Record {
    /// The transaction this record belongs to.
    pub const fn txn(&self) -> TxnId {
        match *self {
            Self::Begin { txn }
            | Self::Op { txn, .. }
            | Self::Commit { txn }
            | Self::RolledBack { txn } => txn,
        }
    }
}

/// An append-only journal file with its records loaded.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    file: File,
    records: Vec<Record>,
}

impl Journal {
    /// Opens the journal at `path`, creating it if absent, and loads its records.
    ///
    /// A final line with no terminating newline is a write that never completed. It is
    /// discarded and truncated away, even if it ends mid-character. Any other unreadable
    /// line is corruption.
    ///
    /// # Errors
    ///
    /// Returns [`Error::JournalCorrupt`] for an unreadable complete line, or [`Error::Io`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .at("open", &path)?;
        atomic::sync_parent(&path)?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).at("read", &path)?;
        let mut records = Vec::new();
        let mut valid = 0_usize;
        for (number, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            let Some(body) = line.strip_suffix(b"\n") else {
                break;
            };
            let record = serde_json::from_slice(body).map_err(|e| Error::JournalCorrupt {
                path: path.clone(),
                line: number + 1,
                reason: e.to_string(),
            })?;
            records.push(record);
            valid += line.len();
        }
        if valid < bytes.len() {
            file.set_len(valid as u64)
                .at("truncate torn record", &path)?;
            file.sync_all().at("fsync", &path)?;
        }
        Ok(Self {
            path,
            file,
            records,
        })
    }

    /// The journal file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every record, oldest first.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Appends a record, returning only once it is flushed to disk.
    pub(crate) fn append(&mut self, record: Record) -> Result<()> {
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        self.file.write_all(&line).at("append", &self.path)?;
        self.file.sync_data().at("fsync", &self.path)?;
        self.records.push(record);
        Ok(())
    }

    /// The id the next transaction will use.
    pub fn next_txn(&self) -> TxnId {
        TxnId(
            self.records
                .iter()
                .map(|record| record.txn().0)
                .max()
                .map_or(1, |max| max + 1),
        )
    }

    /// Transactions that began but neither committed nor rolled back, oldest first.
    pub fn open_transactions(&self) -> Vec<TxnId> {
        let mut open = BTreeSet::new();
        for record in &self.records {
            match record {
                Record::Begin { txn } => {
                    open.insert(*txn);
                }
                Record::Commit { txn } | Record::RolledBack { txn } => {
                    open.remove(txn);
                }
                Record::Op { .. } => {}
            }
        }
        open.into_iter().collect()
    }

    /// Transactions that committed and were not later rolled back, oldest first.
    pub fn live_transactions(&self) -> Vec<TxnId> {
        let mut live = BTreeSet::new();
        for record in &self.records {
            match record {
                Record::Commit { txn } => {
                    live.insert(*txn);
                }
                Record::RolledBack { txn } => {
                    live.remove(txn);
                }
                Record::Begin { .. } | Record::Op { .. } => {}
            }
        }
        live.into_iter().collect()
    }

    /// The operations recorded for `txn`, in the order they ran.
    pub(crate) fn operations(&self, txn: TxnId) -> Vec<(usize, Operation, Prior)> {
        self.records
            .iter()
            .filter_map(|record| match record {
                Record::Op {
                    txn: owner,
                    index,
                    operation,
                    prior,
                } if *owner == txn => Some((*index, operation.clone(), prior.clone())),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write};

    use super::{Journal, Record, TxnId};
    use crate::{Error, Operation, Prior, RelPath};

    fn op(txn: u64, index: usize) -> Record {
        Record::Op {
            txn: TxnId(txn),
            index,
            operation: Operation::Remove {
                path: RelPath::new("a/b.txt").unwrap(),
            },
            prior: Prior::Absent,
        }
    }

    #[test]
    fn records_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.next_txn(), TxnId(1));
        journal.append(Record::Begin { txn: TxnId(1) }).unwrap();
        journal.append(op(1, 0)).unwrap();
        drop(journal);

        let reopened = Journal::open(&path).unwrap();
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.open_transactions(), [TxnId(1)]);
        assert_eq!(reopened.next_txn(), TxnId(2));
    }

    #[test]
    fn a_torn_final_record_is_discarded_and_appends_continue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        journal.append(Record::Begin { txn: TxnId(1) }).unwrap();
        drop(journal);

        // A write that died mid-record, cut inside a two-byte character.
        let torn = "{\"kind\":\"op\",\"txn\":1,\"path\":\"caf\u{e9}";
        let bytes = torn.as_bytes();
        let cut = bytes.get(..bytes.len() - 1).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(cut)
            .unwrap();

        let mut reopened = Journal::open(&path).unwrap();
        assert_eq!(reopened.records(), [Record::Begin { txn: TxnId(1) }]);
        reopened.append(Record::Commit { txn: TxnId(1) }).unwrap();
        drop(reopened);

        let again = Journal::open(&path).unwrap();
        assert_eq!(again.records().len(), 2);
        assert!(again.open_transactions().is_empty());
    }

    #[test]
    fn an_unreadable_interior_record_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        std::fs::write(&path, "not json\n{\"kind\":\"begin\",\"txn\":1}\n").unwrap();
        assert!(matches!(
            Journal::open(&path),
            Err(Error::JournalCorrupt { line: 1, .. })
        ));
    }

    #[test]
    fn live_transactions_exclude_rolled_back_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path().join("journal.jsonl")).unwrap();
        for txn in 1..=2 {
            journal.append(Record::Begin { txn: TxnId(txn) }).unwrap();
            journal.append(Record::Commit { txn: TxnId(txn) }).unwrap();
        }
        journal
            .append(Record::RolledBack { txn: TxnId(2) })
            .unwrap();
        assert_eq!(journal.live_transactions(), [TxnId(1)]);
        assert_eq!(journal.next_txn(), TxnId(3));
    }
}
