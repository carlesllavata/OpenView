// SPDX-License-Identifier: GPL-3.0-or-later
//! Checks against the real map service. They need the network and depend on a third party that
//! may change without notice, so they only run when asked for:
//!
//! `cargo test --test live -- --ignored`
//!
//! When one of these fails and the unit tests pass, the service changed: look at `src/service.rs`.

use std::sync::atomic::AtomicBool;

use openview::links::Kind;
use openview::pano;
use openview::service::{Client, Found};

/// A busy square with many captures over the years.
const PLACE: &str = "https://www.google.com/maps/@40.416729,-3.703339,17z";

fn nearest() -> openview::service::PanoInfo {
    match Client::new().find(PLACE).expect("the service answers") {
        Found::Pano(info) => info,
        Found::Photo(_) => panic!("a position gave a photo"),
    }
}

#[test]
#[ignore = "needs the network"]
fn a_position_finds_a_street_level_panorama_with_other_dates() {
    let info = nearest();
    assert_eq!(info.kind, Kind::Official);
    let (w, h) = info.full_size();
    assert!(w >= 13312 && w == h * 2, "unexpected size {w} x {h}");
    assert_eq!(info.tile, (512, 512));
    assert!(info.lat.is_some() && info.heading.is_some() && info.tilt.is_some() && info.roll.is_some());
    assert!(info.date.is_some_and(|(year, _, _)| year >= 2019), "date {:?}", info.date);
    assert!(info.history.len() >= 3, "only {} other dates", info.history.len());
}

#[test]
#[ignore = "needs the network"]
fn the_smallest_level_downloads_and_is_a_picture() {
    let client = Client::new();
    let info = nearest();
    let image = pano::fetch_preview(&client, &info, 512, &AtomicBool::new(false)).expect("tiles arrive");
    assert_eq!(image.width(), image.height() * 2);
    let bright = image.pixels().filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 60).count();
    assert!(bright * 2 > (image.width() * image.height()) as usize, "the image is mostly black");
}

#[test]
#[ignore = "needs the network"]
fn another_date_can_be_looked_up() {
    let client = Client::new();
    let info = nearest();
    let older = info.history.iter().find(|c| c.kind == Kind::Official).expect("an older capture");
    let other = client.metadata(&older.id, older.kind).expect("its description");
    assert_eq!(other.id, older.id);
    assert_eq!(other.date.map(|(y, m, _)| (y, m)), Some((older.year, older.month)));
}

#[test]
#[ignore = "needs the network"]
fn nothing_is_found_in_the_middle_of_the_ocean() {
    let result = Client::new().find("https://www.google.com/maps/@30.0,-40.0,10z");
    assert!(matches!(result, Err(openview::service::Error::NotFound)), "{result:?}");
}
