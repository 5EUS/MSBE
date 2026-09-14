//! The browser's one top-level window: a banner saying the page is a third-party site, the address
//! of the page shown, and the page. It has its own chrome, so it is never mistaken for MSBE.

use std::ffi::c_int;

// cef-rs's wrap macros name the traits and types they implement unqualified.
use cef::*;

use crate::{handlers::BrowserClient, lock, shared};

const BANNER: &str = "This is a provider's website, not MSBE. MSBE receives only the links and downloads you start here.";

/// Creates the window, showing a blank page until the daemon sends one.
pub(crate) fn open() {
    let mut client = BrowserClient::new();
    let blank = CefString::from("about:blank");
    let view = browser_view_create(
        Some(&mut client),
        Some(&blank),
        Some(&BrowserSettings::default()),
        None,
        None,
        None,
    );
    let Some(view) = view else {
        quit_message_loop();
        return;
    };
    let mut delegate = MainWindow::new(view);
    if window_create_top_level(Some(&mut delegate)).is_none() {
        quit_message_loop();
    }
}

/// Shows `url` in the address bar.
pub(crate) fn show_address(url: &str) {
    let Some(shared) = shared() else {
        return;
    };
    if let Some(address) = lock(&shared.address).as_ref() {
        address.set_text(Some(&CefString::from(url)));
    }
}

wrap_window_delegate! {
    struct MainWindow {
        page: BrowserView,
    }

    impl ViewDelegate {}

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let (Some(shared), Some(window)) = (shared(), window) else {
                return;
            };
            let layout = window.set_to_box_layout(Some(&BoxLayoutSettings::default()));
            if let Some(banner) = label_button_create(None, Some(&CefString::from(BANNER))) {
                window.add_child_view(Some(&mut View::from(&banner)));
            }
            if let Some(address) = textfield_create(None) {
                address.set_read_only(1);
                address.set_accessible_name(Some(&CefString::from("Address of the page shown")));
                window.add_child_view(Some(&mut View::from(&address)));
                *lock(&shared.address) = Some(address);
            }
            let mut page = View::from(&self.page);
            window.add_child_view(Some(&mut page));
            if let Some(layout) = layout {
                layout.set_flex_for_view(Some(&mut page), 1);
            }
            window.set_title(Some(&CefString::from("MSBE browser")));
            window.show();
            *lock(&shared.window) = Some(window.clone());
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            if let Some(shared) = shared() {
                *lock(&shared.window) = None;
                *lock(&shared.address) = None;
            }
        }

        fn can_close(&self, _window: Option<&mut Window>) -> c_int {
            self.page
                .browser()
                .and_then(|browser| browser.host())
                .map_or(1, |host| host.try_close_browser())
        }

        fn initial_bounds(&self, _window: Option<&mut Window>) -> Rect {
            Rect {
                x: 0,
                y: 0,
                width: 1180,
                height: 820,
            }
        }
    }
}
