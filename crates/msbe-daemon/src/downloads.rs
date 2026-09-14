//! The daemon-owned download queue (`docs/03-architecture.md` §3.4).
//!
//! Each item is a requested source and the group of files it resolves to. The queue is persisted
//! in `downloads/queue.json`, and downloaded files wait in `downloads/<id>/`. Items move through
//! two lanes:
//!
//! - The network lane resolves sources and downloads files into quarantine. It holds the
//!   instance-state lock only while it reads the profile. Links a browser hands over are redeemed
//!   on a lane of their own, as soon as they arrive, because their keys expire.
//! - The instance lane adds an item's files to its profile together, once every one of them has
//!   downloaded, as a short job on the job queue.
//!
//! A long queue therefore never holds up a deploy, and a group is never half added.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt, fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
};

use msbe_core::{
    config::Home,
    instance::{Artifact, Instance, Name, Profile, Provenance},
};
use msbe_fsops::{
    RelPath,
    atomic::{rename_replace, write_file},
};
use msbe_pack::{IssueCode, PackError};
use msbe_provider_api::{
    AdapterError, ArtifactDescriptor, DOWNLOAD_LIMIT, HttpClient, ManifestError, Target, acquire,
    hex,
    model::{Download, HandoffTicket, Request, Selection},
    resolve::{InstallPlan, ProjectRequest, Resolver},
};
use msbe_providers::{Providers, RegistryError, Routed};
use msbe_rpc_schema::{
    DownloadConfirm, DownloadEnqueue, DownloadFile, DownloadFileState, DownloadItem, DownloadList,
    DownloadMove, DownloadState, DownloadTarget, HandoffReceipt,
};
use msbe_secrets::{Clock, Secret, SystemClock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Connector,
    jobs::{Environment, Jobs, Work},
};

/// The job method that adds a downloaded item to its profile.
pub(crate) const ADD_JOB: &str = "download.add";

/// The queue file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Queue {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    items: Vec<Entry>,
}

/// An item, and what the daemon keeps about its files that clients never see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    item: DownloadItem,
    /// One record for each of the item's files, in the same order.
    #[serde(default)]
    staged: Vec<Staged>,
}

/// What the daemon keeps about one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Staged {
    /// The downloaded file, in quarantine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    /// What is recorded in the profile about where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<Provenance>,
    /// The project's slug, which names the mod; the file stem does when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module: Option<String>,
    /// The release's version number, when resolution read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    number: Option<String>,
    /// The page the user starts its download on, to wait on again if its link is lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<String>,
    /// The URI scheme of the link that page hands over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheme: Option<String>,
}

impl Queue {
    fn load(home: &Home) -> Result<Self, String> {
        let path = queue_file(home);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                format!("cannot read the download queue {}: {error}", path.display())
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!(
                "cannot read the download queue {}: {error}",
                path.display()
            )),
        }
    }

    fn save(&self, home: &Home) -> Result<(), String> {
        let directory = queue_directory(home);
        fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode the download queue: {error}"))?;
        let path = queue_file(home);
        write_file(&path, &bytes).map_err(|error| {
            format!(
                "cannot write the download queue {}: {error}",
                path.display()
            )
        })
    }

    /// Item `id`, marked as changed in a new revision.
    fn touch(&mut self, id: u64) -> Result<&mut Entry, String> {
        self.revision += 1;
        let revision = self.revision;
        let entry = self
            .items
            .iter_mut()
            .find(|entry| entry.item.id == id)
            .ok_or_else(|| format!("there is no download {id}"))?;
        entry.item.revision = revision;
        Ok(entry)
    }

    /// Appends `entry` under a new ID and returns its item.
    fn insert(&mut self, mut entry: Entry) -> DownloadItem {
        self.next_id += 1;
        self.revision += 1;
        entry.item.id = self.next_id;
        entry.item.revision = self.revision;
        let item = entry.item.clone();
        self.items.push(entry);
        item
    }

    /// The items changed after revision `after`, or every item for a revision this queue never
    /// reached.
    fn list(&self, after: u64) -> DownloadList {
        let after = if after > self.revision { 0 } else { after };
        DownloadList {
            next: self.revision,
            paused: self.paused,
            order: self.items.iter().map(|entry| entry.item.id).collect(),
            items: self
                .items
                .iter()
                .filter(|entry| entry.item.revision > after)
                .map(|entry| entry.item.clone())
                .collect(),
        }
    }

    /// Takes the first queued item for the network lane, unless the queue is paused.
    fn claim(&mut self) -> Option<Claim> {
        if self.paused {
            return None;
        }
        let id = self
            .items
            .iter()
            .find(|entry| {
                entry.item.state == DownloadState::Queued
                    && entry.item.source.is_some()
                    && entry.item.target.is_some()
            })?
            .item
            .id;
        let entry = self.touch(id).ok()?;
        entry.item.state = DownloadState::Resolving;
        entry.item.attempts += 1;
        let kept = entry
            .item
            .files
            .iter()
            .zip(&entry.staged)
            .filter(|(file, staged)| {
                file.state == DownloadFileState::Downloaded && staged.path.is_some()
            })
            .map(|(file, staged)| (file.clone(), staged.clone()))
            .collect();
        Some(Claim {
            id,
            attempts: entry.item.attempts,
            source: entry.item.source.clone()?,
            target: entry.item.target.clone()?,
            with_deps: entry.item.with_deps,
            kept,
        })
    }

    /// Records a link: the file an unfinished item waits on, or else a new item of its own that
    /// waits for a profile. Returns the item, the file's index, and whether an item was waiting.
    fn accept(&mut self, ticket: &HandoffTicket) -> (u64, usize, bool) {
        let waiting = self.items.iter().find_map(|entry| {
            let open = matches!(
                entry.item.state,
                DownloadState::AwaitingUser { .. }
                    | DownloadState::Downloading
                    | DownloadState::Paused
            );
            let index = entry.item.files.iter().position(|file| {
                matches!(file.state, DownloadFileState::AwaitingUser { .. })
                    && file.provider == ticket.provider
                    && file.project == ticket.project
                    && file.release == ticket.release
            })?;
            open.then_some((entry.item.id, index))
        });
        if let Some((id, index)) = waiting
            && let Ok(entry) = self.touch(id)
            && let Some(file) = entry.item.files.get_mut(index)
        {
            file.state = DownloadFileState::Downloading;
            settle(&mut entry.item);
            return (id, index, true);
        }
        let mut item = blank(DownloadState::Downloading);
        item.attempts = 1;
        item.files.push(DownloadFile {
            provider: ticket.provider.clone(),
            project: ticket.project.clone(),
            release: ticket.release.clone(),
            name: String::new(),
            size: None,
            state: DownloadFileState::Downloading,
        });
        let item = self.insert(Entry {
            item,
            staged: vec![Staged::default()],
        });
        (item.id, 0, false)
    }
}

