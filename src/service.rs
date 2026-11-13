// SPDX-License-Identifier: GPL-3.0-or-later
//! Talks to the map service: what a panorama or a photo is (size, date, position, author, place,
//! other dates) and where its image is.
//!
//! These are internal services of a third party. They are not documented and may change without
//! notice; everything that depends on their answers is kept in this file.
//!
//! The description request (`photometa`) and the way flat photos and whole user images are fetched
//! follow the original OpenView by Daniel Castellanos (MIT, see NOTICE).

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

use crate::links::{self, Kind, Target};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";
const RPC: &str = "https://maps.googleapis.com/$rpc/google.internal.maps.mapsjs.v1.MapsJsInternalService";
/// Radius, in metres, used when a link only has coordinates.
pub const NEAR_RADIUS_M: f64 = 50.0;
/// The largest answer that is read into memory (a full-size photo fits comfortably).
const MAX_BODY: u64 = 256 * 1024 * 1024;

/// Why a request did not get through.
#[derive(Debug, Clone, PartialEq)]
pub enum Net {
    /// No connection, or the service cannot be reached.
    Offline,
    Timeout,
    /// The service answered with this HTTP status.
    Status(u16),
    Other(String),
}

#[derive(Debug)]
pub enum Error {
    /// The service has no such panorama (or nothing near the position).
    NotFound,
    /// The service answered something this version does not understand.
    Unexpected(String),
    Network(Net),
    Link(links::LinkError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::text::Lang::En.service(self))
    }
}

impl std::error::Error for Error {}

impl From<links::LinkError> for Error {
    fn from(e: links::LinkError) -> Self {
        Error::Link(e)
    }
}

fn net(e: ureq::Error) -> Error {
    Error::Network(match e {
        ureq::Error::Timeout(_) => Net::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => Net::Offline,
        ureq::Error::StatusCode(code) => Net::Status(code),
        other => Net::Other(other.to_string()),
    })
}

/// Another capture of the same place.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub id: String,
    pub kind: Kind,
    pub year: i32,
    pub month: u32,
}

impl Capture {
    pub fn date_text(&self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }
}

/// Everything known about a panorama (or a flat photo) before downloading it.
#[derive(Debug, Clone, PartialEq)]
pub struct PanoInfo {
    pub id: String,
    pub kind: Kind,
    /// An ordinary photo, not a 360 image.
    pub flat: bool,
    /// Image size at each zoom level, smallest first: (width, height). A flat photo has one entry;
    /// a photo pasted as a bare image address has none.
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
    /// Name of the place, most specific first, parts joined with " / ".
    pub address: Option<String>,
    /// Other dates of the same place, newest first. Does not include this one.
    pub history: Vec<Capture>,
    /// Address of the image that some photo links carry, without its size suffix.
    pub image_url: Option<String>,
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

    /// A 360 photo that does not cover the whole sphere is wider than 2:1.
    pub fn is_partial(&self) -> bool {
        let (w, h) = self.full_size();
        !self.flat && h > 0 && (w as f64 / h as f64 - 2.0).abs() > 0.02
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

    /// Addresses to try, best first, for the whole image at `width` x `height` (a user's 360 photo
    /// or a flat photo): the service hands these out in one piece.
    pub fn whole_image_urls(&self, width: u32, height: u32) -> Vec<String> {
        let mut urls = vec![
            format!("https://lh3.ggpht.com/jsapi2/a/b/c/w{width}-h{height}/{}", self.id),
            format!("https://lh3.googleusercontent.com/p/{}=w{width}-h{height}-k-no", self.id),
        ];
        if let Some(base) = &self.image_url {
            urls.push(format!("{base}=w{width}-h{height}-k-no"));
        }
        urls
    }
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    /// Language of place names: "en" or "es".
    lang: &'static str,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self::with_language("en")
    }

    pub fn with_language(lang: &'static str) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        Client { agent: config.into(), lang }
    }

