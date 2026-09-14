//! The MSBE browser: a separate process the daemon starts, sends to the pages downloads wait on,
//! and hears back from over the capture channel (`docs/07-browser-and-secrets.md` §7.2).
//!
//! The browser has no binding into MSBE. It is started with its own profile and quarantine
//! directories for one provider, the origins of that provider's waiting pages, and its link scheme,
//! and it can only report captured links, finished downloads and the page it shows. The daemon
//! checks every report. A link goes to the download queue as `handoff.submit` would take it, and a
//! download fills the file whose page the browser was last sent to. A report the channel does not
//! carry, or one that does not match the browser's own launch, ends the session.

use std::{
    collections::BTreeSet,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

use msbe_browser_channel::{
    BrowserMessage, DaemonMessage, Launch, Origin, TITLE_LIMIT, URL_LIMIT, captured_scheme,
    display_name, is_navigable, is_quarantine_name, read_frame, write_frame,
};
use msbe_core::config::Home;
use msbe_rpc_schema::{BrowserOpen, BrowserStatus};
use msbe_secrets::SystemClock;

use crate::downloads::{Lanes, Waiting};

/// A started browser process: the two ends of its channel, and how to stop it.
pub struct Process {
    /// What the browser writes.
    pub reader: Box<dyn Read + Send>,
    /// What the browser reads.
    pub writer: Box<dyn Write + Send>,
    /// Waits briefly for the process to exit after its channel closes, then ends it.
    pub stop: Box<dyn FnOnce() + Send>,
}

impl fmt::Debug for Process {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Process").finish_non_exhaustive()
    }
}

/// Starts the browser process for a launch.
pub type Launcher = Arc<dyn Fn(&Launch) -> io::Result<Process> + Send + Sync>;

/// Where the browser component belongs: `msbe-browser` beside the daemon's executable.
fn component() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?
        .with_file_name(format!("msbe-browser{}", std::env::consts::EXE_SUFFIX)))
}

/// Starts `msbe-browser` from beside the daemon's executable, with its standard input and output as
/// the channel. No `MSBE_<PROVIDER>_TOKEN` variable reaches it.
pub fn installed_launcher() -> Launcher {
    Arc::new(|launch| {
        let program = component()?;
        if !program.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the MSBE browser component is not installed at {}",
                    program.display()
                ),
            ));
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the daemon starts MSBE's own browser component from beside itself; the process has no binding into MSBE and talks only over the checked capture channel"
        )]
        let mut command = Command::new(&program);
        command
            .args(launch.arguments())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for variable in msbe_core::config::provider_token_variables() {
            command.env_remove(variable);
        }
        let mut child = command.spawn()?;
        let missing = || io::Error::other("the browser process has no channel");
        let reader = child.stdout.take().ok_or_else(missing)?;
        let writer = child.stdin.take().ok_or_else(missing)?;
        Ok(Process {
            reader: Box::new(reader),
            writer: Box::new(writer),
            stop: Box::new(move || {
                for _ in 0..30 {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                drop(child.kill());
                drop(child.wait());
            }),
        })
    })
}

/// The daemon's browser: at most one session, for one provider at a time.
pub struct Browser {
    launcher: Launcher,
    state: Mutex<State>,
}

impl fmt::Debug for Browser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Browser").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    /// Counts sessions, so a stopped session's channel thread cannot act on a newer one.
    generation: u64,
    session: Option<Session>,
    auto_advance: bool,
    message: Option<String>,
    /// Checks, on every report, whether the browser component is installed. A launcher a test
    /// supplies has none, and is always there.
    component: Option<fn() -> bool>,
}

struct Session {
    generation: u64,
    provider: String,
    launch: Launch,
    writer: Box<dyn Write + Send>,
    stop: Box<dyn FnOnce() + Send>,
    /// The waiting file whose page the browser was last sent to.
    target: Option<Waiting>,
    /// The URL and title the browser last reported.
    shown: Option<(String, String)>,
}

impl State {
    fn session_mut(&mut self, generation: u64) -> Option<&mut Session> {
        self.session
            .as_mut()
            .filter(|session| session.generation == generation)
    }

