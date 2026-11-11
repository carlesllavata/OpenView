// SPDX-License-Identifier: GPL-3.0-or-later
//! Development tool: measures how vertical the buildings of real panoramas are, as downloaded and
//! after levelling with each combination of signs. This is how the signs in `geom` were chosen.
//!
//! `cargo run --example verticals -- <panorama id> [<panorama id>...]`

use std::sync::atomic::AtomicBool;

use openview::geom;
use openview::links::Kind;
use openview::pano;
use openview::service::Client;

fn main() {
    let client = Client::new();
    let cancel = AtomicBool::new(false);
    println!("{:<24} {:>6} {:>6} | {:>6} | {:>6} {:>6} {:>6} {:>6} | {:>6}", "id", "pitch", "roll", "raw", "+p+r", "+p-r", "-p+r", "-p-r", "app");
    for id in std::env::args().skip(1) {
        let info = match client.metadata(&id, Kind::Official) {
            Ok(info) => info,
            Err(e) => {
                println!("{id}: {e}");
                continue;
            }
        };
        let (Some(tilt), Some(roll)) = (info.tilt, info.roll) else { continue };
        let (pitch, roll) = geom::camera_lean(tilt, roll);
        let image = match pano::fetch_preview(&client, &info, 4096, &cancel) {
            Ok(image) => image,
            Err(e) => {
                println!("{id}: {e}");
                continue;
            }
        };
        let mut row = format!("{id:<24} {pitch:>6.2} {roll:>6.2} | {:>6.3} |", geom::vertical_deviation(&image));
        for (ps, rs) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
            let fixed = geom::reproject(&image, &geom::lean_matrix(pitch, roll, ps, rs));
            row.push_str(&format!(" {:>6.3}", geom::vertical_deviation(&fixed)));
        }
        let app = match pano::level_of(&info) {
            Some(m) => geom::vertical_deviation(&geom::reproject(&image, &m)),
            None => geom::vertical_deviation(&image),
        };
        println!("{row} | {app:>6.3}");
    }
}
