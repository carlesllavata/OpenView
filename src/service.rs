// SPDX-License-Identifier: GPL-3.0-or-later
//! Talks to the map service: what a panorama is (size, date, position, author, other dates) and
//! where its image tiles are.
//!
//! These are internal services of a third party. They are not documented and may change without
//! notice; everything that depends on their answers is kept in this file.

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

use crate::links::{self, Kind, Target};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const RPC: &str = "https://maps.googleapis.com/$rpc/google.internal.maps.mapsjs.v1.MapsJsInternalService";
/// Radius, in metres, used when a link only has coordinates.
pub const NEAR_RADIUS_M: f64 = 50.0;
/// The largest answer that is read into memory (a full-size photo fits comfortably).
const MAX_BODY: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    /// The service has no such panorama (or nothing near the position).
    NotFound,
    /// The service answered something this version does not understand.
    Unexpected(String),
    Network(String),
    Link(links::LinkError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "nothing was found for this link"),
            Error::Unexpected(what) => write!(f, "the service answered something unexpected ({what})"),
            Error::Network(what) => write!(f, "network error: {what}"),
            Error::Link(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<links::LinkError> for Error {
    fn from(e: links::LinkError) -> Self {
        Error::Link(e)
    }
}

/// Another capture of the same place.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub id: String,
    pub kind: Kind,
    pub year: i32,
    pub month: u32,
}

/// Everything known about a panorama before downloading it.
#[derive(Debug, Clone, PartialEq)]
pub struct PanoInfo {
    pub id: String,
    pub kind: Kind,
    /// Image size at each zoom level, smallest first: (width, height).
    pub sizes: Vec<(u32, u32)>,
    pub tile: (u32, u32),
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Compass direction of the centre of the image, in degrees.
    pub heading: Option<f64>,
    /// 90 when the camera was level.
    pub tilt: Option<f64>,
    /// 0 when the camera was level.
    pub roll: Option<f64>,
    /// Year, month and (user photos only) day of capture.
    pub date: Option<(i32, u32, Option<u32>)>,
    pub author: Option<String>,
    pub copyright: Option<String>,
    /// Other dates of the same place, newest first. Does not include this one.
    pub history: Vec<Capture>,
}

impl PanoInfo {
    /// Full size: (width, height).
    pub fn full_size(&self) -> (u32, u32) {
        self.sizes.last().copied().unwrap_or((0, 0))
    }

    /// The highest zoom level that is not wider than `max_width` (the smallest one if none fits).
    pub fn zoom_for(&self, max_width: Option<u32>) -> usize {
        let top = self.sizes.len().saturating_sub(1);
        match max_width {
            None => top,
            Some(limit) => self.sizes.iter().rposition(|(w, _)| *w <= limit).unwrap_or(0),
        }
    }

    /// A 360 photo that does not cover the whole sphere is not twice as wide as it is tall.
    pub fn is_partial(&self) -> bool {
        let (w, h) = self.full_size();
        h > 0 && w != h * 2
    }

    pub fn date_text(&self) -> String {
        match self.date {
            Some((y, m, Some(d))) => format!("{y:04}-{m:02}-{d:02}"),
            Some((y, m, None)) => format!("{y:04}-{m:02}"),
            None => String::new(),
        }
    }

    /// The address that opens this panorama in the map application.
    pub fn map_link(&self) -> String {
        let (lat, lon) = (self.lat.unwrap_or(0.0), self.lon.unwrap_or(0.0));
        let code = if self.kind == Kind::Official { 0 } else { 10 };
        format!(
            "https://www.google.com/maps/@{lat},{lon},3a,75y,{:.2}h,90t/data=!3m4!1e1!3m2!1s{}!2e{code}",
            self.heading.unwrap_or(0.0),
            self.id
        )
    }
}

/// A flat photo: where to get it.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoInfo {
    pub id: String,
    pub base_url: String,
    pub size: Option<(u32, u32)>,
}