    fn report(&self, waiting: &[Waiting]) -> BrowserStatus {
        let session = self.session.as_ref();
        let target = session.and_then(|session| session.target.as_ref());
        let shown = session.and_then(|session| session.shown.as_ref());
        BrowserStatus {
            running: session.is_some(),
            installed: self.component.is_none_or(|installed| installed()),
            provider: session.map(|session| session.provider.clone()),
            item: target.map(|target| target.item),
            page: target.map(|target| target.page.clone()),
            position: target
                .and_then(|target| waiting.iter().position(|file| same(file, target)))
                .and_then(|index| u64::try_from(index + 1).ok()),
            waiting: u64::try_from(waiting.len()).unwrap_or(u64::MAX),
            url: shown.map(|(url, _)| url.clone()),
            title: shown.map(|(_, title)| title.clone()),
            auto_advance: self.auto_advance,
            message: self.message.clone(),
        }
    }
}

impl Session {
    /// Sends the browser to `target`'s page, which must be HTTPS on an origin it was started for.
    fn navigate(&mut self, target: Waiting) -> Result<(), String> {
        if !is_navigable(&target.page, &self.launch.origins) {
            return Err(format!(
                "download {}'s page is not HTTPS on an origin the browser was started for",
                target.item
            ));
        }
        write_frame(
            &mut self.writer,
            &DaemonMessage::Navigate {
                url: target.page.clone(),
            },
        )
        .map_err(|error| error.to_string())?;
        self.target = Some(target);
        Ok(())
    }

    /// Asks the browser to close, closes its channel, and stops it.
    fn end(self) {
        let Self {
            mut writer, stop, ..
        } = self;
        drop(write_frame(&mut writer, &DaemonMessage::Close));
        drop(writer);
        stop();
    }
}

impl Browser {
    /// A browser that starts its process through `launcher`.
    pub fn new(launcher: Launcher) -> Self {
        Self {
            launcher,
            state: Mutex::new(State::default()),
        }
    }

    /// The browser component installed beside the daemon, started by [`installed_launcher`].
    pub fn installed() -> Self {
        let browser = Self::new(installed_launcher());
        browser.state().component = Some(|| component().is_ok_and(|program| program.is_file()));
        browser
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `browser.status`.
    pub(crate) fn status(&self, lanes: &Lanes) -> Result<BrowserStatus, String> {
        let waiting = lanes.waiting()?;
        Ok(self.state().report(&waiting))
    }

    /// `browser.open`: sends the browser to download `id`'s first waiting page, or to the next
    /// waiting page after the current one. A browser started for another provider, or without the
    /// page's origin, is closed and started again. With only `auto_advance`, while the browser shows
    /// a waiting page, the setting changes and the page stays, so a wait timer is never reset.
    pub(crate) fn open(
        self: &Arc<Self>,
        lanes: &Lanes,
        request: BrowserOpen,
    ) -> Result<BrowserStatus, String> {
        let home = lanes.home()?;
        let waiting = lanes.waiting()?;
        let mut state = self.state();
        if let Some(advance) = request.auto_advance {
            state.auto_advance = advance;
            let showing = state
                .session
                .as_ref()
                .is_some_and(|session| session.target.is_some());
            if request.id.is_none() && showing {
                return Ok(state.report(&waiting));
            }
        }
        let current = state
            .session
            .as_ref()
            .and_then(|session| session.target.clone());
        let target = match request.id {
            Some(id) => waiting
                .iter()
                .find(|file| file.item == id)
                .cloned()
                .ok_or_else(|| format!("download {id} does not wait on a page"))?,
            None => next(&waiting, current.as_ref(), None)
                .ok_or_else(|| "no download waits on a page".to_owned())?,
        };
        let reusable = state.session.as_ref().is_some_and(|session| {
            session.provider == target.provider
                && is_navigable(&target.page, &session.launch.origins)
        });
        let mut ended = Vec::new();
        if !reusable {
            ended.extend(state.session.take());
            let generation = state.generation + 1;
            match self.start(lanes, &home, &waiting, &target, generation) {
                Ok(session) => {
                    state.generation = generation;
                    state.session = Some(session);
                }
                Err(message) => {
                    state.message = Some(message.clone());
                    drop(state);
                    ended.into_iter().for_each(Session::end);
                    return Err(message);
                }
            }
        }
        let navigated = state
            .session
            .as_mut()
            .map_or(Ok(()), |session| session.navigate(target));
        let result = match navigated {
            Ok(()) => {
                state.message = None;
                Ok(state.report(&waiting))
            }
            Err(message) => {
                ended.extend(state.session.take());
                state.message = Some(message.clone());
                Err(message)
            }
        };
        drop(state);
        ended.into_iter().for_each(Session::end);
        result
    }

    /// `browser.close`.
    pub(crate) fn close(&self, lanes: &Lanes) -> Result<BrowserStatus, String> {
        let waiting = lanes.waiting()?;
        let mut state = self.state();
        let session = state.session.take();
        state.message = None;
        let status = state.report(&waiting);
        drop(state);
        if let Some(session) = session {
            session.end();
        }
        Ok(status)
    }

    /// Starts a browser for `target`'s provider, allowed the origins of that provider's waiting
    /// pages and capturing its link schemes, and follows its channel on a thread of its own.
    fn start(
        self: &Arc<Self>,
        lanes: &Lanes,
        home: &Home,
        waiting: &[Waiting],
        target: &Waiting,
        generation: u64,
    ) -> Result<Session, String> {
        let provider = &target.provider;
        if provider.is_empty()
            || !provider
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(format!("{provider:?} is not a provider id"));
        }
        let files = || waiting.iter().filter(|file| file.provider == *provider);
        let origins: BTreeSet<Origin> = files().filter_map(|file| Origin::of(&file.page)).collect();
        let schemes: BTreeSet<String> = files().filter_map(|file| file.scheme.clone()).collect();
        let directory = home.root().join("browser").join(provider);
        let launch = Launch {
            profile: directory.join("profile"),
            quarantine: directory.join("quarantine"),
            origins: origins.into_iter().collect(),
            schemes: schemes.into_iter().collect(),
        };
        private_directory(&launch.profile)?;
        private_directory(&launch.quarantine)?;
        clear_quarantine(&launch.quarantine);
        let Process {
            reader,
            writer,
            stop,
        } = (self.launcher)(&launch)
            .map_err(|error| format!("cannot start the MSBE browser: {error}"))?;
        let browser = Arc::clone(self);
        let listening = lanes.clone();
        let spawned = thread::Builder::new()
            .name("msbe-browser".to_owned())
            .spawn(move || browser.listen(&listening, generation, reader));
        if let Err(error) = spawned {
            drop(writer);
            stop();
            return Err(format!("cannot follow the MSBE browser: {error}"));
        }
        Ok(Session {
            generation,
            provider: provider.clone(),
            launch,
            writer,
            stop,
            target: None,
            shown: None,
        })
    }