/// An item the network lane took.
#[derive(Debug)]
struct Claim {
    id: u64,
    attempts: u32,
    source: String,
    target: DownloadTarget,
    with_deps: bool,
    /// Files an earlier attempt downloaded, which are kept if resolution picks them again.
    kept: Vec<(DownloadFile, Staged)>,
}

/// A link to redeem. It carries a key, so it is kept only in memory.
#[derive(Debug)]
struct Link {
    item: u64,
    file: usize,
    uri: Secret,
}

#[derive(Debug, Default)]
struct Signals {
    /// Whether the network lane may have something to do.
    network: bool,
    /// Links waiting to be redeemed, oldest first.
    links: VecDeque<Link>,
}

/// The download queue, shared by the listener and the lanes.
#[derive(Debug, Default)]
pub struct Downloads {
    queue: Mutex<()>,
    signals: Mutex<Signals>,
    network_ready: Condvar,
    link_ready: Condvar,
}

impl Downloads {
    /// Loads the queue, applies `change`, and saves the queue if `change` succeeds.
    fn change<T>(
        &self,
        home: &Home,
        change: impl FnOnce(&mut Queue) -> Result<T, String>,
    ) -> Result<T, String> {
        let _queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let mut queue = Queue::load(home)?;
        let value = change(&mut queue)?;
        queue.save(home)?;
        Ok(value)
    }

    fn read<T>(&self, home: &Home, read: impl FnOnce(&Queue) -> T) -> Result<T, String> {
        let _queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        Queue::load(home).map(|queue| read(&queue))
    }

    fn signals(&self) -> MutexGuard<'_, Signals> {
        self.signals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake_network(&self) {
        self.signals().network = true;
        self.network_ready.notify_one();
    }

    fn push_link(&self, link: Link) {
        self.signals().links.push_back(link);
        self.link_ready.notify_one();
    }
}

/// The queue and what its lanes run against: the data directory, the network, and the job queue
/// that is the instance lane.
#[derive(Clone)]
pub struct Lanes {
    pub(crate) downloads: Arc<Downloads>,
    pub(crate) jobs: Arc<Jobs>,
    pub(crate) home: Option<PathBuf>,
    pub(crate) connect: Connector,
}

