//! What the browser does for a page: which navigations it allows, which links it captures, where
//! downloads go, and what it reports to the daemon.

use std::{ffi::c_int, fs, path::Path};

// cef-rs's wrap macros name the traits and types they implement unqualified.
use cef::*;
use msbe_browser_channel::{BrowserMessage, captured_scheme, quarantine_name};

use crate::{Saving, lock, shared, window};

/// Chromium features a provider page has no use for, turned off in the browser process.
const SWITCHES: [&str; 5] = [
    "disable-extensions",
    "disable-component-update",
    "disable-sync",
    "no-default-browser-check",
    "no-first-run",
];

wrap_app! {
    pub(crate) struct BrowserApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>
        ) {
            let subprocess = process_type.is_some_and(|kind| !kind.to_string().is_empty());
            let (false, Some(command_line)) = (subprocess, command_line) else {
                return;
            };
            for switch in SWITCHES {
                command_line.append_switch(Some(&CefString::from(switch)));
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(ProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    struct ProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            window::open();
        }
    }
}

wrap_client! {
    pub(crate) struct BrowserClient;

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(PageDisplay::new())
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(Quarantine::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(OneWindow::new())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(Navigation::new())
        }
    }
}

/// Whether the browser may go to `url`: an HTTPS page, the blank page it starts on, or a link in a
/// scheme it captures, which never loads.
fn may_browse(url: &str) -> bool {
    let Some(shared) = shared() else {
        return false;
    };
    url == "about:blank"
        || url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        || captured_scheme(url, &shared.launch.schemes).is_some()
}

wrap_request_handler! {
    struct Navigation;

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: c_int,
            _is_redirect: c_int
        ) -> c_int {
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();
            c_int::from(!may_browse(&url))
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: c_int,
            _is_download: c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut c_int>
        ) -> Option<ResourceRequestHandler> {
            Some(Protocols::new())
        }
    }
}

wrap_resource_request_handler! {
    struct Protocols;

    impl ResourceRequestHandler {
        fn on_protocol_execution(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            allow_os_execution: Option<&mut c_int>
        ) {
            // The operating system never opens a link from this browser.
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
            let (Some(shared), Some(request)) = (shared(), request) else {
                return;
            };
            let url = CefString::from(&request.url()).to_string();
            if captured_scheme(&url, &shared.launch.schemes).is_some() {
                shared.report(&BrowserMessage::CapturedProtocolUrl { url });
            }
        }
    }
}

wrap_download_handler! {
    struct Quarantine;

    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>
        ) -> c_int {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>
        ) -> c_int {
            // Handled either way: a download that cannot go to the quarantine does not happen.
            let (Some(shared), Some(item), Some(callback)) = (shared(), download_item, callback)
            else {
                return 1;
            };
            let mut random = [0_u8; 16];
            if getrandom::fill(&mut random).is_err() {
                return 1;
            }
            let name = quarantine_name(random);
            let path = shared.launch.quarantine.join(&name);
            let Some(path) = path.to_str() else {
                return 1;
            };
            lock(&shared.saving).insert(
                item.id(),
                Saving {
                    name,
                    suggested: suggested_name.map(ToString::to_string).unwrap_or_default(),
                    url: CefString::from(&item.url()).to_string(),
                },
            );
            callback.cont(Some(&CefString::from(path)), 0);
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>
        ) {
            let (Some(shared), Some(item)) = (shared(), download_item) else {
                return;
            };
            if item.is_complete() == 1 {
                let saving = lock(&shared.saving).remove(&item.id());
                if let Some(saving) = saving {
                    shared.report(&BrowserMessage::CapturedDownload {
                        suggested_name: saving.suggested,
                        quarantine_file: saving.name,
                        origin_url: saving.url,
                        size: u64::try_from(item.received_bytes()).unwrap_or_default(),
                    });
                }
            } else if item.is_canceled() == 1 || item.is_interrupted() == 1 {
                let saving = lock(&shared.saving).remove(&item.id());
                if let Some(saving) = saving {
                    remove(&shared.launch.quarantine.join(saving.name));
                }
            }
        }
    }
}

wrap_display_handler! {
    struct PageDisplay;

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>
        ) {
            let main = frame.is_some_and(|frame| frame.is_main() == 1);
            let (Some(shared), true) = (shared(), main) else {
                return;
            };
            let url = url.map(ToString::to_string).unwrap_or_default();
            window::show_address(&url);
            let title = {
                let mut shown = lock(&shared.shown);
                shown.0.clone_from(&url);
                shown.1.clone()
            };
            shared.report(&BrowserMessage::NavigationState { url, title });
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let Some(shared) = shared() else {
                return;
            };
            let title = title.map(ToString::to_string).unwrap_or_default();
            let url = {
                let mut shown = lock(&shared.shown);
                shown.1.clone_from(&title);
                shown.0.clone()
            };
            shared.report(&BrowserMessage::NavigationState { url, title });
        }
    }
}

wrap_life_span_handler! {
    struct OneWindow;

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>
        ) -> c_int {
            // A popup opens in the one window instead, so the address shown is always the page's.
            let main = browser.and_then(|browser| browser.main_frame());
            if let (Some(main), Some(url)) = (main, target_url)
                && may_browse(&url.to_string())
            {
                main.load_url(Some(url));
            }
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let (Some(shared), Some(browser)) = (shared(), browser) {
                *lock(&shared.browser) = Some(browser.clone());
            }
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            if let Some(shared) = shared() {
                *lock(&shared.browser) = None;
            }
            quit_message_loop();
        }
    }
}

/// Removes a download that was cancelled or interrupted part-way.
fn remove(path: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the quarantine holds only this browser's downloads, and nothing is deployed from it"
    )]
    let removed = fs::remove_file(path);
    drop(removed);
}