    /// Reads session `generation`'s reports until its channel closes or carries something it may
    /// not.
    fn listen(self: Arc<Self>, lanes: &Lanes, generation: u64, mut reader: Box<dyn Read + Send>) {
        let reason = loop {
            match read_frame::<BrowserMessage>(&mut reader) {
                Ok(Some(message)) => {
                    if let Err(refusal) = self.receive(lanes, generation, message) {
                        break Some(refusal);
                    }
                }
                Ok(None) => break None,
                Err(error) => break Some(error.to_string()),
            }
        };
        self.ended(generation, reason);
    }

    /// Acts on one report. An error is a report the session may not make, and ends it.
    fn receive(
        &self,
        lanes: &Lanes,
        generation: u64,
        message: BrowserMessage,
    ) -> Result<(), String> {
        match message {
            BrowserMessage::NavigationState { url, title } => {
                if let Some(session) = self.state().session_mut(generation) {
                    session.shown = Some((clip(&url, URL_LIMIT), clip(&title, TITLE_LIMIT)));
                }
                Ok(())
            }
            BrowserMessage::CapturedProtocolUrl { url } => {
                let captured = self.state().session_mut(generation).is_some_and(|session| {
                    captured_scheme(&url, &session.launch.schemes).is_some()
                });
                if url.len() > URL_LIMIT || !captured {
                    return Err("it reported a link in a scheme it does not capture".to_owned());
                }
                let outcome = lanes.submit_link(url, &SystemClock).map(|_| ());
                self.captured(lanes, generation, outcome);
                Ok(())
            }
            BrowserMessage::CapturedDownload {
                suggested_name,
                quarantine_file,
                origin_url,
                size,
            } => {
                let Some((quarantine, target)) = self
                    .state()
                    .session_mut(generation)
                    .map(|session| (session.launch.quarantine.clone(), session.target.clone()))
                else {
                    return Ok(());
                };
                if !is_quarantine_name(&quarantine_file) {
                    return Err("it reported a download outside its quarantine".to_owned());
                }
                let path = quarantine.join(&quarantine_file);
                let matches = fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() == size);
                if !matches {
                    remove_captured(&path);
                    return Err("it reported a download that does not match its file".to_owned());
                }
                let outcome = match target {
                    _ if Origin::of(&origin_url).is_none() => {
                        Err("a download that did not come over HTTPS was discarded".to_owned())
                    }
                    Some(target) => lanes.capture(&target, &path, display_name(&suggested_name)),
                    None => Err(
                        "a download arrived while no waiting page was open, and was discarded"
                            .to_owned(),
                    ),
                };
                if outcome.is_err() {
                    remove_captured(&path);
                }
                self.captured(lanes, generation, outcome);
                Ok(())
            }
        }
    }

    /// Records a capture's outcome. After a capture the page is done; with auto-advance on, the
    /// browser goes to the provider's next waiting page. Nothing is clicked for the user.
    fn captured(&self, lanes: &Lanes, generation: u64, outcome: Result<(), String>) {
        let waiting = outcome.as_ref().ok().and_then(|()| lanes.waiting().ok());
        let mut state = self.state();
        let auto_advance = state.auto_advance;
        let mut message = outcome.err();
        if message.is_none()
            && let Some(session) = state.session_mut(generation)
        {
            let done = session.target.take();
            let upcoming = waiting
                .as_deref()
                .and_then(|waiting| next(waiting, done.as_ref(), Some(&session.provider)));
            if auto_advance && let Some(upcoming) = upcoming {
                message = session.navigate(upcoming).err();
            }
        }
        state.message = message;
    }

    /// Forgets session `generation` once its channel closed, and stops its process.
    fn ended(&self, generation: u64, reason: Option<String>) {
        let mut state = self.state();
        let Some(session) = state
            .session
            .take_if(|session| session.generation == generation)
        else {
            return;
        };
        state.message = reason.map(|reason| format!("the MSBE browser was stopped: {reason}"));
        drop(state);
        session.end();
    }
}