impl PhotoInfo {
    /// The photo at its original size.
    pub fn original_url(&self) -> String {
        format!("{}=s0", self.base_url)
    }

    /// A small version for the card.
    pub fn preview_url(&self, width: u32) -> String {
        format!("{}=w{width}", self.base_url)
    }
}

/// What a link turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    Pano(PanoInfo),
    Photo(PhotoInfo),
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

fn net<E: std::fmt::Display>(e: E) -> Error {
    Error::Network(e.to_string())
}

impl Client {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(40)))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        Client { agent: config.into() }
    }

    /// GET that returns the status, the `Location` header and the body.
    fn get_raw(&self, url: &str) -> Result<(u16, Option<String>, Vec<u8>), Error> {
        let mut response = self.agent.get(url).call().map_err(net)?;
        let status = response.status().as_u16();
        let location =
            response.headers().get("location").and_then(|v| v.to_str().ok()).map(|v| v.to_string());
        let mut body = Vec::new();
        response.body_mut().as_reader().take(MAX_BODY).read_to_end(&mut body).map_err(net)?;
        Ok((status, location, body))
    }

    /// Downloads a file, following redirects.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, Error> {
        let mut url = url.to_string();
        for _ in 0..6 {
            let (status, location, body) = self.get_raw(&url)?;
            match status {
                200 => return Ok(body),
                301 | 302 | 303 | 307 | 308 => match location {
                    Some(next) => url = absolute(&url, &next),
                    None => return Err(Error::Unexpected("redirect without address".into())),
                },
                400 | 403 | 404 | 410 => return Err(Error::NotFound),
                other => return Err(Error::Network(format!("status {other}"))),
            }
        }
        Err(Error::Network("too many redirects".into()))
    }

    /// Follows a short "share" link until it becomes a link the parser understands.
    pub fn follow_short(&self, link: &str) -> Result<String, Error> {
        let mut url = link.to_string();
        for _ in 0..6 {
            let (status, location, _) = self.get_raw(&url)?;
            if !(300..400).contains(&status) {
                return Err(Error::NotFound);
            }
            let next = absolute(&url, &location.ok_or(Error::NotFound)?);
            // A consent page keeps the real address in its `continue` parameter.
            let next = match next.split_once("continue=") {
                Some((before, rest)) if before.contains("consent.") => {
                    links::percent_decode(rest.split('&').next().unwrap_or(rest))
                }
                _ => next,
            };
            if !matches!(links::parse(&next), Ok(Target::Short(_)) | Err(_)) {
                return Ok(next);
            }
            url = next;
        }
        Err(Error::NotFound)
    }

    fn rpc(&self, method: &str, body: &Value) -> Result<Value, Error> {
        let url = format!("{RPC}/{method}");
        let mut response = self
            .agent
            .post(&url)
            .header("content-type", "application/json+protobuf")
            .header("x-user-agent", "grpc-web-javascript/0.1")
            .send(body.to_string())
            .map_err(net)?;
        let status = response.status().as_u16();
        let mut text = String::new();
        response.body_mut().as_reader().take(MAX_BODY).read_to_string(&mut text).map_err(net)?;
        if status != 200 {
            return Err(Error::Network(format!("status {status}")));
        }
        serde_json::from_str(&text).map_err(|e| Error::Unexpected(e.to_string()))
    }

    /// What is known about a panorama.
    pub fn metadata(&self, id: &str, kind: Kind) -> Result<PanoInfo, Error> {
        let answer = self.rpc("GetMetadata", &metadata_request(id, kind))?;
        parse_pano(at(&answer, &[1, 0])).ok_or(Error::NotFound)
    }

    /// The street-level panorama nearest to a position.
    pub fn nearest(&self, lat: f64, lon: f64, radius_m: f64) -> Result<PanoInfo, Error> {
        let answer = self.rpc("SingleImageSearch", &search_request(lat, lon, radius_m))?;
        let mut found = parse_pano(at(&answer, &[1])).ok_or(Error::NotFound)?;
        // The search answers with whatever capture it likes; the newest one taken by the service
        // itself is the useful default. Each capture lists other dates, so look a few times.
        for _ in 0..4 {
            let current = found.date.map_or((0, 0), |(y, m, _)| (y, m));
            let newer = found.history.iter().find(|c| c.kind == Kind::Official && (c.year, c.month) > current);
            let Some(newer) = newer else { break };
            match self.metadata(&newer.id, newer.kind) {
                Ok(info) => found = info,
                Err(_) => break,
            }
        }
        Ok(found)
    }

    /// Resolves a pasted link completely.
    pub fn find(&self, link: &str) -> Result<Found, Error> {
        let mut target = links::parse(link)?;
        if let Target::Short(short) = &target {
            let long = self.follow_short(short)?;
            target = links::parse(&long)?;
        }
        match target {
            Target::Pano { id, kind } => match self.metadata(&id, kind) {
                Ok(info) => Ok(Found::Pano(info)),
                // The link did not say which kind it was (or said it wrongly): try the other one.
                Err(Error::NotFound) => {
                    let other = if kind == Kind::Official { Kind::User } else { Kind::Official };
                    self.metadata(&id, other).map(Found::Pano)
                }
                Err(e) => Err(e),
            },
            Target::Near { lat, lon } => self.nearest(lat, lon, NEAR_RADIUS_M).map(Found::Pano),
            Target::Photo { id, base_url, size } => {
                let base_url = match base_url {
                    Some(url) => url,
                    None if id.starts_with("AF1Qip") => format!("https://lh3.googleusercontent.com/p/{id}"),
                    None => return Err(Error::Unexpected("the link does not carry the photo's address".into())),
                };
                Ok(Found::Photo(PhotoInfo { id, base_url, size }))
            }
            Target::Short(_) => Err(Error::NotFound),
        }
    }

    /// One image tile, as the service sends it (JPEG).
    pub fn tile(&self, info: &PanoInfo, zoom: usize, x: u32, y: u32) -> Result<Vec<u8>, Error> {
        self.get_bytes(&tile_url(&info.id, info.kind, zoom, x, y))
    }
}

