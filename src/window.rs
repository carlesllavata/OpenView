// SPDX-License-Identifier: GPL-3.0-or-later
//! The app's own window: WebView2 (the web engine built into Windows) showing the interface the
//! local server serves. No account, no sync, no history shared with the browser. When WebView2 is
//! not available the interface opens in the default browser instead, and the program ends a while
//! after that page is closed.

use std::path::Path;
use std::time::Duration;

use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Icon, WindowBuilder};
use wry::{PageLoadEvent, WebContext, WebViewBuilder};

use crate::settings::{self, WindowPlace};
use crate::text::{Lang, Msg};
use crate::web::Server;

const DEFAULT_SIZE: (f64, f64) = (1180.0, 860.0);
const MIN_SIZE: (f64, f64) = (640.0, 520.0);

enum UserEvent {
    PageLoaded,
    Show,
}

/// The window icon: the 64-pixel picture inside the program's icon file.
fn icon() -> Option<Icon> {
    let ico = include_bytes!("../assets/icon.ico");
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let entry = (0..count).map(|i| &ico[6 + 16 * i..22 + 16 * i]).find(|e| e[0] == 64)?;
    let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
    let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
    let picture = image::load_from_memory(ico.get(offset..offset + size)?).ok()?.into_rgba8();
    let (w, h) = picture.dimensions();
    Icon::from_rgba(picture.into_raw(), w, h).ok()
}

/// Whether a point of the desktop (in screen pixels) is on a connected screen: a window must not
/// be restored to a monitor that is no longer there.
fn on_a_monitor(x: i32, y: i32) -> bool {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromPoint(point: Point, flags: u32) -> isize;
    }
    // SAFETY: a plain Win32 query taking a point by value; 0 asks for null when no monitor has it.
    unsafe { MonitorFromPoint(Point { x, y }, 0) != 0 }
}

/// Asks a yes/no question in a message box that belongs to the window.
fn confirm(hwnd: isize, title: &str, question: &str) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(question), wide(title));
    const MB_YESNO: u32 = 0x4;
    const MB_ICONWARNING: u32 = 0x30;
    const MB_DEFBUTTON2: u32 = 0x100;
    const IDYES: i32 = 6;
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
    unsafe { MessageBoxW(hwnd, text.as_ptr(), caption.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) == IDYES }
}

/// Opens the window and runs until it is closed. `open` are links to add straight away; a path to
/// an image on disk opens it in the HDRI converter instead.
pub fn run(open: Vec<String>) -> Result<(), String> {
    let server = Server::start()?;
    let (links, images): (Vec<String>, Vec<String>) = open.into_iter().partition(|item| !Path::new(item).is_file());
    server.add_links(&links);
    let mut url = server.url().to_string();
    if let Some(sid) = images.first().and_then(|image| server.preopen_hdri(Path::new(image))) {
        url.push_str(&format!("?hdri={sid}"));
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let saved = server.settings().window;
    let mut builder = WindowBuilder::new()
        .with_title("OpenView")
        .with_window_icon(icon())
        .with_min_inner_size(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1))
        .with_inner_size(LogicalSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1))
        .with_visible(false);
    if let Some(place) = saved {
        builder = builder.with_inner_size(PhysicalSize::new(place.w.max(MIN_SIZE.0 as u32), place.h.max(MIN_SIZE.1 as u32)));
        if let (Some(x), Some(y)) = (place.x, place.y) {
            if on_a_monitor(x + (place.w.min(400) / 2) as i32, y + 20) {
                builder = builder.with_position(PhysicalPosition::new(x, y));
            }
        }
        builder = builder.with_maximized(place.max);
    }
    let window = builder.build(&event_loop).map_err(|e| e.to_string())?;
    server.set_parent(window.hwnd());

    let proxy = event_loop.create_proxy();
    let mut context = WebContext::new(Some(settings::data_dir().join("webview")));
    let base = server.url().to_string();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(&url)
        .with_devtools(cfg!(debug_assertions))
        // Only the app's own page; any other address opens in the browser.
        .with_navigation_handler(move |target| {
            if target.starts_with(&base) {
                true
            } else {
                let _ = std::process::Command::new("explorer").arg(&target).spawn();
                false
            }
        })
        .with_new_window_req_handler(|target, _| {
            let _ = std::process::Command::new("explorer").arg(&target).spawn();
            wry::NewWindowResponse::Deny
        })
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = proxy.send_event(UserEvent::PageLoaded);
            }
        })
        .build(&window);
    let webview = match webview {
        Ok(webview) => webview,
        Err(_) => {
            // Plan B: the default browser.
            drop(window);
            return run_in_browser(&server, &url);
        }
    };
    // Show the window even if the page is slow, so the app never looks dead.
    let late = event_loop.create_proxy();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let _ = late.send_event(UserEvent::Show);
    });

    let mut place = saved.unwrap_or(WindowPlace { w: 0, h: 0, x: None, y: None, max: false });
    let mut shown = false;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep = &webview;
        match event {
            Event::UserEvent(UserEvent::PageLoaded | UserEvent::Show) if !shown => {
                shown = true;
                window.set_visible(true);
                window.set_focus();
            }
            Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                place.max = window.is_maximized();
                if !place.max && !window.is_minimized() && size.width > 0 {
                    (place.w, place.h) = (size.width, size.height);
                }
            }
            Event::WindowEvent { event: WindowEvent::Moved(position), .. } => {
                // Windows moves minimised windows to -32000.
                if !window.is_maximized() && !window.is_minimized() && position.x > -3000 && position.y > -3000 {
                    (place.x, place.y) = (Some(position.x), Some(position.y));
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                let running = server.running();
                if running > 0 {
                    let lang = server.lang();
                    if !confirm(window.hwnd(), lang.msg(Msg::CloseTitle), &lang.close_question(running)) {
                        return;
                    }
                    server.stop_all();
                }
                if place.w == 0 {
                    let size = window.inner_size();
                    (place.w, place.h) = (size.width, size.height);
                }
                server.save_window(place);
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

/// Opens the interface in the default browser and keeps the program alive while the page is open
/// (it says so every few seconds) or something is still running.
fn run_in_browser(server: &Server, url: &str) -> Result<(), String> {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    let lang: Lang = server.lang();
    eprintln!("{}", lang.msg(Msg::NoWebView));
    // Give the browser time to start before counting silence.
    std::thread::sleep(Duration::from_secs(30));
    loop {
        std::thread::sleep(Duration::from_secs(5));
        if server.running() == 0 && server.since_ping() > Duration::from_secs(20) {
            return Ok(());
        }
    }
}
