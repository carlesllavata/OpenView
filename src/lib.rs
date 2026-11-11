// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenView: downloads 360 panoramas and photos from map applications at full resolution, with a
//! viewer, crops and HDRI. Part of the Open3D suite.

pub mod cli;
pub mod extras;
pub mod geom;
pub mod hdri;
pub mod links;
pub mod pano;
pub mod service;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