/// Where a tile lives.
pub fn tile_url(id: &str, kind: Kind, zoom: usize, x: u32, y: u32) -> String {
    match kind {
        Kind::Official => format!(
            "https://streetviewpixels-pa.googleapis.com/v1/tile?cb_client=maps_sv.tactile&panoid={id}&x={x}&y={y}&zoom={zoom}"
        ),
        Kind::User => format!("https://lh3.ggpht.com/jsapi2/a/b/c/x{x}-y{y}-z{zoom}/{id}"),
    }
}

/// A small picture of another capture of the same place, for the strip of dates.
pub fn capture_thumb_url(id: &str, kind: Kind) -> String {
    match kind {
        Kind::Official => format!(
            "https://streetviewpixels-pa.googleapis.com/v1/thumbnail?cb_client=maps_sv.tactile&w=224&h=112&pitch=0&yaw=0&panoid={id}"
        ),
        Kind::User => tile_url(id, kind, 0, 0, 0),
    }
}

fn absolute(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let Some((scheme, rest)) = base.split_once("://") else { return location.to_string() };
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(stripped) = location.strip_prefix("//") {
        format!("{scheme}://{stripped}")
    } else {
        format!("{scheme}://{host}/{}", location.trim_start_matches('/'))
    }
}

fn request_header() -> Value {
    json!(["apiv3", null, null, null, "US", null, null, null, null, null, [[0]]])
}

fn metadata_request(id: &str, kind: Kind) -> Value {
    json!([request_header(), ["en", "US"], [[[kind.code(), id]]], [[1, 2, 3, 4, 8, 6]]])
}

fn search_request(lat: f64, lon: f64, radius_m: f64) -> Value {
    json!([
        request_header(),
        [[null, null, lat, lon], radius_m],
        [null, ["en", "US"], null, null, null, null, null, null, [2], null, [[[2, true, 2]]]],
        [[1, 2, 3, 4, 8, 6]]
    ])
}

