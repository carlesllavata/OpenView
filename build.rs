// SPDX-License-Identifier: GPL-3.0-or-later
//! Gives the Windows executable its icon and file properties.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    if std::path::Path::new("assets/icon.ico").exists() {
        resource.set_icon("assets/icon.ico");
    }
    resource.set("ProductName", "OpenView");
    resource.set("FileDescription", "OpenView");
    resource.set("LegalCopyright", "GPL-3.0-or-later");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=the executable has no icon: {error}");
    }
}