impl fmt::Debug for Lanes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Lanes")
            .field("downloads", &self.downloads)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl Lanes {
    pub(crate) fn home(&self) -> Result<Home, String> {
        self.home
            .as_ref()
            .map_or_else(Home::discover, |home| Ok(Home::at(home)))
            .map_err(|error| error.to_string())
    }

    /// `download.enqueue`.
    pub(crate) fn enqueue(&self, request: DownloadEnqueue) -> Result<DownloadItem, String> {
        let home = self.home()?;
        check_target(&home, &request.instance, &request.profile)?;
        check_source(&home, &request.source)?;
        let item = self.downloads.change(&home, |queue| {
            let queued = queue.items.iter().find(|entry| {
                !entry.item.state.is_finished()
                    && entry.item.source.as_deref() == Some(request.source.as_str())
                    && entry.item.target.as_ref().is_some_and(|target| {
                        target.instance == request.instance && target.profile == request.profile
                    })
            });
            if let Some(entry) = queued {
                return Ok(entry.item.clone());
            }
            let mut item = blank(DownloadState::Queued);
            item.title = request.title;
            item.source = Some(request.source);
            item.target = Some(DownloadTarget {
                instance: request.instance,
                profile: request.profile,
            });
            item.with_deps = request.with_deps;
            Ok(queue.insert(Entry {
                item,
                staged: Vec::new(),
            }))
        })?;
        self.downloads.wake_network();
        Ok(item)
    }

    /// `download.list`.
    pub(crate) fn list(&self, after: u64) -> Result<DownloadList, String> {
        let home = self.home()?;
        self.downloads.read(&home, |queue| queue.list(after))
    }

    /// `download.pause`: one item before its next network step, or the whole network lane.
    pub(crate) fn pause(&self, id: Option<u64>) -> Result<DownloadList, String> {
        let home = self.home()?;
        self.downloads.change(&home, |queue| {
            if let Some(id) = id {
                let entry = queue.touch(id)?;
                let pausable = matches!(
                    entry.item.state,
                    DownloadState::Queued
                        | DownloadState::Resolving
                        | DownloadState::Downloading
                        | DownloadState::AwaitingUser { .. }
                );
                if !pausable || entry.item.source.is_none() {
                    return Err(refusal(&entry.item, "pause"));
                }
                entry.item.state = DownloadState::Paused;
            } else {
                queue.paused = true;
                queue.revision += 1;
            }
            Ok(queue.list(0))
        })
    }

    /// `download.resume`.
    pub(crate) fn resume(&self, id: Option<u64>) -> Result<DownloadList, String> {
        let home = self.home()?;
        let list = self.downloads.change(&home, |queue| {
            if let Some(id) = id {
                let entry = queue.touch(id)?;
                if entry.item.state != DownloadState::Paused {
                    return Err(refusal(&entry.item, "resume"));
                }
                entry.item.state = DownloadState::Queued;
            } else {
                queue.paused = false;
                queue.revision += 1;
            }
            Ok(queue.list(0))
        })?;
        self.downloads.wake_network();
        Ok(list)
    }

    /// `download.cancel`.
    pub(crate) fn cancel(&self, id: u64) -> Result<DownloadItem, String> {
        let home = self.home()?;
        self.downloads.change(&home, |queue| {
            let entry = queue.touch(id)?;
            if entry.item.state.is_finished() || entry.item.state == DownloadState::Adding {
                return Err(refusal(&entry.item, "cancel"));
            }
            entry.item.state = DownloadState::Cancelled;
            Ok(entry.item.clone())
        })
    }

    /// `download.retry`: an item whose files all downloaded is added again; any other is resolved
    /// again, keeping the files it already has.
    pub(crate) fn retry(&self, id: u64) -> Result<DownloadItem, String> {
        let home = self.home()?;
        let item = self.downloads.change(&home, |queue| {
            let entry = queue.touch(id)?;
            let item = &mut entry.item;
            if !matches!(
                item.state,
                DownloadState::Failed { .. } | DownloadState::Cancelled
            ) {
                return Err(refusal(item, "retry"));
            }
            let downloaded = !item.files.is_empty()
                && item
                    .files
                    .iter()
                    .all(|file| file.state == DownloadFileState::Downloaded);
            item.state = if downloaded {
                DownloadState::Downloaded
            } else if item.source.is_some() {
                DownloadState::Queued
            } else {
                return Err(format!(
                    "download {id} came from a link that was not redeemed; open the link again"
                ));
            };
            Ok(item.clone())
        })?;
        self.advance(&item);
        Ok(item)
    }

    /// `download.move`.
    pub(crate) fn move_item(&self, request: DownloadMove) -> Result<DownloadList, String> {
        let home = self.home()?;
        self.downloads.change(&home, |queue| {
            let index = queue
                .items
                .iter()
                .position(|entry| entry.item.id == request.id)
                .ok_or_else(|| format!("there is no download {}", request.id))?;
            if request.position >= queue.items.len() {
                return Err(format!(
                    "position {} is past the end of the queue",
                    request.position
                ));
            }
            let entry = queue.items.remove(index);
            queue.items.insert(request.position, entry);
            queue.revision += 1;
            Ok(queue.list(0))
        })
    }

    /// `download.confirm`: the profile for an item a link created on its own.
    pub(crate) fn confirm(&self, request: DownloadConfirm) -> Result<DownloadItem, String> {
        let home = self.home()?;
        check_target(&home, &request.instance, &request.profile)?;
        let item = self.downloads.change(&home, |queue| {
            let entry = queue.touch(request.id)?;
            if entry.item.target.is_some() || entry.item.state.is_finished() {
                return Err(refusal(&entry.item, "take another profile"));
            }
            entry.item.target = Some(DownloadTarget {
                instance: request.instance,
                profile: request.profile,
            });
            Ok(entry.item.clone())
        })?;
        self.advance(&item);
        Ok(item)
    }

    /// `download.clear`.
    pub(crate) fn clear(&self) -> Result<DownloadList, String> {
        let home = self.home()?;
        let (list, cleared) = self.downloads.change(&home, |queue| {
            let (finished, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut queue.items)
                .into_iter()
                .partition(|entry| entry.item.state.is_finished());
            queue.items = kept;
            queue.revision += 1;
            let cleared: Vec<u64> = finished.iter().map(|entry| entry.item.id).collect();
            Ok((queue.list(0), cleared))
        })?;
        for id in cleared {
            discard(&quarantine(&home, id));
        }
        Ok(list)
    }

    /// `handoff.submit`: reads a link against its provider's declared structure, records the file
    /// it fills, and hands it to the link lane.
    pub(crate) fn submit_link(
        &self,
        uri: String,
        clock: &dyn Clock,
    ) -> Result<HandoffReceipt, String> {
        let home = self.home()?;
        let providers = Providers::installed(&home).map_err(|error| error.to_string())?;
        let ticket = providers
            .handoff(&uri)
            .map_err(|error| error.to_string())?
            .parse(&uri, clock.now())
            .map_err(|error| error.to_string())?;
        let uri = Secret::new(uri).map_err(|error| format!("the link cannot be kept: {error}"))?;
        let (id, file, matched) = self
            .downloads
            .change(&home, |queue| Ok(queue.accept(&ticket)))?;
        self.downloads.push_link(Link {
            item: id,
            file,
            uri,
        });
        Ok(HandoffReceipt {
            id,
            provider: ticket.provider,
            game: ticket.game,
            project: ticket.project,
            release: ticket.release,
            matched,
        })
    }

    /// Every file waiting for the user to start its download on a page, in queue order. Paused and
    /// finished items are left out.
    pub(crate) fn waiting(&self) -> Result<Vec<Waiting>, String> {
        let home = self.home()?;
        self.downloads.read(&home, |queue| {
            queue
                .items
                .iter()
                .filter(|entry| {
                    !entry.item.state.is_finished() && entry.item.state != DownloadState::Paused
                })
                .flat_map(|entry| {
                    entry
                        .item
                        .files
                        .iter()
                        .enumerate()
                        .filter_map(|(index, file)| match &file.state {
                            DownloadFileState::AwaitingUser { page, scheme } => Some(Waiting {
                                item: entry.item.id,
                                file: index,
                                provider: file.provider.clone(),
                                page: page.clone(),
                                scheme: scheme.clone(),
                            }),
                            _ => None,
                        })
                })
                .collect()
        })
    }

    /// Fills `waiting` with the download the MSBE browser captured on its page. The file is moved
    /// into the item's quarantine under `name` when that is a safe file name, the same rule
    /// provider downloads follow, and its SHA-256 and SHA-512 are recorded as provenance, since the
    /// page published none MSBE could check.
    pub(crate) fn capture(
        &self,
        waiting: &Waiting,
        captured: &Path,
        name: Option<String>,
    ) -> Result<(), String> {
        let home = self.home()?;
        let name = name
            .and_then(|name| RelPath::new(&name).ok())
            .filter(|name| !name.as_str().contains('/'))
            .map_or_else(|| "download".to_owned(), |name| name.as_str().to_owned());
        let size = fs::symlink_metadata(captured)
            .map_err(|error| format!("cannot read the captured download: {error}"))?
            .len();
        if size > DOWNLOAD_LIMIT {
            return Err("the captured download is larger than 2 GiB".to_owned());
        }
        let (sha256, sha512) = digests(captured)?;
        let slot = quarantine(&home, waiting.item).join(format!("capture-{}", waiting.file));
        reset(&slot)?;
        let path = slot.join(&name);
        rename_replace(captured, &path).map_err(|error| {
            format!("cannot move the captured download into the queue: {error}")
        })?;
        let recorded = self.downloads.change(&home, |queue| {
            let entry = queue.touch(waiting.item)?;
            let open = !entry.item.state.is_finished() && entry.item.state != DownloadState::Paused;
            let (Some(listed), Some(staged)) = (
                entry.item.files.get_mut(waiting.file),
                entry.staged.get_mut(waiting.file),
            ) else {
                return Err(format!("download {} no longer has that file", waiting.item));
            };
            if !open
                || listed.provider != waiting.provider
                || !matches!(listed.state, DownloadFileState::AwaitingUser { .. })
            {
                return Err(format!(
                    "download {} no longer waits on that page",
                    waiting.item
                ));
            }
            listed.name.clone_from(&name);
            listed.size = Some(size);
            listed.state = DownloadFileState::Downloaded;
            staged.provenance = Some(Provenance {
                provider: listed.provider.clone(),
                project: listed.project.clone(),
                version: listed.release.clone(),
                version_number: staged
                    .number
                    .clone()
                    .unwrap_or_else(|| listed.release.clone()),
                hashes: BTreeMap::from([
                    ("sha256".to_owned(), sha256.clone()),
                    ("sha512".to_owned(), sha512.clone()),
                ]),
            });
            staged.path = Some(path.clone());
            settle(&mut entry.item);
            Ok(is_ready(&entry.item))
        });
        match recorded {
            Ok(ready) => {
                if ready {
                    self.submit_add(waiting.item);
                }
                Ok(())
            }
            Err(message) => {
                discard(&slot);
                Err(message)
            }
        }
    }

    /// Resolves and downloads the first queued item, returning whether there was one.
    pub fn run_next(&self) -> bool {
        let Ok(home) = self.home() else {
            return false;
        };
        let Ok(Some(claim)) = self.downloads.change(&home, |queue| Ok(queue.claim())) else {
            return false;
        };
        let id = claim.id;
        if let Err(message) = self.fetch(&home, &claim) {
            self.fail(&home, id, message, None);
        }
        true
    }

    /// Blocks until an item may be queued, then runs every queued item.
    pub fn wait_and_run(&self) {
        let mut signals = self.downloads.signals();
        while !signals.network {
            signals = self
                .downloads
                .network_ready
                .wait(signals)
                .unwrap_or_else(PoisonError::into_inner);
        }
        signals.network = false;
        drop(signals);
        while self.run_next() {}
    }

    /// Redeems and downloads the oldest link, returning whether there was one.
    pub fn run_next_link(&self) -> bool {
        let link = self.downloads.signals().links.pop_front();
        link.is_some_and(|link| {
            self.redeem(&link);
            true
        })
    }

    /// Blocks until a link arrives, then redeems and downloads it.
    pub fn wait_and_run_link(&self) {
        let mut signals = self.downloads.signals();
        let link = loop {
            if let Some(link) = signals.links.pop_front() {
                break link;
            }
            signals = self
                .downloads
                .link_ready
                .wait(signals)
                .unwrap_or_else(PoisonError::into_inner);
        };
        drop(signals);
        self.redeem(&link);
    }

    /// Puts back what a daemon that stopped part-way left: interrupted resolutions and downloads
    /// run again, files whose link was lost wait for the user again, and downloaded items are
    /// added.
    pub fn recover(&self) {
        let Ok(home) = self.home() else {
            return;
        };
        let ready = self.downloads.change(&home, |queue| {
            let mut revision = queue.revision;
            for entry in &mut queue.items {
                if recover(entry) {
                    revision += 1;
                    entry.item.revision = revision;
                }
            }
            queue.revision = revision;
            Ok(queue
                .items
                .iter()
                .filter(|entry| is_ready(&entry.item))
                .map(|entry| entry.item.id)
                .collect::<Vec<_>>())
        });
        for id in ready.unwrap_or_default() {
            self.submit_add(id);
        }
        self.downloads.wake_network();
    }

    /// Queues the add for an item whose files are all downloaded, or wakes the network lane for
    /// one queued again.
    fn advance(&self, item: &DownloadItem) {
        if is_ready(item) {
            self.submit_add(item.id);
        } else if item.state == DownloadState::Queued {
            self.downloads.wake_network();
        }
    }

    fn submit_add(&self, id: u64) {
        self.jobs.submit(
            ADD_JOB,
            Work::DownloadAdd { id },
            Environment {
                home: self.home.clone(),
                connect: Arc::clone(&self.connect),
                downloads: Arc::clone(&self.downloads),
            },
        );
    }

    fn fetch(&self, home: &Home, claim: &Claim) -> Result<(), String> {
        let providers = Providers::installed(home).map_err(|error| error.to_string())?;
        let http = (self.connect)().map_err(|error| error.to_string())?;
        let (existing, target) = self.read_profile(home, &claim.target)?;
        let planned = plan(&providers, http.as_ref(), claim, &existing, target)?;
        let fetch = Fetch {
            lanes: self,
            home,
            providers: &providers,
            http: http.as_ref(),
            claim,
        };
        let Some(pending) = fetch.stage(planned)? else {
            return Ok(());
        };
        for (index, selection) in &pending {
            if !fetch.file(*index, selection)? {
                return Ok(());
            }
        }
        self.settle(home, claim.id)
    }

    /// The profile and its target. Opening an instance recovers an interrupted transaction, so
    /// this is the one step of the network lane that holds the instance-state lock.
    fn read_profile(&self, home: &Home, target: &DownloadTarget) -> Result<ProfileRead, String> {
        let _state = self.jobs.lock_state();
        let name = Name::new(&target.instance).map_err(|error| error.to_string())?;
        let instance = Instance::open(home, &name).map_err(|error| error.to_string())?;
        let profile = Name::new(&target.profile).map_err(|error| error.to_string())?;
        let existing = instance
            .profile(&profile)
            .map_err(|error| error.to_string())?;
        let resolved = msbe_cli::target(&instance, &profile).map_err(|error| error.to_string());
        Ok((existing, resolved))
    }

    /// Settles item `id` after one of its files changed, and queues the add once every file has
    /// downloaded.
    fn settle(&self, home: &Home, id: u64) -> Result<(), String> {
        let ready = self.downloads.change(home, |queue| {
            let entry = queue.touch(id)?;
            settle(&mut entry.item);
            Ok(is_ready(&entry.item))
        })?;
        if ready {
            self.submit_add(id);
        }
        Ok(())
    }

    /// Records why item `id` stopped, unless the user paused or cancelled it meanwhile. A file
    /// whose link failed waits for the user again.
    fn fail(&self, home: &Home, id: u64, message: String, link: Option<usize>) {
        let recorded = self.downloads.change(home, |queue| {
            let entry = queue.touch(id)?;
            if let Some(index) = link
                && let (Some(file), Some(staged)) =
                    (entry.item.files.get_mut(index), entry.staged.get(index))
                && file.state == DownloadFileState::Downloading
            {
                file.state = waiting_state(staged);
            }
            if matches!(
                entry.item.state,
                DownloadState::Resolving
                    | DownloadState::Downloading
                    | DownloadState::AwaitingUser { .. }
            ) {
                entry.item.state = DownloadState::Failed { message };
            }
            Ok(())
        });
        // The lane has no one to report a queue it cannot write to; the item stays as it was.
        drop(recorded);
    }

    fn redeem(&self, link: &Link) {
        let Ok(home) = self.home() else {
            return;
        };
        if let Err(message) = self.redeem_link(&home, link) {
            self.fail(&home, link.item, message, Some(link.file));
        }
    }

    fn redeem_link(&self, home: &Home, link: &Link) -> Result<(), String> {
        let providers = Providers::installed(home).map_err(|error| error.to_string())?;
        let handoff = providers
            .handoff(link.uri.expose())
            .map_err(|error| error.to_string())?;
        let ticket = handoff
            .parse(link.uri.expose(), SystemClock.now())
            .map_err(|error| error.to_string())?;
        let http = (self.connect)().map_err(|error| error.to_string())?;
        let file = handoff
            .redeem(http.as_ref(), &ticket)
            .map_err(|error| error.to_string())?;
        let Download::Direct { url } = &file.download else {
            return Err(format!(
                "{} did not answer the link with a download",
                ticket.provider
            ));
        };
        let slot = quarantine(home, link.item).join(format!("link-{}", link.file));
        reset(&slot)?;
        let acquired = acquire(
            http.as_ref(),
            &ArtifactDescriptor {
                url: url.clone(),
                file_name: file.name.clone(),
                limit: file.limit.unwrap_or(DOWNLOAD_LIMIT),
                size: file.size,
                md5: file.md5.clone(),
                sha1: file.sha1.clone(),
                sha256: file.sha256.clone(),
                sha512: file.sha512.clone(),
            },
            &slot,
        )
        .map_err(|error| error.to_string())?;
        let ready = self.downloads.change(home, |queue| {
            let entry = queue.touch(link.item)?;
            let (Some(listed), Some(staged)) = (
                entry.item.files.get_mut(link.file),
                entry.staged.get_mut(link.file),
            ) else {
                return Ok(false);
            };
            if entry.item.state.is_finished()
                || listed.state != DownloadFileState::Downloading
                || listed.release != ticket.release
            {
                return Ok(false);
            }
            listed.name.clone_from(&file.name);
            listed.size = Some(acquired.size);
            listed.state = DownloadFileState::Downloaded;
            staged.provenance = Some(Provenance {
                provider: ticket.provider.clone(),
                project: ticket.project.clone(),
                version: ticket.release.clone(),
                version_number: staged
                    .number
                    .clone()
                    .unwrap_or_else(|| ticket.release.clone()),
                hashes: BTreeMap::from([
                    ("sha256".to_owned(), acquired.sha256.clone()),
                    ("sha512".to_owned(), acquired.sha512.clone()),
                ]),
            });
            staged.path = Some(acquired.path.clone());
            settle(&mut entry.item);
            Ok(is_ready(&entry.item))
        })?;
        if ready {
            self.submit_add(link.item);
        }
        Ok(())
    }
}

