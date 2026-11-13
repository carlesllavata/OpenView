// SPDX-License-Identifier: GPL-3.0-or-later
//! The local web server behind the window: it serves the interface (`ui/index.html`) and the API
//! the interface talks to. It only listens on 127.0.0.1, and every order has to carry the
//! session's secret token, so other web pages cannot use it.
//!
//! The API, its routes and its answers are those of the original OpenView by Daniel Castellanos
//! (MIT, see NOTICE), so its interface works unchanged.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use image::RgbImage;
use serde_json::{json, Map, Value};

use crate::geom;
use crate::hdri::{self, HdrFormat};
use crate::links::{self, Kind};
use crate::pano::{self, Control, Crop, Options, Step};
use crate::service::{Client, PanoInfo};
use crate::settings::{Settings, WindowPlace};
use crate::text::{Lang, Msg};

const PAGE: &str = include_str!("../ui/index.html");
const STRINGS_EN: &str = include_str!("../ui/i18n/en.json");
const STRINGS_ES: &str = include_str!("../ui/i18n/es.json");
/// The largest request body that is read.
const MAX_BODY: u64 = 1 << 20;

/// The interface's texts in one language.
pub fn strings(lang: Lang) -> &'static Map<String, Value> {
    static EN: OnceLock<Map<String, Value>> = OnceLock::new();
    static ES: OnceLock<Map<String, Value>> = OnceLock::new();
    let (cell, text) = match lang {
        Lang::En => (&EN, STRINGS_EN),
        Lang::Es => (&ES, STRINGS_ES),
    };
    cell.get_or_init(|| serde_json::from_str(text).expect("the interface's texts are valid JSON"))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The page in a language and a theme, carrying the session's token.