    /// GET that returns the status, the `Location` header and the body.
    fn get_raw(&self, url: &str) -> Result<(u16, Option<String>, Vec<u8>), Error> {
        let mut response = self.agent.get(url).header("Referer", "https://www.google.com/").call().map_err(net)?;
        let status = response.status().as_u16();
        let location =
            response.headers().get("location").and_then(|v| v.to_str().ok()).map(|v| v.to_string());
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_BODY)
            .read_to_end(&mut body)
            .map_err(|_| Error::Network(Net::Offline))?;
        Ok((status, location, body))
    }

    /// Downloads a file, following redirects. An answer that says the file does not exist is
    /// `NotFound`; anything else that is not the file is a network error worth retrying.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, Error> {
        let mut url = url.to_string();
        for _ in 0..6 {
            let (status, location, body) = self.get_raw(&url)?;
            match status {
                200 if !body.is_empty() => return Ok(body),
                301 | 302 | 303 | 307 | 308 => match location {
                    Some(next) => url = absolute(&url, &next),
                    None => return Err(Error::Unexpected("redirect without address".into())),
                },
                400..=499 if status != 429 => return Err(Error::NotFound),
                other => return Err(Error::Network(Net::Status(other))),
            }
        }
        Err(Error::Network(Net::Other("too many redirects".into())))
    }

    /// Follows a short "share" link until it becomes a link the parser understands.
    pub fn follow_short(&self, link: &str) -> Result<String, Error> {
        let mut url = link.to_string();
        for _ in 0..10 {
            // A consent page keeps the real address in its `continue` parameter.
            if let Some(real) = consent_target(&url) {
                url = real;
            }
            if !matches!(links::parse(&url), Ok(Target::Short(_)) | Err(_)) {
                return Ok(url);
            }
            let (status, location, _) = self.get_raw(&url)?;
            if !(300..400).contains(&status) {
                return Err(Error::NotFound);
            }
            url = absolute(&url, &location.ok_or(Error::NotFound)?);
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
        response
            .body_mut()
            .as_reader()
            .take(MAX_BODY)
            .read_to_string(&mut text)
            .map_err(|_| Error::Network(Net::Offline))?;
        if status != 200 {
            return Err(Error::Network(Net::Status(status)));
        }
        serde_json::from_str(&text).map_err(|e| Error::Unexpected(e.to_string()))
    }

    /// What is known about a panorama or a photo.
    pub fn metadata(&self, id: &str, kind: Kind) -> Result<PanoInfo, Error> {
        let (status, _, body) = self.get_raw(&photometa_url(id, kind, self.lang))?;
        match status {
            200 => {}
            400 | 404 => return Err(Error::NotFound),
            other => return Err(Error::Network(Net::Status(other))),
        }
        // The answer starts with a line that is not JSON.
        let text = String::from_utf8_lossy(&body);
        let json_start = text.find('\n').map_or(0, |i| i + 1);
        let answer: Value =
            serde_json::from_str(&text[json_start..]).map_err(|e| Error::Unexpected(e.to_string()))?;
        if !matches!(at(&answer, &[1, 0, 0, 0]).as_u64(), Some(1 | 3)) {
            return Err(Error::NotFound);
        }
        parse_pano(at(&answer, &[1, 0])).ok_or(Error::NotFound)
    }

    fn search(&self, lat: f64, lon: f64, radius_m: f64, kind: Kind) -> Result<PanoInfo, Error> {
        let answer = self.rpc("SingleImageSearch", &search_request(lat, lon, radius_m, kind))?;
        parse_pano(at(&answer, &[1])).ok_or(Error::NotFound)
    }

    /// The street-level panorama nearest to a position; a user's 360 photo when there is none.
    pub fn nearest(&self, lat: f64, lon: f64, radius_m: f64) -> Result<PanoInfo, Error> {
        let mut found = match self.search(lat, lon, radius_m, Kind::Official) {
            Err(Error::NotFound) => self.search(lat, lon, radius_m, Kind::User)?,
            other => other?,
        };
        // The search answers with whatever capture it likes; the newest one taken by the service
        // itself is the useful default. Each capture lists other dates, so look a few times.
        let mut described = false;
        for _ in 0..4 {
            let current = found.date.map_or((0, 0), |(y, m, _)| (y, m));
            let newer = found.history.iter().find(|c| c.kind == Kind::Official && (c.year, c.month) > current);
            let Some(newer) = newer else { break };
            match self.metadata(&newer.id, newer.kind) {
                Ok(info) => {
                    found = info;
                    described = true;
                }
                Err(_) => break,
            }
        }
        // The search does not say where the place is; the description does.
        if !described {
            if let Ok(info) = self.metadata(&found.id, found.kind) {
                found = info;
            }
        }
        Ok(found)
    }

    /// Resolves a pasted link completely.
    pub fn find(&self, link: &str) -> Result<PanoInfo, Error> {
        let mut target = links::parse(link)?;
        let mut long_link = link.to_string();
        if let Target::Short(short) = &target {
            long_link = self.follow_short(short)?;
            target = links::parse(&long_link)?;
        }
        match target {
            Target::Pano { id, kind } => {
                let mut info = match self.metadata(&id, kind) {
                    Ok(info) => info,
                    // The link did not say which kind it was (or said it wrongly): try the other one.
                    Err(Error::NotFound) => {
                        let other = if kind == Kind::Official { Kind::User } else { Kind::Official };
                        self.metadata(&id, other)?
                    }
                    Err(e) => return Err(e),
                };
                info.image_url = links::image_address(&long_link);
                Ok(info)
            }
            Target::Near { lat, lon } => self.nearest(lat, lon, NEAR_RADIUS_M),
            Target::Image { id, base_url } => Ok(PanoInfo {
                id,
                kind: Kind::User,
                flat: true,
                sizes: Vec::new(),
                tile: (0, 0),
                lat: None,
                lon: None,
                heading: None,
                tilt: None,
                roll: None,
                date: None,
                author: None,
                copyright: None,
                address: None,
                history: Vec::new(),
                image_url: Some(base_url),
            }),
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

/// The address a consent page would continue to, when `url` is one.
fn consent_target(url: &str) -> Option<String> {
    let host = url.split_once("://")?.1.split('/').next()?;
    if !host.contains("consent.google") {
        return None;
    }
    let query = url.split_once('?')?.1;
    let value = query.split('&').find_map(|pair| pair.strip_prefix("continue="))?;
    Some(links::percent_decode(value))
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

/// The request for the description of an image, in the service's own notation.
fn photometa_url(id: &str, kind: Kind, lang: &str) -> String {
    format!(
        "https://www.google.com/maps/photometa/v1?authuser=0&hl={lang}&gl={lang}&pb=\
         !1m4!1smaps_sv.tactile!11m2!2m1!1b1!2m2!1s{lang}!2s{lang}!3m3!1m2!1e{code}!2s{id}\
         !4m54!1e1!1e2!1e3!1e4!1e5!1e6!1e8!1e12!2m1!1e1!4m1!1i48!5m0!6m0!9m36\
         !1m3!1e2!2b1!3e2!1m3!1e2!2b0!3e3!1m3!1e3!2b1!3e2!1m3!1e3!2b0!3e3!1m3!1e8!2b0!3e3\
         !1m3!1e1!2b0!3e3!1m3!1e4!2b0!3e3!1m3!1e10!2b1!3e2!1m3!1e10!2b0!3e3!11m2!3m1!4b1",
        code = kind.code()
    )
}

fn search_request(lat: f64, lon: f64, radius_m: f64, kind: Kind) -> Value {
    json!([
        ["apiv3", null, null, null, "US", null, null, null, null, null, [[0]]],
        [[null, null, lat, lon], radius_m],
        [null, ["en", "US"], null, null, null, null, null, null, [2], null, [[[kind.code(), true, 2]]]],
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

/// Reads the description of a panorama or a photo out of the service's answer.
pub fn parse_pano(pano: &Value) -> Option<PanoInfo> {
    let (id, kind) = image_key(at(pano, &[1]))?;
    // Form 3 is an ordinary photo; 2 is a 360 image, which comes with its zoom levels.
    let levels = at(pano, &[2, 3, 0]).as_array();
    let flat = at(pano, &[2, 1]).as_u64() == Some(3) || levels.is_none();
    let (sizes, tile) = if flat {
        let h = at(pano, &[2, 2, 0]).as_u64()? as u32;
        let w = at(pano, &[2, 2, 1]).as_u64()? as u32;
        (vec![(w, h)], (0, 0))
    } else {
        let mut sizes = Vec::new();
        for level in levels? {
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
        (sizes, tile)
    };
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

    let address = at(pano, &[3, 2]).as_array().map(|parts| {
        parts.iter().filter_map(|part| part.get(0).and_then(Value::as_str)).collect::<Vec<_>>().join(" / ")
    });

    Some(PanoInfo {
        id,
        kind,
        flat,
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
        address: address.filter(|a| !a.is_empty()),
        history,
        image_url: None,
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
            [null, null, [["Example Square", "en"], ["Sampleton, Exampleshire", "en"]]],
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
        assert!(!info.flat);
        assert_eq!(info.full_size(), (16384, 8192));
        assert_eq!(info.sizes.len(), 6);
        assert_eq!(info.tile, (512, 512));
        assert_eq!((info.lat, info.lon), (Some(40.5), Some(-3.25)));
        assert_eq!((info.heading, info.tilt, info.roll), (Some(271.5), Some(92.0), Some(358.5)));
        assert_eq!(info.date, Some((2024, 6, None)));
        assert_eq!(info.date_text(), "2024-06");
        assert_eq!(info.author.as_deref(), Some("Ann Example"));
        assert_eq!(info.copyright.as_deref(), Some("(c) Example Maps"));
        assert_eq!(info.address.as_deref(), Some("Example Square / Sampleton, Exampleshire"));
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
        assert_eq!(info.history[0].date_text(), "2022-11");
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
    fn a_flat_photo_is_recognised_with_its_size() {
        let photo = json!([
            [1],
            [10, "CIHM0ogKEICAgIFFFFFFFF"],
            [10, 3, [964, 900]],
            null,
            [null, [[["Bob Example"]]]],
            null,
            [null, null, null, null, null, null, null, [2024, 4, 24]]
        ]);
        let info = parse_pano(&photo).expect("parsed");
        assert!(info.flat);
        assert_eq!(info.sizes, vec![(900, 964)]);
        assert!(!info.is_partial());
        assert_eq!(info.date_text(), "2024-04-24");
        assert_eq!(info.author.as_deref(), Some("Bob Example"));
        assert_eq!((info.tilt, info.address.as_deref()), (None, None));
        assert_eq!(
            info.whole_image_urls(900, 964)[0],
            "https://lh3.ggpht.com/jsapi2/a/b/c/w900-h964/CIHM0ogKEICAgIFFFFFFFF"
        );
    }

    #[test]
    fn an_empty_answer_is_not_a_panorama() {
        assert_eq!(parse_pano(&json!([[5, "generic", "no"]])), None);
        assert_eq!(parse_pano(&Value::Null), None);
    }

    #[test]
    fn addresses_depend_on_the_kind() {
        assert!(tile_url("ID", Kind::Official, 5, 3, 2).ends_with("panoid=ID&x=3&y=2&zoom=5"));
        assert_eq!(tile_url("ID", Kind::User, 3, 1, 0), "https://lh3.ggpht.com/jsapi2/a/b/c/x1-y0-z3/ID");
        let official = photometa_url("ABC", Kind::Official, "es");
        assert!(official.contains("hl=es&gl=es") && official.contains("!3m3!1m2!1e2!2sABC!4m54"), "{official}");
        assert!(!official.contains(' '), "the request has a stray space");
        assert!(photometa_url("ABC", Kind::User, "en").contains("!1e10!2sABC"));
    }

    #[test]
    fn relative_redirects_become_absolute() {
        assert_eq!(absolute("https://a.example/x/y", "/z"), "https://a.example/z");
        assert_eq!(absolute("https://a.example/x", "//b.example/q"), "https://b.example/q");
        assert_eq!(absolute("https://a.example/x", "http://c.example/"), "http://c.example/");
    }

    #[test]
    fn consent_pages_give_the_real_address() {
        let url = "https://consent.google.com/m?continue=https://www.google.com/maps/%4040.4,-3.7,17z&gl=ES";
        assert_eq!(consent_target(url).as_deref(), Some("https://www.google.com/maps/@40.4,-3.7,17z"));
        assert_eq!(consent_target("https://www.google.com/maps/@1,2"), None);
    }
}