/// A file waiting for the user to start its download on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Waiting {
    /// The item.
    pub(crate) item: u64,
    /// The file's index in the item.
    pub(crate) file: usize,
    /// The file's provider.
    pub(crate) provider: String,
    /// The page the user starts the download on.
    pub(crate) page: String,
    /// The scheme of the link the page hands over, when it hands one over.
    pub(crate) scheme: Option<String>,
}

/// The SHA-256 and SHA-512 of the file at `path`, in lowercase hexadecimal.
fn digests(path: &Path) -> Result<(String, String), String> {
    use sha2::{Digest as _, Sha256, Sha512};

    let failed = |error: io::Error| format!("cannot read the captured download: {error}");
    let mut file = fs::File::open(path).map_err(failed)?;
    let (mut sha256, mut sha512) = (Sha256::new(), Sha512::new());
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer).map_err(failed)?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        sha256.update(chunk);
        sha512.update(chunk);
    }
    Ok((hex(&sha256.finalize()), hex(&sha512.finalize())))
}

/// What resolution chose: the files to download, mods the profile already has, and warnings.
#[derive(Debug)]
struct Planned {
    selections: Vec<Selection>,
    skipped: Vec<String>,
    warnings: Vec<String>,
}

fn plan(
    providers: &Providers,
    http: &dyn HttpClient,
    claim: &Claim,
    existing: &Profile,
    target: Result<Target, String>,
) -> Result<Planned, String> {
    let Routed { provider, request } = providers
        .request(&claim.source)
        .map_err(|error| error.to_string())?;
    let (selections, warnings) = match request {
        Request::Project { reference, version } => {
            let target = target?;
            let plan = Resolver {
                adapters: providers,
                http,
                target: &target,
                overlay: providers.overlay(),
            }
            .plan_install(
                &[ProjectRequest {
                    provider,
                    reference,
                    version,
                }],
                claim.with_deps,
                &msbe_cli::installed_releases(existing),
            )
            .map_err(|error| error.to_string())?;
            let warnings = warnings(&plan);
            (plan.selections, warnings)
        }
        Request::File(selection) => (vec![*selection], Vec::new()),
    };
    let mut planned = Planned {
        selections: Vec::new(),
        skipped: Vec::new(),
        warnings,
    };
    for selection in selections {
        let package = &selection.project.id;
        match msbe_cli::installed_from(existing, &package.provider, &package.project) {
            Some(name) => planned.skipped.push(name.to_string()),
            None => planned.selections.push(selection),
        }
    }
    Ok(planned)
}

