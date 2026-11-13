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
use openview::service::Client;

/// A busy square with many captures over the years.
const PLACE: &str = "https://www.google.com/maps/@40.416729,-3.703339,17z";
/// An ordinary photo of a place in that square.
const PHOTO: &str = "https://www.google.com/maps/place/Puerta+del+Sol,+Centro,+Madrid/@40.4169769,-3.7027379,3a,75y,90t/data=!3m8!1e2!3m6!1sCIHM0ogKEICAgICz2MKBtgE!2e10!3e12!6shttps:%2F%2Flh3.googleusercontent.com%2Fgps-cs-s%2FANWiy9TsCA5EZFPNDdcxpTl9Pn6qJd77ITjcFb9Avd043Xt3jwAai4WDm5evWnMXUWCXd5t9YSEl1uXVpto8tIVk9m2qTxf_O3PmUaVeo6KJGufSGXTNeatNdov0sN1VQfaI1TgqTTKh-w%3Dw203-h217-k-no!7i900!8i964";

#[test]
#[ignore = "needs the network"]
fn a_position_finds_a_street_level_panorama_with_other_dates() {
    let info = Client::new().find(PLACE).expect("the service answers");
    assert_eq!(info.kind, Kind::Official);
    assert!(!info.flat);
    let (w, h) = info.full_size();
    assert!(w >= 13312 && w == h * 2, "unexpected size {w} x {h}");
    assert_eq!(info.tile, (512, 512));
    assert!(info.lat.is_some() && info.heading.is_some() && info.tilt.is_some() && info.roll.is_some());
    assert!(info.date.is_some_and(|(year, _, _)| year >= 2019), "date {:?}", info.date);
    assert!(info.history.len() >= 3, "only {} other dates", info.history.len());
    assert!(info.address.is_some(), "the place has no name");
}

#[test]
#[ignore = "needs the network"]
fn the_smallest_level_downloads_and_is_a_picture() {
    let client = Client::new();
    let info = client.find(PLACE).expect("the service answers");
    let image = pano::fetch_preview(&client, &info, 512, &AtomicBool::new(false)).expect("tiles arrive");
    assert_eq!(image.width(), image.height() * 2);
    let bright = image.pixels().filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 60).count();
    assert!(bright * 2 > (image.width() * image.height()) as usize, "the image is mostly black");
}

#[test]
#[ignore = "needs the network"]
fn another_date_can_be_looked_up() {
    let client = Client::new();
    let info = client.find(PLACE).expect("the service answers");
    let older = info.history.iter().find(|c| c.kind == Kind::Official).expect("an older capture");
    let other = client.metadata(&older.id, older.kind).expect("its description");
    assert_eq!(other.id, older.id);
    assert_eq!(other.date.map(|(y, m, _)| (y, m)), Some((older.year, older.month)));
    let mini = pano::mini(&client, &older.id, older.kind).expect("its small picture");
    assert!(mini.starts_with(&[0xFF, 0xD8]));
}

#[test]
#[ignore = "needs the network"]
fn a_photo_link_is_a_flat_photo_at_its_original_size() {
    let client = Client::new();
    let info = client.find(PHOTO).expect("the service answers");
    assert!(info.flat);
    assert_eq!(info.full_size(), (900, 964));
    let (image, _) = pano::fetch_whole(&client, &info, 900, 964, &AtomicBool::new(false)).expect("the photo arrives");
    assert_eq!(image.dimensions(), (900, 964));
}

#[test]
#[ignore = "needs the network"]
fn a_user_360_photo_comes_in_one_piece() {
    let client = Client::new();
    let info = client.metadata("CIHM0ogKEICAgICPqPujeQ", Kind::User).expect("its description");
    assert!(!info.flat && info.kind == Kind::User);
    let (w, h) = info.full_size();
    let (image, raw) = pano::fetch_whole(&client, &info, w, h, &AtomicBool::new(false)).expect("the photo arrives");
    assert_eq!(image.dimensions(), (w, h));
    assert!(raw.starts_with(&[0xFF, 0xD8, 0xFF]));
}

#[test]
#[ignore = "needs the network"]
fn nothing_is_found_in_the_middle_of_the_ocean() {
    let result = Client::new().find("https://www.google.com/maps/@30.0,-40.0,10z");
    assert!(matches!(result, Err(openview::service::Error::NotFound)), "{:?}", result.err());
}