/// Walks nested arrays; anything missing gives `Null`.
fn at<'a>(value: &'a Value, path: &[usize]) -> &'a Value {
    let mut current = value;
    for index in path {
        current = current.get(*index).unwrap_or(&Value::Null);
    }
    current
}

fn image_key(value: &Value) -> Option<(String, Kind)> {
    let code = value.get(0)?.as_u64()?;
    let id = value.get(1)?.as_str()?;
    Some((id.to_string(), Kind::from_code(code)))
}

/// Reads the description of a panorama out of the service's answer.
pub fn parse_pano(pano: &Value) -> Option<PanoInfo> {
    let (id, kind) = image_key(at(pano, &[1]))?;
    let levels = at(pano, &[2, 3, 0]).as_array()?;
    let mut sizes = Vec::new();
    for level in levels {
        let h = at(level, &[0, 0]).as_u64()? as u32;
        let w = at(level, &[0, 1]).as_u64()? as u32;
        sizes.push((w, h));
    }
    if sizes.is_empty() {
        return None;
    }
    let tile = (
        at(pano, &[2, 3, 1, 0]).as_u64().unwrap_or(512) as u32,
        at(pano, &[2, 3, 1, 1]).as_u64().unwrap_or(512) as u32,
    );
    let place = at(pano, &[5, 0]);
    let pose = at(place, &[1]);
    let date = at(pano, &[6, 7]);
    let date = date.get(0).and_then(Value::as_i64).zip(date.get(1).and_then(Value::as_u64)).map(|(y, m)| {
        (y as i32, m as u32, date.get(2).and_then(Value::as_u64).map(|d| d as u32))
    });

    let mut history = Vec::new();
    let others = at(place, &[3, 0]);
    if let Some(entries) = at(place, &[8]).as_array() {
        for entry in entries {
            let Some(index) = entry.get(0).and_then(Value::as_u64) else { continue };
            let Some((other_id, other_kind)) = image_key(at(others, &[index as usize, 0])) else { continue };
            let (Some(year), Some(month)) = (at(entry, &[1, 0]).as_i64(), at(entry, &[1, 1]).as_u64()) else {
                continue;
            };
            if other_id != id {
                history.push(Capture { id: other_id, kind: other_kind, year: year as i32, month: month as u32 });
            }
        }
    }
    history.sort_by(|a, b| (b.year, b.month).cmp(&(a.year, a.month)));

    Some(PanoInfo {
        id,
        kind,
        sizes,
        tile,
        lat: at(pose, &[0, 2]).as_f64(),
        lon: at(pose, &[0, 3]).as_f64(),
        heading: at(pose, &[2, 0]).as_f64(),
        tilt: at(pose, &[2, 1]).as_f64(),
        roll: at(pose, &[2, 2]).as_f64(),
        date,
        author: at(pano, &[4, 1, 0, 0, 0]).as_str().map(str::to_string),
        copyright: at(pano, &[4, 0, 0, 0, 0]).as_str().map(str::to_string),
        history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer with the same shape as the service's, with made-up values.
    fn sample() -> Value {
        json!([
            [1],
            [2, "AAAAAAAAAAAAAAAAAAAAAA"],
            [2, 2, [8192, 16384],
                [[[[256, 512]], [[512, 1024]], [[1024, 2048]], [[2048, 4096]], [[4096, 8192]], [[8192, 16384]]], [512, 512]],
                null, null, null, null, null, "AAAAAAAAAAAAAAAAAAAAAA"],
            [],
            [[[["(c) Example Maps"]]], [[["Ann Example"], "//example.invalid/contrib/1"]]],
            [[
                [1],
                [[null, null, 40.5, -3.25], [650.0, null, 700.0], [271.5, 92.0, 358.5]],
                null,
                [[
                    [[2, "AAAAAAAAAAAAAAAAAAAAAA"], null, []],
                    [[2, "BBBBBBBBBBBBBBBBBBBBBB"], null, []],
                    [[2, "CCCCCCCCCCCCCCCCCCCCCC"], null, []],
                    [[10, "CIHM0ogKEICAgIDDDDDDDD"], null, []]
                ]],
                null, null, null, null,
                [[1, [2016, 5]], [2, [2022, 11]], [3, [2019, 8]], [9, [2001, 1]]]
            ]],
            [3, 4, 1, [1], null, [null, null, "scout", [6]], null, [2024, 6]]
        ])
    }

    #[test]
    fn panorama_description_is_read() {
        let info = parse_pano(&sample()).expect("parsed");
        assert_eq!(info.id, "AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(info.kind, Kind::Official);
        assert_eq!(info.full_size(), (16384, 8192));
        assert_eq!(info.sizes.len(), 6);
        assert_eq!(info.tile, (512, 512));
        assert_eq!((info.lat, info.lon), (Some(40.5), Some(-3.25)));
        assert_eq!((info.heading, info.tilt, info.roll), (Some(271.5), Some(92.0), Some(358.5)));
        assert_eq!(info.date, Some((2024, 6, None)));
        assert_eq!(info.date_text(), "2024-06");
        assert_eq!(info.author.as_deref(), Some("Ann Example"));
        assert_eq!(info.copyright.as_deref(), Some("(c) Example Maps"));
        assert!(!info.is_partial());
    }

    #[test]
    fn other_dates_come_newest_first_and_skip_broken_entries() {
        let info = parse_pano(&sample()).expect("parsed");
        let dates: Vec<_> = info.history.iter().map(|c| (c.id.as_str(), c.year, c.month, c.kind)).collect();
        assert_eq!(
            dates,
            vec![
                ("CCCCCCCCCCCCCCCCCCCCCC", 2022, 11, Kind::Official),
                ("CIHM0ogKEICAgIDDDDDDDD", 2019, 8, Kind::User),
                ("BBBBBBBBBBBBBBBBBBBBBB", 2016, 5, Kind::Official),
            ]
        );
    }

    #[test]
    fn zoom_level_respects_the_width_limit() {
        let info = parse_pano(&sample()).expect("parsed");
        assert_eq!(info.zoom_for(None), 5);
        assert_eq!(info.zoom_for(Some(8192)), 4);
        assert_eq!(info.zoom_for(Some(9000)), 4);
        assert_eq!(info.zoom_for(Some(100)), 0);
    }

    #[test]
    fn user_photo_keeps_its_day_and_partial_flag() {
        let mut value = sample();
        value[1] = json!([10, "CIHM0ogKEICAgIEEEEEEEE"]);
        value[2][3][0] = json!([[[300, 1000]], [[600, 2000]]]);
        value[6][7] = json!([2023, 2, 14]);
        let info = parse_pano(&value).expect("parsed");
        assert_eq!(info.kind, Kind::User);
        assert_eq!(info.date_text(), "2023-02-14");
        assert!(info.is_partial());
    }

    #[test]
    fn an_empty_answer_is_not_a_panorama() {
        assert_eq!(parse_pano(&json!([[5, "generic", "no"]])), None);
        assert_eq!(parse_pano(&Value::Null), None);
    }

    #[test]
    fn tile_addresses_depend_on_the_kind() {
        assert!(tile_url("ID", Kind::Official, 5, 3, 2).ends_with("panoid=ID&x=3&y=2&zoom=5"));
        assert_eq!(tile_url("ID", Kind::User, 3, 1, 0), "https://lh3.ggpht.com/jsapi2/a/b/c/x1-y0-z3/ID");
    }

    #[test]
    fn relative_redirects_become_absolute() {
        assert_eq!(absolute("https://a.example/x/y", "/z"), "https://a.example/z");
        assert_eq!(absolute("https://a.example/x", "//b.example/q"), "https://b.example/q");
        assert_eq!(absolute("https://a.example/x", "http://c.example/"), "http://c.example/");
    }
}