fn warnings(plan: &InstallPlan) -> Vec<String> {
    let unresolved = plan.unresolved.iter().map(|requirement| {
        format!(
            "{} requires {}:{}; queue it too, or queue with dependencies",
            requirement.declared_by, requirement.provider, requirement.project_id
        )
    });
    let incompatible = plan.incompatible.iter().map(|requirement| {
        format!(
            "{} is incompatible with {}:{}",
            requirement.declared_by, requirement.provider, requirement.project_id
        )
    });
    unresolved.chain(incompatible).collect()
}

/// The files left to fetch, with their indexes among the item's files.
type Pending = Vec<(usize, Selection)>;

/// A profile, and its target or why it has none.
type ProfileRead = (Profile, Result<Target, String>);

/// The mods an add added, and the ones already in the profile.
type Added = (Vec<String>, Vec<String>);

/// One network-lane pass over a claimed item.
struct Fetch<'a> {
    lanes: &'a Lanes,
    home: &'a Home,
    providers: &'a Providers,
    http: &'a dyn HttpClient,
    claim: &'a Claim,
}

/// What became of one file.
enum Fetched {
    Downloaded {
        path: PathBuf,
        size: u64,
        provenance: Provenance,
    },
    Awaiting {
        page: String,
        scheme: Option<String>,
    },
}