fn same(file: &Waiting, other: &Waiting) -> bool {
    file.item == other.item && file.file == other.file
}

/// The waiting file after `current`, wrapping around, or the first when `current` no longer
/// waits. Only `provider`'s files when one is given.
fn next(waiting: &[Waiting], current: Option<&Waiting>, provider: Option<&str>) -> Option<Waiting> {
    let files: Vec<&Waiting> = waiting
        .iter()
        .filter(|file| provider.is_none_or(|provider| file.provider == provider))
        .collect();
    let after = current
        .and_then(|current| files.iter().position(|file| same(file, current)))
        .map_or(0, |index| index + 1);
    files
        .get(after)
        .or_else(|| files.first())
        .map(|file| (*file).clone())
}

/// `text`, cut to at most `limit` bytes on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

fn private_directory(directory: &Path) -> Result<(), String> {
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot secure {}: {error}", directory.display()))?;
    }
    Ok(())
}

/// Removes what an earlier session left in the quarantine.
fn clear_quarantine(quarantine: &Path) {
    let Ok(entries) = fs::read_dir(quarantine) else {
        return;
    };
    for entry in entries.flatten() {
        remove_captured(&entry.path());
    }
}

fn remove_captured(path: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the browser's quarantine holds only downloads it captured, and nothing is deployed from it"
    )]
    let removed = fs::remove_file(path);
    drop(removed);
}

#[cfg(test)]
mod tests {
    use super::{Waiting, clip, next};

    fn file(item: u64, provider: &str) -> Waiting {
        Waiting {
            item,
            file: 0,
            provider: provider.to_owned(),
            page: format!("https://www.example.test/{item}"),
            scheme: None,
        }
    }

    #[test]
    fn the_next_page_follows_the_current_one_and_wraps_around() {
        let waiting = [file(1, "a"), file(2, "b"), file(3, "a")];
        assert_eq!(next(&waiting, None, None).map(|file| file.item), Some(1));
        assert_eq!(
            next(&waiting, Some(&waiting[0]), None).map(|file| file.item),
            Some(2)
        );
        assert_eq!(
            next(&waiting, Some(&waiting[0]), Some("a")).map(|file| file.item),
            Some(3)
        );
        assert_eq!(
            next(&waiting, Some(&waiting[2]), Some("a")).map(|file| file.item),
            Some(1)
        );
        assert_eq!(
            next(&waiting, Some(&file(9, "a")), Some("a")).map(|file| file.item),
            Some(1)
        );
        assert_eq!(next(&waiting, None, Some("c")), None);
    }

    #[test]
    fn reports_are_clipped_on_character_boundaries() {
        assert_eq!(clip("héllo", 2), "h");
        assert_eq!(clip("hello", 10), "hello");
    }
}