pub fn page(lang: Lang, theme: &str, token: &str) -> String {
    let texts = strings(lang);
    let mut out = String::with_capacity(PAGE.len() + 4096);
    let mut rest = PAGE;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").expect("every {{ in the page is closed");
        let key = &after[..end];
        match key {
            "lang" => out.push_str(lang.code()),
            "theme" => out.push_str(theme),
            _ => {
                let value = texts.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("missing text {key}"));
                // Keys ending in _html carry markup on purpose; everything else is plain text.
                if key.ends_with("_html") {
                    out.push_str(value);
                } else {
                    out.push_str(&escape_html(value));
                }
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    let texts_json = serde_json::to_string(texts).unwrap_or_default().replace("</", "<\\/");
    out.replace("__APP_TOKEN__", token).replace("__I18N__", &texts_json)
}

fn random_token() -> String {
    let mut token = String::new();
    for _ in 0..3 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(Instant::now().elapsed().as_nanos() ^ std::process::id() as u128);
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

/// Compares two strings in a time that does not depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Resolving,
    Ready,
    Queued,
    Downloading,
    Done,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Resolving => "resolving",
            Status::Ready => "ready",
            Status::Queued => "queued",
            Status::Downloading => "downloading",
            Status::Done => "done",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DateEntry {
    id: String,
    date: String,
    kind: Kind,
}

/// One pasted link: a card of the list.
struct Item {
    id: String,
    link: String,
    status: Status,
    progress: f32,
    error: Option<String>,
    path: Option<PathBuf>,
    notice: Option<String>,
    info: Option<PanoInfo>,
    dates: Vec<DateEntry>,
    thumb_ver: u32,
    cancel: Arc<AtomicBool>,
}

impl Item {
    fn public(&self, has_thumb: bool) -> Value {
        let meta = self.info.as_ref().map(|info| {
            let (w, h) = info.full_size();
            let level = pano::level_of(info).map(|m| geom::viewer_matrix(&m));
            json!({
                "id": info.id,
                "date": if info.date.is_some() { Some(info.date_text()) } else { None },
                "lat": info.lat,
                "lon": info.lon,
                "uploader": info.author,
                "address": info.address,
                "heading": info.heading,
                "third_party": info.kind == Kind::User,
                "partial": info.is_partial(),
                "kind": if info.flat { "photo" } else { "pano" },
                "level": level,
                "width": if w > 0 { Some(w) } else { None },
                "height": if h > 0 { Some(h) } else { None },
            })
        });
        json!({
            "id": self.id,
            "link": self.link,
            "status": self.status.as_str(),
            "progress": (self.progress as f64 * 1000.0).round() / 1000.0,
            "error": self.error,
            "path": self.path,
            "notice": self.notice,
            "hasThumb": has_thumb,
            "thumbVer": self.thumb_ver,
            "dates": self.dates.iter().map(|d| json!({"id": d.id, "date": d.date, "kind": d.kind.code()})).collect::<Vec<_>>(),
            "meta": meta,
        })
    }
}

/// The dates of a place (the current one included), newest first, without repeats.
fn merge_dates(old: &[DateEntry], info: &PanoInfo) -> Vec<DateEntry> {
    let mut all: Vec<DateEntry> = old.to_vec();
    let mut add = |entry: DateEntry| {
        if !all.iter().any(|d| d.id == entry.id) {
            all.push(entry);
        }
    };
    if info.date.is_some() {
        add(DateEntry { id: info.id.clone(), date: info.date_text(), kind: info.kind });
    }
    for capture in &info.history {
        add(DateEntry { id: capture.id.clone(), date: capture.date_text(), kind: capture.kind });
    }
    all.sort_by(|a, b| b.date.cmp(&a.date));
    all
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Idle,
    Working,
    Done,
    Error,
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            JobStatus::Idle => "idle",
            JobStatus::Working => "working",
            JobStatus::Done => "done",
            JobStatus::Error => "error",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

struct CropJob {
    cid: String,
    item: String,
    status: JobStatus,
    progress: f32,
    stage: String,
    out: Option<PathBuf>,
    error: Option<String>,
    size: Option<(u32, u32)>,
}

impl CropJob {
    fn public(&self) -> Value {
        json!({
            "cid": self.cid, "item": self.item, "status": self.status.as_str(), "progress": (self.progress as f64 * 1000.0).round() / 1000.0,
            "stage": self.stage, "out": self.out, "error": self.error, "size": self.size.map(|(w, h)| [w, h]),
        })
    }
}

struct HdriJob {
    sid: String,
    src: PathBuf,
    name: String,
    small: Arc<RgbImage>,
    size: (u32, u32),
    status: JobStatus,
    progress: f32,
    out: Option<PathBuf>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
}

impl HdriJob {
    fn public(&self) -> Value {
        json!({
            "sid": self.sid, "name": self.name, "size": [self.size.0, self.size.1], "status": self.status.as_str(),
            "progress": (self.progress as f64 * 1000.0).round() / 1000.0, "out": self.out, "error": self.error,
        })
    }

    fn opened(&self) -> Value {
        json!({"sid": self.sid, "name": self.name, "size": [self.size.0, self.size.1]})
    }
}

#[derive(Default)]
struct State {
    items: HashMap<String, Item>,
    order: Vec<String>,
    thumbs: HashMap<String, Vec<u8>>,
    /// The sharper picture the viewer uses: per card, with the panorama it shows.
    views: HashMap<String, (String, Arc<Vec<u8>>)>,
    crops: HashMap<String, CropJob>,
    hdri: HashMap<String, HdriJob>,
}

struct Shared {
    token: String,
    base_url: String,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    english: Client,
    spanish: Client,
    downloads: Mutex<Sender<(String, Options)>>,
    last_ping: Mutex<Instant>,
    /// The window, for dialogs.
    parent: Mutex<Option<isize>>,
    /// The last whole panorama, kept for several crops of it.
    full: Mutex<Option<(String, Arc<RgbImage>)>>,
    minis: Mutex<HashMap<String, Vec<u8>>>,
    next_id: Mutex<u64>,
}

/// The running server. Cloning it gives another handle to the same one.
#[derive(Clone)]
pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    /// Starts listening on a free port of 127.0.0.1.
    pub fn start() -> Result<Server, String> {
        let http = tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = http.server_addr().to_ip().map(|a| a.port()).ok_or("no port")?;
        let (tx, rx) = mpsc::channel::<(String, Options)>();
        let server = Server {
            shared: Arc::new(Shared {
                token: random_token(),
                base_url: format!("http://127.0.0.1:{port}/"),
                state: Mutex::new(State::default()),
                settings: Mutex::new(Settings::load()),
                english: Client::with_language("en"),
                spanish: Client::with_language("es"),
                downloads: Mutex::new(tx),
                last_ping: Mutex::new(Instant::now()),
                parent: Mutex::new(None),
                full: Mutex::new(None),
                minis: Mutex::new(HashMap::new()),
                next_id: Mutex::new(1),
            }),
        };
        let worker = server.clone();
        std::thread::spawn(move || {
            for (id, options) in rx {
                worker.download(&id, &options);
            }
        });
        let listener = server.clone();
        std::thread::spawn(move || {
            for request in http.incoming_requests() {
                let handler = listener.clone();
                std::thread::spawn(move || handler.handle(request));
            }
        });
        Ok(server)
    }

    pub fn url(&self) -> &str {
        &self.shared.base_url
    }

    pub fn settings(&self) -> Settings {
        self.shared.settings.lock().unwrap().clone()
    }

    pub fn lang(&self) -> Lang {
        Lang::from_setting(&self.shared.settings.lock().unwrap().lang)
    }

    fn client(&self) -> &Client {
        match self.lang() {
            Lang::En => &self.shared.english,
            Lang::Es => &self.shared.spanish,
        }
    }

    /// The window that dialogs belong to.
    pub fn set_parent(&self, hwnd: isize) {
        *self.shared.parent.lock().unwrap() = Some(hwnd);
    }

    pub fn save_window(&self, place: WindowPlace) {
        let mut settings = self.shared.settings.lock().unwrap();
        settings.window = Some(place);
        settings.save();
    }

    /// Seconds since the page last said it is open.
    pub fn since_ping(&self) -> Duration {
        self.shared.last_ping.lock().unwrap().elapsed()
    }

    fn new_id(&self) -> String {
        let mut next = self.shared.next_id.lock().unwrap();
        *next += 1;
        format!("{:x}{:06x}", *next, Instant::now().elapsed().subsec_nanos() & 0xff_ffff)
    }

    /// How many downloads, crops and conversions are running or waiting.
    pub fn running(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        state.items.values().filter(|i| matches!(i.status, Status::Queued | Status::Downloading)).count()
            + state.hdri.values().filter(|j| j.status == JobStatus::Working).count()
            + state.crops.values().filter(|j| j.status == JobStatus::Working).count()
    }

    /// Cancels everything and waits a moment for it to stop.
    pub fn stop_all(&self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            for item in state.items.values_mut() {
                if item.status == Status::Queued {
                    item.status = Status::Ready;
                }
                item.cancel.store(true, Ordering::Relaxed);
            }
            for job in state.hdri.values() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        let start = Instant::now();
        while self.running() > 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Adds links as cards, newest first.
    pub fn add_links(&self, links: &[String]) -> Vec<String> {
        let mut new = Vec::new();
        for link in links.iter().take(200) {
            let link = link.trim();
            if link.is_empty() {
                continue;
            }
            let id = self.new_id();
            {
                let mut state = self.shared.state.lock().unwrap();
                state.items.insert(
                    id.clone(),
                    Item {
                        id: id.clone(),
                        link: link.to_string(),
                        status: Status::Resolving,
                        progress: 0.0,
                        error: None,
                        path: None,
                        notice: None,
                        info: None,
                        dates: Vec::new(),
                        thumb_ver: 0,
                        cancel: Arc::new(AtomicBool::new(false)),
                    },
                );
                state.order.insert(0, id.clone());
            }
            let me = self.clone();
            let worker_id = id.clone();
            std::thread::spawn(move || me.resolve(&worker_id));
            new.push(id);
        }
        new
    }

    fn resolve(&self, id: &str) {
        let Some(link) = self.shared.state.lock().unwrap().items.get(id).map(|i| i.link.clone()) else { return };
        let lang = self.lang();
        match self.client().find(&link) {
            Ok(info) => {
                {
                    let mut state = self.shared.state.lock().unwrap();
                    // Already in the list? Checked and stored at once, so two equal links pasted
                    // together do not both get in.
                    let duplicate = state
                        .items
                        .values()
                        .any(|other| other.id != id && other.info.as_ref().is_some_and(|i| i.id == info.id));
                    let Some(item) = state.items.get_mut(id) else { return };
                    if duplicate {
                        item.status = Status::Error;
                        item.error = Some(lang.msg(Msg::AlreadyListed).into());
                        return;
                    }
                    item.dates = merge_dates(&[], &info);
                    item.info = Some(info.clone());
                    item.status = Status::Ready;
                }
                self.make_thumb(id, &info);
            }
            Err(e) => {
                if let Some(item) = self.shared.state.lock().unwrap().items.get_mut(id) {
                    item.status = Status::Error;
                    item.error = Some(lang.service(&e));
                }
            }
        }
    }

    fn make_thumb(&self, id: &str, info: &PanoInfo) {
        let never = AtomicBool::new(false);
        // The preview is optional: a card without it still works.
        if let Ok(bytes) = pano::thumbnail(self.client(), info, 2048, &never) {
            let mut state = self.shared.state.lock().unwrap();
            let current = state.items.get(id).and_then(|i| i.info.as_ref()).is_some_and(|i| i.id == info.id);
            if current {
                state.thumbs.insert(id.to_string(), bytes);
                if let Some(item) = state.items.get_mut(id) {
                    item.thumb_ver += 1;
                }
            }
        }
    }

    fn switch_date(&self, id: &str, capture: DateEntry) {
        let lang = self.lang();
        match self.client().metadata(&capture.id, capture.kind) {
            Ok(info) => {
                {
                    let mut state = self.shared.state.lock().unwrap();
                    let Some(item) = state.items.get_mut(id) else { return };
                    item.dates = merge_dates(&item.dates, &info);
                    item.info = Some(info.clone());
                    item.status = Status::Ready;
                    item.path = None;
                    item.error = None;
                    item.notice = None;
                }
                self.make_thumb(id, &info);
            }
            Err(e) => {
                if let Some(item) = self.shared.state.lock().unwrap().items.get_mut(id) {
                    item.status = Status::Ready;
                    item.notice = Some(format!("{}{}", lang.msg(Msg::SwitchFailed), lang.service(&e)));
                }
            }
        }
    }

    fn download(&self, id: &str, options: &Options) {
        let (info, link, cancel) = {
            let mut state = self.shared.state.lock().unwrap();
            let Some(item) = state.items.get_mut(id) else { return };
            if item.status != Status::Queued {
                return;
            }
            let Some(info) = item.info.clone() else { return };
            item.status = Status::Downloading;
            item.progress = 0.0;
            item.cancel = Arc::new(AtomicBool::new(false));
            (info, item.link.clone(), item.cancel.clone())
        };
        let lang = self.lang();
        let me = self.clone();
        let progress = move |step: Step| {
            if let Some(item) = me.shared.state.lock().unwrap().items.get_mut(id) {
                item.progress = (step.fraction() * 1000.0).round() / 1000.0;
            }
        };
        let control = Control { cancel: &cancel, progress: &progress };
        let result = pano::download(self.client(), &info, &link, options, &control);
        let mut state = self.shared.state.lock().unwrap();
        let Some(item) = state.items.get_mut(id) else { return };
        match result {
            Ok(saved) => {
                item.status = Status::Done;
                item.progress = 1.0;
                item.path = Some(saved.path);
            }
            Err(pano::Error::Cancelled) => {
                item.status = Status::Ready;
                item.progress = 0.0;
                item.notice = Some(lang.msg(Msg::Cancelled).into());
            }
            Err(e) => {
                item.status = Status::Error;
                item.error = Some(lang.pano(&e));
            }
        }
    }

    /// The sharper picture for the viewer (4096 pixels wide), fetched once per panorama.
    fn view_texture(&self, id: &str) -> Option<Arc<Vec<u8>>> {
        let info = self.shared.state.lock().unwrap().items.get(id)?.info.clone()?;
        if let Some((pano_id, bytes)) = self.shared.state.lock().unwrap().views.get(id) {
            if *pano_id == info.id {
                return Some(bytes.clone());
            }
        }
        let never = AtomicBool::new(false);
        let bytes = Arc::new(pano::thumbnail(self.client(), &info, 4096, &never).ok()?);
        self.shared.state.lock().unwrap().views.insert(id.to_string(), (info.id.clone(), bytes.clone()));
        Some(bytes)
    }

    fn mini(&self, id: &str, kind: Kind) -> Option<Vec<u8>> {
        if let Some(bytes) = self.shared.minis.lock().unwrap().get(id) {
            return Some(bytes.clone());
        }
        let bytes = pano::mini(self.client(), id, kind).ok()?;
        self.shared.minis.lock().unwrap().insert(id.to_string(), bytes.clone());
        Some(bytes)
    }

    /// The whole panorama, untouched; the last one is kept for several crops.
    fn full_image(&self, info: &PanoInfo, control: &Control) -> Result<Arc<RgbImage>, pano::Error> {
        let mut cache = self.shared.full.lock().unwrap();
        if let Some((id, image)) = cache.as_ref() {
            if *id == info.id {
                (control.progress)(Step::Tiles(1, 1));
                return Ok(image.clone());
            }
        }
        // Free the previous one before fetching another.
        *cache = None;
        let fetched = pano::fetch_pano(self.client(), info, info.sizes.len().saturating_sub(1), control)?;
        let image = Arc::new(fetched.image);
        *cache = Some((info.id.clone(), image.clone()));
        Ok(image)
    }

    fn crop(&self, cid: &str, item_id: &str, crop: Crop, options: Options) {
        let lang = self.lang();
        let update = |f: &mut dyn FnMut(&mut CropJob)| {
            if let Some(job) = self.shared.state.lock().unwrap().crops.get_mut(cid) {
                f(job);
            }
        };
        let info = self.shared.state.lock().unwrap().items.get(item_id).and_then(|i| i.info.clone());
        let result = (|| -> Result<pano::Saved, String> {
            let info = info.ok_or_else(|| lang.msg(Msg::CropGone).to_string())?;
            update(&mut |job| job.stage = lang.msg(Msg::CropDownloading).into());
            let never = AtomicBool::new(false);
            let me = self.clone();
            let progress = move |step: Step| {
                if let Some(job) = me.shared.state.lock().unwrap().crops.get_mut(cid) {
                    job.progress = ((0.8 * step.fraction()) * 1000.0).round() / 1000.0;
                }
            };
            let control = Control { cancel: &never, progress: &progress };
            let image = self.full_image(&info, &control).map_err(|e| lang.pano(&e))?;
            update(&mut |job| {
                job.stage = lang.msg(Msg::CropExtracting).into();
                job.progress = 0.85;
            });
            std::fs::create_dir_all(&options.out_dir).map_err(|e| lang.pano(&pano::Error::Io(e)))?;
            pano::extract_crop(&image, &info, &options, &crop).map_err(|e| lang.pano(&e))
        })();
        update(&mut |job| match &result {
            Ok(saved) => {
                job.status = JobStatus::Done;
                job.progress = 1.0;
                job.out = Some(saved.path.clone());
                job.size = Some(saved.size);
                job.stage = lang.msg(Msg::Saved).into();
            }
            Err(e) => {
                job.status = JobStatus::Error;
                job.error = Some(e.clone());
            }
        });
    }

    fn hdri_open(&self, path: &Path) -> Result<Value, String> {
        let lang = self.lang();
        if !path.is_file() {
            return Err(lang.msg(Msg::HdriNoFile).into());
        }
        let size = hdri::dimensions(path).map_err(|e| lang.hdri(&e))?;
        let source = hdri::load(path).map_err(|e| lang.hdri(&e))?;
        let small = Arc::new(hdri::analysis_copy(&source.rgb));
        drop(source);
        let sid = self.new_id();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let job = HdriJob {
            sid: sid.clone(),
            src: path.to_path_buf(),
            name,
            small,
            size,
            status: JobStatus::Idle,
            progress: 0.0,
            out: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let opened = job.opened();
        self.shared.state.lock().unwrap().hdri.insert(sid, job);
        Ok(opened)
    }

    /// Opens an image in the HDRI converter before the page asks, for `--window <image>`. The
    /// page picks it up from the address (`?hdri=<sid>`).
    pub fn preopen_hdri(&self, path: &Path) -> Option<String> {
        self.hdri_open(path).ok().and_then(|v| v["sid"].as_str().map(str::to_string))
    }

    fn hdri_convert(&self, sid: &str, params: hdri::Params, width: Option<u32>, format: HdrFormat) {
        let lang = self.lang();
        let Some((src, cancel)) = self.shared.state.lock().unwrap().hdri.get(sid).map(|j| (j.src.clone(), j.cancel.clone()))
        else {
            return;
        };
        let me = self.clone();
        let progress = move |f: f32| {
            if let Some(job) = me.shared.state.lock().unwrap().hdri.get_mut(sid) {
                job.progress = ((0.1 + 0.9 * f) * 1000.0).round() / 1000.0;
            }
        };
        let out = hdri::output_path(&src, format);
        let result = hdri::load(&src).and_then(|source| hdri::write(&source, &params, width, format, &out, &cancel, &progress));
        if let Some(job) = self.shared.state.lock().unwrap().hdri.get_mut(sid) {
            match result {
                Ok(_) => {
                    job.status = JobStatus::Done;
                    job.out = Some(out);
                    job.progress = 1.0;
                }
                Err(e) => {
                    job.status = if cancel.load(Ordering::Relaxed) { JobStatus::Cancelled } else { JobStatus::Error };
                    job.error = Some(lang.hdri(&e));
                }
            }
        }
    }

    fn state_json(&self) -> Value {
        let state = self.shared.state.lock().unwrap();
        let items: Vec<Value> = state
            .order
            .iter()
            .filter_map(|id| state.items.get(id).map(|item| item.public(state.thumbs.contains_key(id))))
            .collect();
        let hdri: Map<String, Value> = state.hdri.iter().map(|(k, j)| (k.clone(), j.public())).collect();
        let crops: Map<String, Value> = state.crops.iter().map(|(k, j)| (k.clone(), j.public())).collect();
        drop(state);
        json!({"items": items, "settings": self.settings(), "hdri": hdri, "crops": crops})
    }

    fn parent_window(&self) -> Option<ParentWindow> {
        self.shared.parent.lock().unwrap().and_then(ParentWindow::new)
    }

    fn pick_folder(&self) -> Option<PathBuf> {
        let current = self.settings().outdir;
        let mut dialog = rfd::FileDialog::new().set_title(self.lang().msg(Msg::PickFolderTitle));
        if current.is_dir() {
            dialog = dialog.set_directory(&current);
        }
        if let Some(parent) = self.parent_window() {
            dialog = dialog.set_parent(&parent);
        }
        dialog.pick_folder()
    }

    fn pick_image(&self) -> Option<PathBuf> {
        let lang = self.lang();
        let current = self.settings().outdir;
        let mut dialog = rfd::FileDialog::new()
            .set_title(lang.msg(Msg::PickImageTitle))
            .add_filter(lang.msg(Msg::PanoramaFiles), &["jpg", "jpeg", "png", "tif", "tiff"]);
        if current.is_dir() {
            dialog = dialog.set_directory(&current);
        }
        if let Some(parent) = self.parent_window() {
            dialog = dialog.set_parent(&parent);
        }
        dialog.pick_file()
    }

    /// Whether a request comes from the app's own page.
    fn allowed(&self, request: &tiny_http::Request) -> bool {
        let header = |name: &str| {
            request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string())
        };
        if let Some(origin) = header("Origin") {
            if origin != format!("http://{}", header("Host").unwrap_or_default()) {
                return false;
            }
        }
        same_secret(&header("X-Token").unwrap_or_default(), &self.shared.token)
    }

    fn handle(&self, mut request: tiny_http::Request) {
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let query = url.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
        let response = match request.method() {
            tiny_http::Method::Get => self.get(&path, &query, &request),
            tiny_http::Method::Post => {
                if !self.allowed(&request) {
                    Reply::Json(403, json!({"error": "not allowed"}))
                } else {
                    let mut body = String::new();
                    let read = request.as_reader().take(MAX_BODY).read_to_string(&mut body);
                    match read.ok().and_then(|_| serde_json::from_str::<Value>(if body.is_empty() { "{}" } else { &body }).ok()) {
                        Some(data) if data.is_object() => self.post(&path, &data),
                        _ => Reply::Json(400, json!({"error": "bad request"})),
                    }
                }
            }
            _ => Reply::Json(405, json!({"error": "method not allowed"})),
        };
        let _ = request.respond(response.into_response());
    }

    fn get(&self, path: &str, query: &str, request: &tiny_http::Request) -> Reply {
        let last = path.rsplit('/').next().unwrap_or("").to_string();
        match path {
            "/" | "/index.html" => {
                let settings = self.settings();
                let theme = if matches!(settings.theme.as_str(), "light" | "dark") { settings.theme.as_str() } else { "" };
                Reply::Html(page(self.lang(), theme, &self.shared.token))
            }
            "/api/state" => {
                if self.allowed(request) {
                    Reply::Json(200, self.state_json())
                } else {
                    Reply::Json(403, json!({"error": "not allowed"}))
                }
            }
            _ if path.starts_with("/api/view/") => match self.view_texture(&last) {
                Some(bytes) => Reply::Jpeg(bytes.as_ref().clone()),
                None => Reply::Json(404, json!({})),
            },
            _ if path.starts_with("/api/mini/") => {
                let kind = if query.split('&').any(|p| p == "k=10") { Kind::User } else { Kind::Official };
                let id = links::percent_decode(&last);
                match self.mini(&id, kind) {
                    Some(bytes) => Reply::Jpeg(bytes),
                    None => Reply::Json(404, json!({})),
                }
            }
            _ if path.starts_with("/api/thumb/") => match self.shared.state.lock().unwrap().thumbs.get(&last) {
                Some(bytes) => Reply::Jpeg(bytes.clone()),
                None => Reply::Json(404, json!({})),
            },
            "/logo.svg" => Reply::Svg(include_str!("../assets/logo.svg").to_string()),
            _ => Reply::Json(404, json!({"error": "not found"})),
        }
    }

    fn post(&self, path: &str, data: &Value) -> Reply {
        let id = data["id"].as_str().unwrap_or("").to_string();
        let ok = || Reply::Json(200, json!({}));
        match path {
            "/api/ping" => {
                *self.shared.last_ping.lock().unwrap() = Instant::now();
                ok()
            }
            "/api/add" => {
                let links: Vec<String> =
                    data["links"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                Reply::Json(200, json!({"ids": self.add_links(&links)}))
            }
            "/api/remove" => {
                let mut state = self.shared.state.lock().unwrap();
                if let Some(item) = state.items.remove(&id) {
                    // If it was downloading, it stops.
                    item.cancel.store(true, Ordering::Relaxed);
                }
                state.order.retain(|o| *o != id);
                state.thumbs.remove(&id);
                state.views.remove(&id);
                ok()
            }
            "/api/switch" => {
                let panoid = data["panoid"].as_str().unwrap_or("");
                let capture = {
                    let mut state = self.shared.state.lock().unwrap();
                    let Some(item) = state.items.get_mut(&id) else { return ok() };
                    if !matches!(item.status, Status::Ready | Status::Done | Status::Error) {
                        return ok();
                    }
                    let Some(capture) = item.dates.iter().find(|d| d.id == panoid).cloned() else { return ok() };
                    item.status = Status::Resolving;
                    state.thumbs.remove(&id);
                    capture
                };
                let me = self.clone();
                std::thread::spawn(move || me.switch_date(&id, capture));
                ok()
            }
            "/api/hdri/open" | "/api/hdri/pick" | "/api/hdri/info" => {
                if path == "/api/hdri/info" {
                    let sid = data["sid"].as_str().unwrap_or("");
                    return match self.shared.state.lock().unwrap().hdri.get(sid) {
                        Some(job) => Reply::Json(200, job.opened()),
                        None => Reply::Json(200, json!({"cancelled": true})),
                    };
                }
                let file = if path == "/api/hdri/pick" {
                    match self.pick_image() {
                        Some(file) => file,
                        None => return Reply::Json(200, json!({"cancelled": true})),
                    }
                } else {
                    let state = self.shared.state.lock().unwrap();
                    state.items.get(&id).and_then(|i| i.path.clone()).unwrap_or_default()
                };
                match self.hdri_open(&file) {
                    Ok(opened) => Reply::Json(200, opened),
                    Err(e) => Reply::Json(200, json!({"error": e})),
                }
            }
            "/api/crop" => self.post_crop(&id, data),
            "/api/hdri/preview" => {
                let sid = data["sid"].as_str().unwrap_or("");
                let Some(small) = self.shared.state.lock().unwrap().hdri.get(sid).map(|j| j.small.clone()) else {
                    return Reply::Json(404, json!({}));
                };
                let view_ev = data["view_ev"].as_f64().unwrap_or(0.0).clamp(-16.0, 4.0) as f32;
                let (params, _, _) = hdri_params(&data["params"]);
                let model = hdri::Model::new(&small, &params);
                let (image, info) = model.preview(&small, view_ev);
                let mut jpeg = Vec::new();
                let encoded = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88).encode_image(&image);
                if encoded.is_err() {
                    return Reply::Json(500, json!({}));
                }
                Reply::Json(200, json!({"img": format!("data:image/jpeg;base64,{}", base64(&jpeg)), "info": info}))
            }
            "/api/hdri/convert" => {
                let sid = data["sid"].as_str().unwrap_or("").to_string();
                let (params, width, format) = hdri_params(&data["params"]);
                let start = {
                    let mut state = self.shared.state.lock().unwrap();
                    match state.hdri.get_mut(&sid) {
                        Some(job) if job.status != JobStatus::Working => {
                            job.status = JobStatus::Working;
                            job.progress = 0.0;
                            job.out = None;
                            job.error = None;
                            job.cancel = Arc::new(AtomicBool::new(false));
                            true
                        }
                        _ => false,
                    }
                };
                if start {
                    let me = self.clone();
                    std::thread::spawn(move || me.hdri_convert(&sid, params, width, format));
                }
                ok()
            }
            "/api/hdri/cancel" => {
                let sid = data["sid"].as_str().unwrap_or("");
                if let Some(job) = self.shared.state.lock().unwrap().hdri.get(sid) {
                    if job.status == JobStatus::Working {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
                ok()
            }
            "/api/hdri/close" => {
                let sid = data["sid"].as_str().unwrap_or("");
                let mut state = self.shared.state.lock().unwrap();
                if state.hdri.get(sid).is_some_and(|j| j.status != JobStatus::Working) {
                    state.hdri.remove(sid);
                }
                ok()
            }
            "/api/cancel" => {
                let lang = self.lang();
                let mut state = self.shared.state.lock().unwrap();
                if let Some(item) = state.items.get_mut(&id) {
                    match item.status {
                        Status::Queued => {
                            item.status = Status::Ready;
                            item.notice = Some(lang.msg(Msg::Cancelled).into());
                        }
                        Status::Downloading => item.cancel.store(true, Ordering::Relaxed),
                        _ => {}
                    }
                }
                ok()
            }
            "/api/clear-done" => {
                let mut state = self.shared.state.lock().unwrap();
                let finished: Vec<String> =
                    state.items.values().filter(|i| matches!(i.status, Status::Done | Status::Error)).map(|i| i.id.clone()).collect();
                for gone in &finished {
                    state.items.remove(gone);
                    state.thumbs.remove(gone);
                    state.views.remove(gone);
                }
                state.order.retain(|o| !finished.contains(o));
                ok()
            }
            "/api/settings" => {
                let mut settings = self.shared.settings.lock().unwrap();
                settings.apply_patch(data);
                settings.save();
                Reply::Json(200, serde_json::to_value(&*settings).unwrap_or(Value::Null))
            }
            "/api/pick-folder" => {
                let picked = self.pick_folder();
                if let Some(folder) = &picked {
                    let mut settings = self.shared.settings.lock().unwrap();
                    settings.outdir = folder.clone();
                    settings.save();
                }
                Reply::Json(200, json!({"path": picked}))
            }
            "/api/download" => {
                let options = self.settings().options();
                let ids: Vec<String> =
                    data["ids"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                for queued in ids {
                    let mut state = self.shared.state.lock().unwrap();
                    let Some(item) = state.items.get_mut(&queued) else { continue };
                    if matches!(item.status, Status::Ready | Status::Done | Status::Error) && item.info.is_some() {
                        item.status = Status::Queued;
                        item.progress = 0.0;
                        item.error = None;
                        item.notice = None;
                        drop(state);
                        let _ = self.shared.downloads.lock().unwrap().send((queued, options.clone()));
                    }
                }
                ok()
            }
            "/api/open" => {
                self.post_open(&id, data);
                ok()
            }
            _ => Reply::Json(404, json!({"error": "not found"})),
        }
    }

    fn post_crop(&self, id: &str, data: &Value) -> Reply {
        let lang = self.lang();
        let number = |key: &str, default: f64| data[key].as_f64().unwrap_or(default);
        let (yaw, pitch, fov, aspect) = (number("yaw", 0.0), number("pitch", 0.0), number("fov", 60.0), number("aspect", 1.5));
        if !(yaw.is_finite() && pitch.is_finite() && fov.is_finite() && aspect.is_finite()) {
            return Reply::Json(400, json!({"error": "bad request"}));
        }
        let flat = {
            let state = self.shared.state.lock().unwrap();
            match state.items.get(id).and_then(|i| i.info.as_ref()) {
                None => return Reply::Json(200, json!({"error": lang.msg(Msg::CropNotReady)})),
                Some(info) => info.flat,
            }
        };
        if flat {
            return Reply::Json(200, json!({"error": lang.msg(Msg::CropNotForPhotos)}));
        }
        let half_pi = std::f64::consts::FRAC_PI_2;
        let crop = Crop::from_viewer(yaw, pitch.clamp(-half_pi, half_pi), fov.clamp(5.0, 120.0), aspect.clamp(0.2, 5.0));
        let options = self.settings().options();
        let cid = self.new_id();
        self.shared.state.lock().unwrap().crops.insert(
            cid.clone(),
            CropJob {
                cid: cid.clone(),
                item: id.to_string(),
                status: JobStatus::Working,
                progress: 0.0,
                stage: lang.msg(Msg::CropPreparing).into(),
                out: None,
                error: None,
                size: None,
            },
        );
        let (me, worker_cid, item) = (self.clone(), cid.clone(), id.to_string());
        std::thread::spawn(move || me.crop(&worker_cid, &item, crop, options));
        Reply::Json(200, json!({"cid": cid}))
    }

    fn post_open(&self, id: &str, data: &Value) {
        let what = data["what"].as_str().unwrap_or("");
        let select = data["mode"].as_str() != Some("file");
        let state = self.shared.state.lock().unwrap();
        let target = match what {
            "crop" => state.crops.get(data["cid"].as_str().unwrap_or("")).and_then(|j| j.out.clone()).map(|p| (p, select)),
            "hdri" => state.hdri.get(data["sid"].as_str().unwrap_or("")).and_then(|j| j.out.clone()).map(|p| (p, true)),
            "outdir" => {
                drop(state);
                let folder = self.settings().outdir;
                let _ = std::fs::create_dir_all(&folder);
                reveal(&folder, false);
                return;
            }
            _ => state.items.get(id).and_then(|i| i.path.clone()).map(|p| (p, what != "file")),
        };
        drop(state);
        if let Some((path, select)) = target {
            if path.exists() {
                reveal(&path, select);
            }
        }
    }
}

/// The HDRI parameters the page sends, kept in range: (parameters, width, format).
fn hdri_params(value: &Value) -> (hdri::Params, Option<u32>, HdrFormat) {
    let d = hdri::Params::default();
    let number = |key: &str, default: f32| value[key].as_f64().map_or(default, |v| v as f32);
    let sun_pos = value["sun_pos"].as_array().and_then(|a| match a.as_slice() {
        [u, v] => Some((u.as_f64()? as f32, v.as_f64()? as f32)),
        _ => None,
    });
    let params = hdri::Params {
        sun_ratio: number("sun_ratio", d.sun_ratio),
        sun_size: number("sun_size", d.sun_size),
        highlights: number("highlights", d.highlights),
        exposure: number("exposure", d.exposure),
        sun_pos,
    }
    .clamped();
    let width = match value["resolution"].as_u64() {
        Some(0) => None,
        Some(w @ (2048 | 4096 | 8192)) => Some(w as u32),
        _ => Some(8192),
    };
    let format = if value["format"].as_str() == Some("hdr") { HdrFormat::Hdr } else { HdrFormat::Exr };
    (params, width, format)
}

/// Shows a file in its folder (`select`) or opens it with its program.
fn reveal(path: &Path, select: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("explorer");
        if select {
            command.raw_arg(format!("/select,\"{}\"", path.display()));
        } else {
            command.raw_arg(format!("\"{}\"", path.display()));
        }
        let _ = command.spawn();
    }
    #[cfg(not(windows))]
    let _ = (path, select);
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { BASE64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { BASE64[n as usize & 63] as char } else { '=' });
    }
    out
}

enum Reply {
    Json(u16, Value),
    Html(String),
    Jpeg(Vec<u8>),
    Svg(String),
}

impl Reply {
    fn into_response(self) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        let (status, kind, body) = match self {
            Reply::Json(status, value) => (status, "application/json", serde_json::to_vec(&value).unwrap_or_default()),
            Reply::Html(text) => (200, "text/html; charset=utf-8", text.into_bytes()),
            Reply::Jpeg(bytes) => (200, "image/jpeg", bytes),
            Reply::Svg(text) => (200, "image/svg+xml", text.into_bytes()),
        };
        let header = |name: &str, value: &str| tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header");
        tiny_http::Response::from_data(body)
            .with_status_code(status)
            .with_header(header("Content-Type", kind))
            .with_header(header("Cache-Control", "no-store"))
    }
}

/// The window as a parent for dialogs.
struct ParentWindow(std::num::NonZeroIsize);

impl ParentWindow {
    fn new(hwnd: isize) -> Option<ParentWindow> {
        std::num::NonZeroIsize::new(hwnd).map(ParentWindow)
    }
}

impl raw_window_handle::HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::Win32WindowHandle::new(self.0);
        // SAFETY: the window lives as long as the app; dialogs are only shown while it is open.
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(handle)) })
    }
}