impl Fetch<'_> {
    /// Records the files `planned` resolved to, keeping ones an earlier attempt downloaded, and
    /// returns the ones left to fetch with their indexes. `None` when the user paused or cancelled
    /// the item meanwhile.
    fn stage(&self, planned: Planned) -> Result<Option<Pending>, String> {
        let mut files = Vec::new();
        let mut staged = Vec::new();
        let mut pending = Vec::new();
        for (index, selection) in planned.selections.into_iter().enumerate() {
            let package = &selection.project.id;
            let kept = self.claim.kept.iter().find(|(file, _)| {
                file.provider == package.provider
                    && file.project == package.project
                    && file.release == selection.release.id
            });
            if let Some((file, record)) = kept {
                files.push(file.clone());
                staged.push(record.clone());
                continue;
            }
            files.push(DownloadFile {
                provider: package.provider.clone(),
                project: package.project.clone(),
                release: selection.release.id.clone(),
                name: selection.file.name.clone(),
                size: selection.file.size,
                state: DownloadFileState::Pending,
            });
            staged.push(Staged {
                module: selection.project.slug.clone(),
                number: Some(selection.release.number.clone()),
                ..Staged::default()
            });
            pending.push((index, selection));
        }
        let stored = self.lanes.downloads.change(self.home, |queue| {
            let entry = queue.touch(self.claim.id)?;
            if entry.item.state != DownloadState::Resolving {
                return Ok(false);
            }
            entry.item.files = files;
            entry.staged = staged;
            entry.item.skipped = planned.skipped;
            entry.item.warnings = planned.warnings;
            entry.item.state = DownloadState::Downloading;
            Ok(true)
        })?;
        Ok(stored.then_some(pending))
    }

    /// Downloads file `index`, or records that the user starts its download. Returns false when
    /// the user paused or cancelled the item meanwhile.
    fn file(&self, index: usize, selection: &Selection) -> Result<bool, String> {
        let adapter = self
            .providers
            .adapter(&selection.project.id.provider)
            .map_err(|error| error.to_string())?;
        let slot =
            quarantine(self.home, self.claim.id).join(format!("{}-{index}", self.claim.attempts));
        reset(&slot)?;
        let fetched = match adapter.acquire(self.http, &selection.file, &slot) {
            Ok(acquired) => Fetched::Downloaded {
                provenance: adapter.provenance(&selection.release, &acquired),
                size: acquired.size,
                path: acquired.path,
            },
            Err(error) => match &error {
                AdapterError::ActionRequired { download, .. } => match download.as_ref() {
                    Download::BrowserAssisted { page, scheme } => Fetched::Awaiting {
                        page: page.clone(),
                        scheme: Some(scheme.clone()),
                    },
                    // The page hands over the file itself, which the MSBE browser captures.
                    Download::UserAction { page, .. } => Fetched::Awaiting {
                        page: page.clone(),
                        scheme: None,
                    },
                    Download::Direct { .. } => return Err(error.to_string()),
                },
                _ => return Err(error.to_string()),
            },
        };
        self.lanes.downloads.change(self.home, |queue| {
            let entry = queue.touch(self.claim.id)?;
            if entry.item.state != DownloadState::Downloading {
                return Ok(false);
            }
            let (Some(file), Some(staged)) =
                (entry.item.files.get_mut(index), entry.staged.get_mut(index))
            else {
                return Ok(false);
            };
            match fetched {
                Fetched::Downloaded {
                    path,
                    size,
                    provenance,
                } => {
                    file.state = DownloadFileState::Downloaded;
                    file.size = Some(size);
                    staged.path = Some(path);
                    staged.provenance = Some(provenance);
                }
                Fetched::Awaiting { page, scheme } => {
                    staged.page = Some(page);
                    staged.scheme = scheme;
                    file.state = waiting_state(staged);
                }
            }
            Ok(true)
        })
    }
}

