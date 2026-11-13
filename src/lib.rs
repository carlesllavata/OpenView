// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenView: downloads 360 panoramas and photos from map applications at full resolution, with a
//! viewer, crops and HDRI. Part of the Open3D suite.
//!
//! Based on the original OpenView by Daniel Castellanos (MIT, see NOTICE): its interface is used
//! as it is, and its download, crop and HDRI methods are ported to Rust.

pub mod cli;
pub mod extras;
pub mod geom;
pub mod hdri;
pub mod links;
pub mod pano;
pub mod service;
pub mod settings;
pub mod text;
pub mod web;
pub mod window;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