impl raw_window_handle::HasDisplayHandle for ParentWindow {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::windows())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_in_page() -> Vec<String> {
        let mut keys = Vec::new();
        let mut rest = PAGE;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let end = after.find("}}").unwrap();
            keys.push(after[..end].to_string());
            rest = &after[end + 2..];
        }
        keys
    }

    /// Every `T.name` the page's script uses.
    fn keys_in_script() -> Vec<String> {
        let bytes = PAGE.as_bytes();
        let mut keys = Vec::new();
        for (i, _) in PAGE.match_indices("T.") {
            if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'.') {
                continue;
            }
            let name: String = PAGE[i + 2..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                keys.push(name);
            }
        }
        keys
    }

    #[test]
    fn both_languages_have_every_text() {
        let (en, es) = (strings(Lang::En), strings(Lang::Es));
        let en_keys: Vec<_> = en.keys().collect();
        let es_keys: Vec<_> = es.keys().collect();
        assert_eq!(en_keys, es_keys, "the two languages have different texts");
        for key in keys_in_page().iter().chain(keys_in_script().iter()) {
            if key != "lang" && key != "theme" {
                assert!(en.contains_key(key), "the page uses {key}, which has no text");
            }
        }
        for (key, value) in en {
            assert!(value.is_string(), "{key} is not text");
        }
    }

    #[test]
    fn the_page_is_filled_in() {
        let html = page(Lang::Es, "dark", "secret-token");
        assert!(html.contains(r#"<html lang="es" data-theme="dark">"#));
        assert!(html.contains("secret-token") && !html.contains("__APP_TOKEN__") && !html.contains("__I18N__"));
        assert!(!html.contains("{{"), "a text was left unfilled");
        assert!(html.contains("Descarga tu primer panorama"));
        let english = page(Lang::En, "", "t");
        assert!(english.contains(r#"<html lang="en" data-theme="">"#) && english.contains("Download your first panorama"));
    }

    #[test]
    fn secrets_are_compared_whole() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd") && !same_secret("abc", "ab"));
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn hdri_parameters_from_the_page_are_kept_in_range() {
        let (p, width, format) = hdri_params(&json!({"sun_ratio": 50, "sun_size": 0.1, "highlights": 2, "exposure": 1.5,
            "resolution": 0, "format": "hdr", "sun_pos": [0.25, 1.5]}));
        assert_eq!((p.sun_ratio, p.sun_size, p.highlights, p.exposure), (20.0, 0.3, 2.0, 1.5));
        assert_eq!((p.sun_pos, width, format), (Some((0.25, 1.0)), None, HdrFormat::Hdr));
        let (p, width, format) = hdri_params(&json!({"resolution": 1234, "format": "bmp", "sun_pos": "x"}));
        assert_eq!((p, width, format), (hdri::Params::default(), Some(8192), HdrFormat::Exr));
    }

    #[test]
    fn dates_are_merged_newest_first_without_repeats() {
        use crate::service::Capture;
        let info = PanoInfo {
            id: "B".into(),
            kind: Kind::Official,
            flat: false,
            sizes: vec![(512, 256)],
            tile: (512, 512),
            lat: None,
            lon: None,
            heading: None,
            tilt: None,
            roll: None,
            date: Some((2019, 8, None)),
            author: None,
            copyright: None,
            address: None,
            history: vec![
                Capture { id: "A".into(), kind: Kind::Official, year: 2024, month: 11 },
                Capture { id: "C".into(), kind: Kind::User, year: 2016, month: 5 },
            ],
            image_url: None,
        };
        let old = vec![DateEntry { id: "A".into(), date: "2024-11".into(), kind: Kind::Official }];
        let merged = merge_dates(&old, &info);
        let ids: Vec<_> = merged.iter().map(|d| (d.id.as_str(), d.date.as_str())).collect();
        assert_eq!(ids, vec![("A", "2024-11"), ("B", "2019-08"), ("C", "2016-05")]);
    }

    #[test]
    fn the_server_answers_only_its_own_page() {
        let server = Server::start().expect("started");
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let url = server.url().to_string();
        let page = agent.get(&url).call().unwrap().body_mut().read_to_string().unwrap();
        let token = &server.shared.token;
        assert!(page.contains(token.as_str()));
        // Without the token, or from another site, orders are refused.
        let refused = agent.post(&format!("{url}api/settings")).send("{}").unwrap();
        assert_eq!(refused.status().as_u16(), 403);
        let other_site = agent
            .post(&format!("{url}api/settings"))
            .header("X-Token", token.as_str())
            .header("Origin", "http://evil.example")
            .send("{}")
            .unwrap();
        assert_eq!(other_site.status().as_u16(), 403);
        let bad = agent.post(&format!("{url}api/settings")).header("X-Token", token.as_str()).send("not json").unwrap();
        assert_eq!(bad.status().as_u16(), 400);
        let state = agent
            .get(&format!("{url}api/state"))
            .header("X-Token", token.as_str())
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        let state: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["items"], json!([]));
        assert!(state["settings"]["outdir"].is_string());
        let missing = agent.get(&format!("{url}nothing")).call().unwrap();
        assert_eq!(missing.status().as_u16(), 404);
    }
}