/// Adds item `id`'s files to its profile: the instance lane, run as a job that holds the
/// instance-state lock. Mods the profile already has are left as they are.
pub(crate) fn add(downloads: &Downloads, home: &Home, id: u64) -> Result<Value, PackError> {
    let claimed = downloads
        .change(home, |queue| {
            let entry = queue.touch(id)?;
            if !is_ready(&entry.item) {
                return Ok(None);
            }
            entry.item.state = DownloadState::Adding;
            Ok(Some(entry.clone()))
        })
        .map_err(host_failure)?;
    let Some(entry) = claimed else {
        return Ok(Value::Null);
    };
    match add_files(home, &entry) {
        Ok((added, skipped)) => {
            let item = downloads
                .change(home, |queue| {
                    let entry = queue.touch(id)?;
                    entry.item.state = DownloadState::Completed;
                    entry.item.added = added;
                    entry.item.skipped.extend(skipped);
                    Ok(entry.item.clone())
                })
                .map_err(host_failure)?;
            discard(&quarantine(home, id));
            serde_json::to_value(item).map_err(|error| host_failure(error.to_string()))
        }
        Err(message) => {
            let recorded = downloads.change(home, |queue| {
                queue.touch(id)?.item.state = DownloadState::Failed {
                    message: message.clone(),
                };
                Ok(())
            });
            drop(recorded);
            Err(host_failure(message))
        }
    }
}

fn add_files(home: &Home, entry: &Entry) -> Result<Added, String> {
    let target = entry
        .item
        .target
        .as_ref()
        .ok_or_else(|| format!("download {} has no profile", entry.item.id))?;
    let name = Name::new(&target.instance).map_err(|error| error.to_string())?;
    let instance = Instance::open(home, &name).map_err(|error| error.to_string())?;
    let profile = Name::new(&target.profile).map_err(|error| error.to_string())?;
    let existing = instance
        .profile(&profile)
        .map_err(|error| error.to_string())?;
    let mut artifacts = Vec::new();
    let mut skipped = Vec::new();
    for (file, staged) in entry.item.files.iter().zip(&entry.staged) {
        if let Some(name) = msbe_cli::installed_from(&existing, &file.provider, &file.project) {
            skipped.push(name.to_string());
            continue;
        }
        let path = staged
            .path
            .clone()
            .ok_or_else(|| format!("{} was not downloaded", file.name))?;
        let module = staged
            .module
            .as_deref()
            .map(Name::sanitize)
            .transpose()
            .map_err(|error| error.to_string())?;
        artifacts.push(Artifact {
            path,
            module,
            provider: staged.provenance.clone(),
            source: None,
        });
    }
    if artifacts.is_empty() {
        return Ok((Vec::new(), skipped));
    }
    let added = instance
        .add_artifacts(&profile, &artifacts)
        .map_err(|error| error.to_string())?;
    Ok((added.iter().map(ToString::to_string).collect(), skipped))
}

/// Refuses invalid names and an instance that does not exist. The profile itself is read by the
/// network lane, which holds the instance-state lock to open the instance.
fn check_target(home: &Home, instance: &str, profile: &str) -> Result<(), String> {
    let instance = Name::new(instance).map_err(|error| error.to_string())?;
    Name::new(profile).map_err(|error| error.to_string())?;
    if home.instance(&instance).join("instance.toml").is_file() {
        Ok(())
    } else {
        Err(format!("there is no instance {instance}"))
    }
}

