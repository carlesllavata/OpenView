// SPDX-License-Identifier: GPL-3.0-or-later
// No console window behind the app; a terminal that starts it with arguments is attached below.
#![windows_subsystem = "windows"]

/// Connects the output to the terminal the app was started from, if there is one.
#[cfg(windows)]
fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process: u32) -> i32;
    }
    const PARENT: u32 = u32::MAX;
    // SAFETY: a plain Win32 call without pointers; failing (no parent console) changes nothing.
    unsafe {
        AttachConsole(PARENT);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    attach_console();
    if args.is_empty() {
        println!("{}", openview::cli::HELP);
        return;
    }
    std::process::exit(openview::cli::run(&args));
}
