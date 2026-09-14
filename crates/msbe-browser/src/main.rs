//! The MSBE browser: an isolated host process for provider pages.
//!
//! The daemon starts it with a [`Launch`] as `--msbe-*` arguments and the capture channel as its
//! standard input and output (`docs/07-browser-and-secrets.md` §7.2). It shows one top-level window,
//! with a banner saying the page is a third-party site and the address it shows. It reports every
//! link in the provider's scheme, every download it saves to its quarantine, and the page it shows,
//! and does nothing else for a page. Nothing on a page can reach the channel: the pipes are moved off
//! the standard descriptors before CEF starts a subprocess, no script is injected, extensions are
//! off, and there is no remote-debugging port.
//!
//! CEF starts its renderer, GPU and utility subprocesses from this executable, and they return from
//! [`execute_process`] before any of this.
#![expect(
    clippy::transmute_ptr_to_ptr,
    reason = "cef-rs's wrap macros reach each handler's reference-counted base through a transmute"
)]
#![expect(
    clippy::print_stderr,
    reason = "the daemon discards this process's output; someone running it by hand sees why it did not start"
)]

mod handlers;
mod window;

use std::{
    collections::BTreeMap,
    fs::File,
    io,
    process::ExitCode,
    ptr,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    thread,
};

// cef-rs's wrap macros name the traits and types they implement unqualified.
use cef::args::Args;
use cef::*;
use msbe_browser_channel::{
    BrowserMessage, DaemonMessage, Launch, is_navigable, read_frame, write_frame,
};

/// A download being saved to the quarantine.
#[derive(Debug, Clone)]
struct Saving {
    /// Its random name in the quarantine.
    name: String,
    /// The name the page suggested.
    suggested: String,
    /// Where it comes from.
    url: String,
}

/// What the handlers share: the launch, the channel's writing end, and the window once it exists.
struct Shared {
    launch: Launch,
    writer: Mutex<File>,
    browser: Mutex<Option<Browser>>,
    window: Mutex<Option<Window>>,
    address: Mutex<Option<Textfield>>,
    /// The URL and title of the page shown.
    shown: Mutex<(String, String)>,
    saving: Mutex<BTreeMap<u32, Saving>>,
}

impl Shared {
    /// Sends `message` to the daemon. When the daemon is gone, the window closes.
    fn report(&self, message: &BrowserMessage) {
        if write_frame(&mut *lock(&self.writer), message).is_err() {
            post(UiAction::Close);
        }
    }
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Work the channel thread hands to CEF's UI thread.
#[derive(Debug, Clone)]
enum UiAction {
    Navigate(String),
    Close,
}

wrap_task! {
    struct UiTask {
        action: UiAction,
    }

    impl Task {
        fn execute(&self) {
            act(&self.action);
        }
    }
}

fn post(action: UiAction) {
    let mut task = UiTask::new(action);
    let _posted = post_task(ThreadId::UI, Some(&mut task));
}

fn act(action: &UiAction) {
    let Some(shared) = shared() else {
        return;
    };
    match action {
        UiAction::Navigate(url) => {
            let frame = lock(&shared.browser)
                .as_ref()
                .and_then(ImplBrowser::main_frame);
            if let Some(frame) = frame {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
        }
        UiAction::Close => {
            let window = lock(&shared.window).clone();
            match window {
                Some(window) => window.close(),
                None => quit_message_loop(),
            }
        }
    }
}

fn main() -> ExitCode {
    let args = Args::new();
    let mut app = handlers::BrowserApp::new();
    let code = execute_process(Some(args.as_main_args()), Some(&mut app), ptr::null_mut());
    if code >= 0 {
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    match run(&args, &mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("msbe-browser: {message}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &Args, app: &mut App) -> Result<(), String> {
    let launch = Launch::parse(std::env::args_os().skip(1)).map_err(|error| error.to_string())?;
    let (reader, writer) =
        take_channel().map_err(|error| format!("cannot take the capture channel: {error}"))?;
    let profile = launch
        .profile
        .to_str()
        .ok_or("the profile directory is not UTF-8")?
        .to_owned();
    let state = Arc::new(Shared {
        launch,
        writer: Mutex::new(writer),
        browser: Mutex::default(),
        window: Mutex::default(),
        address: Mutex::default(),
        shown: Mutex::default(),
        saving: Mutex::default(),
    });
    SHARED
        .set(Arc::clone(&state))
        .map_err(|_| "the browser started twice")?;
    let settings = Settings {
        root_cache_path: CefString::from(profile.as_str()),
        cache_path: CefString::from(profile.as_str()),
        persist_session_cookies: 1,
        // Switches on the command line could turn hardening off; only this process sets them.
        command_line_args_disabled: 1,
        remote_debugging_port: 0,
        ..Settings::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        ptr::null_mut(),
    ) != 1
    {
        return Err("CEF did not start".to_owned());
    }
    let listened = thread::Builder::new()
        .name("msbe-browser-channel".to_owned())
        .spawn(move || listen(reader, &state));
    if let Err(error) = listened {
        shutdown();
        return Err(format!("cannot follow the capture channel: {error}"));
    }
    run_message_loop();
    shutdown();
    Ok(())
}

/// Follows the daemon's messages until it closes the channel or sends something it may not.
fn listen(mut reader: File, shared: &Shared) {
    loop {
        match read_frame::<DaemonMessage>(&mut reader) {
            Ok(Some(DaemonMessage::Navigate { url })) => {
                // The daemon only sends pages the browser may show; any other is refused here too.
                if is_navigable(&url, &shared.launch.origins) {
                    post(UiAction::Navigate(url));
                }
            }
            Ok(Some(DaemonMessage::Close) | None) | Err(_) => {
                post(UiAction::Close);
                return;
            }
        }
    }
}

/// The capture channel's two ends, moved off standard input and output. CEF's subprocesses inherit
/// the standard descriptors, and must never inherit the channel.
#[cfg(unix)]
fn take_channel() -> io::Result<(File, File)> {
    use std::os::fd::AsFd as _;

    let reader = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let writer = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let null = File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
    Ok((reader, writer))
}

/// The capture channel's two ends. Chromium starts Windows subprocesses with an explicit list of
/// handles to inherit; that the standard handles are never on it is not verified.
#[cfg(windows)]
fn take_channel() -> io::Result<(File, File)> {
    use std::os::windows::io::AsHandle as _;

    let reader = File::from(io::stdin().as_handle().try_clone_to_owned()?);
    let writer = File::from(io::stdout().as_handle().try_clone_to_owned()?);
    Ok((reader, writer))
}