/// Refuses a source no provider routes, such as a local file.
fn check_source(home: &Home, source: &str) -> Result<(), String> {
    let providers = Providers::installed(home).map_err(|error| error.to_string())?;
    match providers.request(source) {
        Ok(_) => Ok(()),
        Err(RegistryError::Manifest(ManifestError::UnknownSource(_))) => Err(format!(
            "{source} is not a provider reference or https URL; add local files with msbe add"
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Moves an item whose files changed to the state they add up to. A paused, cancelled or
/// finished item keeps its state.
fn settle(item: &mut DownloadItem) {
    if !matches!(
        item.state,
        DownloadState::Downloading | DownloadState::AwaitingUser { .. }
    ) {
        return;
    }
    let busy = item.files.iter().any(|file| {
        matches!(
            file.state,
            DownloadFileState::Pending | DownloadFileState::Downloading
        )
    });
    let waiting = item.files.iter().find_map(|file| match &file.state {
        DownloadFileState::AwaitingUser { page, scheme } => Some((page.clone(), scheme.clone())),
        _ => None,
    });
    item.state = match (busy, waiting) {
        (true, _) => DownloadState::Downloading,
        (false, Some((page, scheme))) => DownloadState::AwaitingUser { page, scheme },
        (false, None) => DownloadState::Downloaded,
    };
}

/// Puts back one item a stopped daemon left part-way. Returns whether it changed.
fn recover(entry: &mut Entry) -> bool {
    let before = entry.clone();
    let mut lost = false;
    for (file, staged) in entry.item.files.iter_mut().zip(&entry.staged) {
        if file.state == DownloadFileState::Downloading {
            file.state = waiting_state(staged);
            lost |= file.state == DownloadFileState::Pending;
        }
    }
    let item = &mut entry.item;
    let unfinished = lost
        || item
            .files
            .iter()
            .any(|file| file.state == DownloadFileState::Pending);
    let next = match &item.state {
        DownloadState::Resolving => Some(DownloadState::Queued),
        DownloadState::Adding => Some(DownloadState::Downloaded),
        DownloadState::Downloading | DownloadState::AwaitingUser { .. } if unfinished => {
            Some(if item.source.is_some() {
                DownloadState::Queued
            } else {
                DownloadState::Failed {
                    message: "MSBE stopped before the link was redeemed; open the link again"
                        .to_owned(),
                }
            })
        }
        _ => None,
    };
    match next {
        Some(state) => item.state = state,
        None => settle(item),
    }
    *entry != before
}

/// What a file waits for when its download is not running: the user, if it came from a page, or
/// else the network lane.
fn waiting_state(staged: &Staged) -> DownloadFileState {
    match &staged.page {
        Some(page) => DownloadFileState::AwaitingUser {
            page: page.clone(),
            scheme: staged.scheme.clone(),
        },
        None => DownloadFileState::Pending,
    }
}

fn is_ready(item: &DownloadItem) -> bool {
    item.state == DownloadState::Downloaded && item.target.is_some()
}

fn blank(state: DownloadState) -> DownloadItem {
    DownloadItem {
        id: 0,
        revision: 0,
        title: None,
        source: None,
        target: None,
        with_deps: false,
        attempts: 0,
        state,
        files: Vec::new(),
        added: Vec::new(),
        skipped: Vec::new(),
        warnings: Vec::new(),
    }
}

fn refusal(item: &DownloadItem, action: &str) -> String {
    format!(
        "download {} cannot {action} while {}",
        item.id,
        item.state.name().replace('_', " ")
    )
}

fn host_failure(message: String) -> PackError {
    PackError::issue(IssueCode::HostFailure, message)
}

fn queue_directory(home: &Home) -> PathBuf {
    home.root().join("downloads")
}

fn queue_file(home: &Home) -> PathBuf {
    queue_directory(home).join("queue.json")
}

/// Where item `id`'s files wait until they are added.
fn quarantine(home: &Home, id: u64) -> PathBuf {
    queue_directory(home).join(id.to_string())
}

/// Removes quarantined downloads.
fn discard(path: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "quarantine holds only files the daemon downloaded; nothing is deployed from it"
    )]
    let removed = fs::remove_dir_all(path);
    // Already gone, or left for the next clear to remove.
    drop(removed);
}

/// An empty quarantine directory for one download.
fn reset(slot: &Path) -> Result<(), String> {
    discard(slot);
    fs::create_dir_all(slot).map_err(|error| format!("cannot create {}: {error}", slot.display()))
}

#[cfg(test)]
mod tests {
    use msbe_provider_api::model::HandoffTicket;
    use msbe_rpc_schema::{DownloadFile, DownloadFileState, DownloadState, DownloadTarget};

    use super::{Entry, Queue, Staged, blank, recover, settle};

    const PAGE: &str = "https://www.example.test/mods/dep";

    fn awaiting() -> DownloadFileState {
        DownloadFileState::AwaitingUser {
            page: PAGE.to_owned(),
            scheme: Some("handoff".to_owned()),
        }
    }

    fn file(project: &str, state: DownloadFileState) -> DownloadFile {
        DownloadFile {
            provider: "assisted".to_owned(),
            project: project.to_owned(),
            release: "r1".to_owned(),
            name: format!("{project}.txt"),
            size: None,
            state,
        }
    }

    /// An item for `tool` whose dependency `dep` waits for its link.
    fn waiting(queue: &mut Queue) -> u64 {
        let mut item = blank(DownloadState::Downloading);
        item.source = Some("assisted:tool".to_owned());
        item.target = Some(DownloadTarget {
            instance: "demo".to_owned(),
            profile: "default".to_owned(),
        });
        item.files = vec![
            file("tool", DownloadFileState::Downloaded),
            file("dep", awaiting()),
        ];
        settle(&mut item);
        let waiting = Staged {
            page: Some(PAGE.to_owned()),
            scheme: Some("handoff".to_owned()),
            ..Staged::default()
        };
        queue
            .insert(Entry {
                item,
                staged: vec![Staged::default(), waiting],
            })
            .id
    }

    fn ticket(project: &str) -> HandoffTicket {
        HandoffTicket {
            provider: "assisted".to_owned(),
            game: "example".to_owned(),
            catalog_game: "game".to_owned(),
            project: project.to_owned(),
            release: "r1".to_owned(),
            query: Vec::new(),
            expires: None,
        }
    }

    fn entry(queue: &Queue, id: u64) -> &Entry {
        queue
            .items
            .iter()
            .find(|entry| entry.item.id == id)
            .unwrap()
    }

    #[test]
    fn a_link_fills_the_file_an_item_waits_on_and_a_stray_link_waits_for_a_profile() {
        let mut queue = Queue::default();
        let id = waiting(&mut queue);
        assert_eq!(
            entry(&queue, id).item.state,
            DownloadState::AwaitingUser {
                page: PAGE.to_owned(),
                scheme: Some("handoff".to_owned())
            }
        );

        assert_eq!(queue.accept(&ticket("dep")), (id, 1, true));
        assert_eq!(entry(&queue, id).item.state, DownloadState::Downloading);

        let (stray, file, matched) = queue.accept(&ticket("dep"));
        assert!(!matched, "the item no longer waits on that file");
        assert_eq!(file, 0);
        let stray = &entry(&queue, stray).item;
        assert_eq!(
            (stray.target.as_ref(), stray.source.as_ref(), &stray.state),
            (None, None, &DownloadState::Downloading)
        );

        let mut queue = Queue::default();
        let id = waiting(&mut queue);
        queue.touch(id).unwrap().item.state = DownloadState::Cancelled;
        assert!(
            !queue.accept(&ticket("dep")).2,
            "a cancelled item waits on nothing"
        );
    }

    #[test]
    fn the_list_reports_items_changed_after_a_revision_and_every_id_in_order() {
        let mut queue = Queue::default();
        let first = waiting(&mut queue);
        let second = waiting(&mut queue);
        let seen = queue.list(0).next;
        assert_eq!(queue.list(0).order, [first, second]);
        assert!(queue.list(seen).items.is_empty());

        queue.touch(second).unwrap().item.state = DownloadState::Cancelled;
        let changed = queue.list(seen);
        assert_eq!(
            changed.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [second]
        );
        assert_eq!(
            queue.list(seen + 100).items.len(),
            2,
            "a revision the queue never reached reads everything"
        );
    }

    #[test]
    fn recovery_resolves_interrupted_work_again_and_waits_again_for_lost_links() {
        let mut queue = Queue::default();
        let resolving = waiting(&mut queue);
        queue.touch(resolving).unwrap().item.state = DownloadState::Resolving;
        let redeeming = waiting(&mut queue);
        queue.accept(&ticket("dep"));
        let adding = waiting(&mut queue);
        let adding_entry = queue.touch(adding).unwrap();
        adding_entry.item.files.get_mut(1).unwrap().state = DownloadFileState::Downloaded;
        adding_entry.item.state = DownloadState::Adding;
        let (stray, _, _) = queue.accept(&ticket("unrelated"));

        for entry in &mut queue.items {
            recover(entry);
        }
        assert_eq!(entry(&queue, resolving).item.state, DownloadState::Queued);
        let redeemed = &entry(&queue, redeeming).item;
        assert_eq!(redeemed.files.get(1).unwrap().state, awaiting());
        assert!(matches!(redeemed.state, DownloadState::AwaitingUser { .. }));
        assert_eq!(entry(&queue, adding).item.state, DownloadState::Downloaded);
        assert!(matches!(
            entry(&queue, stray).item.state,
            DownloadState::Failed { .. }
        ));
    }
}
