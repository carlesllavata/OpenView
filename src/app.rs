// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: a list of cards (one per pasted link), a 360 viewer with a crop frame and the
//! HDRI converter. Everything slow runs on background threads that report back through a channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use eframe::egui::{self, pos2, vec2, Align, Color32, Layout, Rect, RichText, Sense, Stroke, TextureHandle, Vec2};
use image::RgbImage;

use crate::extras;
use crate::geom::{self, Quality, View};
use crate::hdri::{self, HdrFormat, Sun};
use crate::links::{self, Kind};
use crate::pano::{self, Control, Crop, Format, Saved, Step};
use crate::service::{Capture, Client, Found, PanoInfo};
use crate::settings::Settings;

const ACCENT: Color32 = Color32::from_rgb(51, 184, 168);
const THUMB: Vec2 = vec2(240.0, 120.0);
const HISTORY_THUMB: Vec2 = vec2(112.0, 56.0);
const ASPECTS: [(&str, f64); 6] =
    [("16:9", 16.0 / 9.0), ("3:2", 1.5), ("4:3", 4.0 / 3.0), ("1:1", 1.0), ("4:5", 0.8), ("9:16", 9.0 / 16.0)];
const HDRI_WIDTHS: [(&str, Option<u32>); 4] =
    [("2K", Some(2048)), ("4K", Some(4096)), ("8K", Some(8192)), ("Original", None)];

/// Opens the window and runs until it is closed. `open` are links to add straight away; a path to
/// an image on disk opens the HDRI converter with it instead.
pub fn run(open: Vec<String>) -> Result<(), String> {
    let settings = Settings::load();
    let icon = egui::IconData { rgba: crate::icon::rgba(64), width: 64, height: 64 };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("OpenView")
        .with_min_inner_size([820.0, 560.0])
        .with_inner_size([1120.0, 780.0])
        .with_icon(icon);
    if let Some([x, y, w, h]) = settings.window {
        viewport = viewport.with_inner_size([w.max(820.0), h.max(560.0)]);
        if on_a_monitor(x * settings.window_scale, y * settings.window_scale) {
            viewport = viewport.with_position([x, y]);
        }
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("OpenView", options, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, settings, open)))))
        .map_err(|e| e.to_string())
}

/// Whether a point of the desktop (in screen pixels) is on a connected screen: a window must not
/// be restored to a monitor that is no longer there.
#[cfg(windows)]
fn on_a_monitor(x: f32, y: f32) -> bool {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromPoint(point: Point, flags: u32) -> isize;
    }
    // SAFETY: a plain Win32 query taking a point by value; 0 asks for null when no monitor has it.
    unsafe { MonitorFromPoint(Point { x: x as i32 + 60, y: y as i32 + 20 }, 0) != 0 }
}

#[cfg(not(windows))]
fn on_a_monitor(_x: f32, _y: f32) -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Download,
    Crop,
}

struct Job {
    cancel: Arc<AtomicBool>,
    step: Step,
}

/// One pasted link.
struct Card {
    id: u64,
    link: String,
    found: Option<Found>,
    error: Option<String>,
    /// A change of date is being looked up.
    switching: bool,
    thumb: Option<TextureHandle>,
    history_thumbs: HashMap<String, TextureHandle>,
    /// The image the viewer shows, with the panorama it belongs to.
    preview: Option<(String, Arc<RgbImage>)>,
    preview_requested: Option<String>,
    job: Option<Job>,
    downloaded: Option<Result<Saved, String>>,
    cropped: Option<Result<Saved, String>>,
}

impl Card {
    fn new(id: u64, link: String) -> Self {
        Card {
            id,
            link,
            found: None,
            error: None,
            switching: false,
            thumb: None,
            history_thumbs: HashMap::new(),
            preview: None,
            preview_requested: None,
            job: None,
            downloaded: None,
            cropped: None,
        }
    }

    fn pano(&self) -> Option<&PanoInfo> {
        match &self.found {
            Some(Found::Pano(info)) => Some(info),
            _ => None,
        }
    }

    /// The identifier of what the card shows now; answers for anything else are stale.
    fn key(&self) -> &str {
        match &self.found {
            Some(Found::Pano(info)) => &info.id,
            Some(Found::Photo(photo)) => &photo.id,
            None => "",
        }
    }
}

enum Msg {
    Found(u64, Result<Found, String>),
    Thumb(u64, String, RgbImage),
    HistoryThumb(u64, String, RgbImage),
    Preview(u64, String, Arc<RgbImage>),
    Step(u64, Step),
    Done(u64, JobKind, Result<Saved, String>),
    HdriLoaded(u64, Result<(Arc<hdri::Source>, RgbImage), String>),
    HdriSaved(u64, Result<(PathBuf, (u32, u32)), String>),
}

/// What a click asked for; applied after the frame is drawn.
enum Action {
    Remove(u64),
    Download(u64),
    Cancel(u64),
    OpenViewer(u64, bool),
    SwitchDate(u64, Capture),
    ExtractCrop(u64, Crop),
    OpenHdri(PathBuf),
    Reveal(PathBuf),
    Back,
}

/// The way background threads talk to the window.
#[derive(Clone)]
struct Reporter {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Reporter {
    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

struct Viewer {
    card: u64,
    view: View,
    texture: Option<TextureHandle>,
    /// What the texture shows, so it is only redrawn when something changes.
    drawn: Option<(View, [u32; 2], usize, bool)>,
    crop: bool,
    aspect: usize,
    /// Size of the picture on screen, as last drawn.
    area: Vec2,
}

struct HdriPanel {
    token: u64,
    path: PathBuf,
    source: Option<Arc<hdri::Source>>,
    small: Option<RgbImage>,
    error: Option<String>,
    params: hdri::Params,
    ev: f32,
    only_sun: bool,
    width: Option<u32>,
    format: HdrFormat,
    texture: Option<TextureHandle>,
    drawn: Option<(hdri::Params, f32)>,
    sun: Option<(f32, f32)>,
    saving: bool,
    result: Option<Result<(PathBuf, (u32, u32)), String>>,
}

#[derive(PartialEq)]
enum Screen {
    List,
    Viewer,
    Hdri,
}

pub struct App {
    settings: Settings,
    saved: Settings,
    client: Client,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cards: Vec<Card>,
    next_id: u64,
    link_text: String,
    screen: Screen,
    viewer: Option<Viewer>,
    hdri: Option<HdriPanel>,
    confirm_close: bool,
    close_anyway: bool,
}

fn texture(ctx: &egui::Context, name: String, image: &RgbImage) -> TextureHandle {
    let size = [image.width() as usize, image.height() as usize];
    ctx.load_texture(name, egui::ColorImage::from_rgb(size, image.as_raw()), egui::TextureOptions::LINEAR)
}

fn whole() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

fn size_text(size: (u32, u32)) -> String {
    format!("{} x {}", size.0, size.1)
}

impl App {
    fn new(ctx: &egui::Context, settings: Settings, open: Vec<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        ctx.set_theme(if settings.dark { egui::Theme::Dark } else { egui::Theme::Light });
        let mut app = App {
            saved: settings.clone(),
            settings,
            client: Client::new(),
            tx,
            rx,
            cards: Vec::new(),
            next_id: 1,
            link_text: String::new(),
            screen: Screen::List,
            viewer: None,
            hdri: None,
            confirm_close: false,
            close_anyway: false,
        };
        for item in open {
            if Path::new(&item).is_file() {
                app.open_hdri(ctx, PathBuf::from(item));
            } else {
                app.add_links(ctx, &item);
            }
        }
        app
    }

    fn spawn(&self, ctx: &egui::Context, work: impl FnOnce(Client, Reporter) + Send + 'static) {
        let reporter = Reporter { tx: self.tx.clone(), ctx: ctx.clone() };
        let client = self.client.clone();
        std::thread::spawn(move || work(client, reporter));
    }

    fn card_mut(&mut self, id: u64) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.id == id)
    }

    fn busy(&self) -> bool {
        self.cards.iter().any(|c| c.job.is_some()) || self.hdri.as_ref().is_some_and(|h| h.saving)
    }

    fn add_links(&mut self, ctx: &egui::Context, text: &str) {
        for link in links::split_links(text) {
            if self.cards.iter().any(|c| c.link == link) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.cards.insert(0, Card::new(id, link.clone()));
            self.spawn(ctx, move |client, report| {
                let found = client.find(&link).map_err(|e| e.to_string());
                report.send(Msg::Found(id, found));
            });
        }
    }

    /// Fetches the pictures of a card: its own and those of the other dates.
    fn request_thumbs(&self, ctx: &egui::Context, id: u64, found: &Found) {
        let found = found.clone();
        self.spawn(ctx, move |client, report| {
            let never = AtomicBool::new(false);
            match &found {
                Found::Pano(info) => {
                    if let Ok(image) = pano::fetch_preview(&client, info, 512, &never) {
                        report.send(Msg::Thumb(id, info.id.clone(), image));
                    }
                    for capture in &info.history {
                        let url = crate::service::capture_thumb_url(&capture.id, capture.kind);
                        let Ok(bytes) = client.get_bytes(&url) else { continue };
                        let Ok(image) = image::load_from_memory(&bytes) else { continue };
                        let image = image.into_rgb8();
                        // A user's tile holds the picture in its top-left corner only.
                        let image = if capture.kind == Kind::User { extras::trim_padding(&image) } else { image };
                        let small = image::imageops::resize(&image, 224, 112, image::imageops::FilterType::Triangle);
                        report.send(Msg::HistoryThumb(id, capture.id.clone(), small));
                    }
                }
                Found::Photo(photo) => {
                    if let Ok(image) = client.get_bytes(&photo.preview_url(512)).map(|b| image::load_from_memory(&b)) {
                        if let Ok(image) = image {
                            report.send(Msg::Thumb(id, photo.id.clone(), image.into_rgb8()));
                        }
                    }
                }
            }
        });
    }

    /// Fetches the image the viewer needs: a quick one first, then a sharper one.
    fn request_preview(&mut self, ctx: &egui::Context, id: u64) {
        let Some(card) = self.card_mut(id) else { return };
        let Some(info) = card.pano().cloned() else { return };
        if card.preview_requested.as_deref() == Some(info.id.as_str()) {
            return;
        }
        card.preview_requested = Some(info.id.clone());
        self.spawn(ctx, move |client, report| {
            let never = AtomicBool::new(false);
            let mut last_width = 0;
            for width in [2048, 4096] {
                let zoom = info.zoom_for(Some(width));
                if info.sizes[zoom].0 == last_width {
                    continue;
                }
                last_width = info.sizes[zoom].0;
                if let Ok(image) = pano::fetch_preview(&client, &info, width, &never) {
                    let image = extras::complete_sphere(&image).unwrap_or(image);
                    report.send(Msg::Preview(id, info.id.clone(), Arc::new(image)));
                }
            }
        });
    }

    fn start_job(&mut self, ctx: &egui::Context, id: u64, crop: Option<Crop>) {
        let options = self.settings.options();
        let Some(card) = self.card_mut(id) else { return };
        if card.job.is_some() {
            return;
        }
        let Some(found) = card.found.clone() else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        let kind = if crop.is_some() { JobKind::Crop } else { JobKind::Download };
        card.job = Some(Job { cancel: cancel.clone(), step: Step::Tiles(0, 1) });
        match kind {
            JobKind::Download => card.downloaded = None,
            JobKind::Crop => card.cropped = None,
        }
        self.spawn(ctx, move |client, report| {
            let progress_report = report.clone();
            let progress = move |step: Step| progress_report.send(Msg::Step(id, step));
            let control = Control { cancel: &cancel, progress: &progress };
            let result = match (&found, crop) {
                (Found::Pano(info), Some(crop)) => pano::extract_crop(&client, info, &options, &crop, &control),
                (Found::Pano(info), None) => pano::download(&client, info, &options, &control),
                (Found::Photo(photo), _) => pano::download_photo(&client, photo, &options.out_dir, &control),
            };
            report.send(Msg::Done(id, kind, result.map_err(|e| e.to_string())));
        });
    }

    fn open_hdri(&mut self, ctx: &egui::Context, path: PathBuf) {
        let token = self.next_id;
        self.next_id += 1;
        self.hdri = Some(HdriPanel {
            token,
            path: path.clone(),
            source: None,
            small: None,
            error: None,
            params: hdri::Params {
                sun_strength: self.settings.sun_strength,
                sun_size_deg: self.settings.sun_size_deg,
                sun: Sun::Auto,
            },
            ev: 0.0,
            only_sun: false,
            width: self.settings.hdri_width,
            format: self.settings.hdri_format,
            texture: None,
            drawn: None,
            sun: None,
            saving: false,
            result: None,
        });
        self.screen = Screen::Hdri;
        self.spawn(ctx, move |_, report| {
            let loaded = hdri::load(&path)
                .map(|source| {
                    let small = hdri::analysis_copy(&source.rgb);
                    (Arc::new(source), small)
                })
                .map_err(|e| e.to_string());
            report.send(Msg::HdriLoaded(token, loaded));
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Found(id, result) => {
                    let Some(card) = self.card_mut(id) else { continue };
                    card.switching = false;
                    match result {
                        Ok(found) => {
                            card.error = None;
                            card.thumb = None;
                            card.history_thumbs.clear();
                            card.preview = None;
                            card.preview_requested = None;
                            card.downloaded = None;
                            card.cropped = None;
                            card.found = Some(found.clone());
                            self.request_thumbs(ctx, id, &found);
                            if self.viewer.as_ref().is_some_and(|v| v.card == id) {
                                self.request_preview(ctx, id);
                            }
                        }
                        Err(e) => card.error = Some(e),
                    }
                }
                Msg::Thumb(id, key, image) => {
                    if let Some(card) = self.card_mut(id).filter(|c| c.key() == key) {
                        card.thumb = Some(texture(ctx, format!("thumb-{id}"), &image));
                    }
                }
                Msg::HistoryThumb(id, capture, image) => {
                    if let Some(card) = self.card_mut(id) {
                        if card.pano().is_some_and(|p| p.history.iter().any(|c| c.id == capture)) {
                            let handle = texture(ctx, format!("history-{id}-{capture}"), &image);
                            card.history_thumbs.insert(capture, handle);
                        }
                    }
                }
                Msg::Preview(id, key, image) => {
                    if let Some(card) = self.card_mut(id).filter(|c| c.key() == key) {
                        card.preview = Some((key, image));
                    }
                }
                Msg::Step(id, step) => {
                    if let Some(job) = self.card_mut(id).and_then(|c| c.job.as_mut()) {
                        job.step = step;
                    }
                }
                Msg::Done(id, kind, result) => {
                    if let Some(card) = self.card_mut(id) {
                        card.job = None;
                        let result = match result {
                            Err(e) if e == pano::Error::Cancelled.to_string() => None,
                            other => Some(other),
                        };
                        match kind {
                            JobKind::Download => card.downloaded = result,
                            JobKind::Crop => card.cropped = result,
                        }
                    }
                }
                Msg::HdriLoaded(token, result) => {
                    if let Some(panel) = self.hdri.as_mut().filter(|p| p.token == token) {
                        match result {
                            Ok((source, small)) => {
                                panel.source = Some(source);
                                panel.small = Some(small);
                            }
                            Err(e) => panel.error = Some(e),
                        }
                    }
                }
                Msg::HdriSaved(token, result) => {
                    if let Some(panel) = self.hdri.as_mut().filter(|p| p.token == token) {
                        panel.saving = false;
                        panel.result = Some(result);
                    }
                }
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Remove(id) => {
                if let Some(job) = self.card_mut(id).and_then(|c| c.job.as_ref()) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                self.cards.retain(|c| c.id != id);
            }
            Action::Download(id) => self.start_job(ctx, id, None),
            Action::Cancel(id) => {
                if let Some(job) = self.card_mut(id).and_then(|c| c.job.as_ref()) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            Action::OpenViewer(id, crop) => {
                let heading_view = View { yaw: 0.0, pitch: 0.0, hfov: 90f64.to_radians() };
                self.viewer =
                    Some(Viewer { card: id, view: heading_view, texture: None, drawn: None, crop, aspect: 0, area: vec2(16.0, 9.0) });
                self.screen = Screen::Viewer;
                self.request_preview(ctx, id);
            }
            Action::SwitchDate(id, capture) => {
                if let Some(card) = self.card_mut(id) {
                    if card.switching || card.job.is_some() {
                        return;
                    }
                    card.switching = true;
                }
                self.spawn(ctx, move |client, report| {
                    let found = client.metadata(&capture.id, capture.kind).map(Found::Pano).map_err(|e| e.to_string());
                    report.send(Msg::Found(id, found));
                });
            }
            Action::ExtractCrop(id, crop) => self.start_job(ctx, id, Some(crop)),
            Action::OpenHdri(path) => self.open_hdri(ctx, path),
            Action::Reveal(path) => reveal(&path),
            Action::Back => self.screen = Screen::List,
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("OpenView").strong().size(18.0).color(ACCENT));
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.settings.dark { "Light" } else { "Dark" };
                if ui.button(label).on_hover_text("Switch between the dark and the light theme").clicked() {
                    self.settings.dark = !self.settings.dark;
                    ctx.set_theme(if self.settings.dark { egui::Theme::Dark } else { egui::Theme::Light });
                }
                if ui
                    .button("Image to HDRI...")
                    .on_hover_text("Convert any 360 image on your computer (JPG, PNG or TIFF) to an HDRI")
                    .clicked()
                {
                    let picked =
                        rfd::FileDialog::new().add_filter("360 image", &["jpg", "jpeg", "png", "tif", "tiff"]).pick_file();
                    if let Some(path) = picked {
                        self.open_hdri(&ctx, path);
                    }
                }
                let add = ui.button("Add");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.link_text)
                        .hint_text("Paste one or more links (Ctrl+V anywhere in the window)")
                        .desired_width(f32::INFINITY),
                );
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (add.clicked() || entered) && !self.link_text.trim().is_empty() {
                    let text = std::mem::take(&mut self.link_text);
                    self.add_links(&ctx, &text);
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Folder...").on_hover_text("Where downloads are saved").clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.settings.out_dir).pick_folder() {
                    self.settings.out_dir = dir;
                }
            }
            ui.add(egui::Label::new(RichText::new(self.settings.out_dir.display().to_string()).weak()).truncate());
            ui.separator();
            egui::ComboBox::from_id_salt("format").width(64.0).selected_text(self.settings.format.label()).show_ui(
                ui,
                |ui| {
                    for format in Format::ALL {
                        ui.selectable_value(&mut self.settings.format, format, format.label());
                    }
                },
            );
            ui.checkbox(&mut self.settings.level, "Level horizon")
                .on_hover_text("Straighten panoramas taken with a leaning camera: level horizon, vertical buildings");
            ui.checkbox(&mut self.settings.north, "Centre to north")
                .on_hover_text("Turn the image so its centre looks north; saved with _north in the name");
            ui.checkbox(&mut self.settings.complete, "Complete partials")
                .on_hover_text("Fill the missing sky and ground of 360 photos that do not cover the whole sphere");
            ui.checkbox(&mut self.settings.cover_ground, "Cover ground")
                .on_hover_text("Cover what is under the camera with a patch; saved with _nadir in the name");
            ui.checkbox(&mut self.settings.json, "Save data (.json)")
                .on_hover_text("Also write position, date, heading, author and link");
        });
        ui.add_space(6.0);
    }

    fn list(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.cards.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Paste a link to start").size(20.0));
                ui.add_space(8.0);
                ui.label("Open a panorama, a 360 photo or a photo in the map application,");
                ui.label("copy the address from the browser (or use Share) and press Ctrl+V here.");
                ui.add_space(8.0);
                ui.label(RichText::new("A link with only a position downloads the nearest street-level panorama.").weak());
            });
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for card in &self.cards {
                card_ui(ui, card, actions);
                ui.add_space(6.0);
            }
        });
    }

    fn viewer_screen(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let App { viewer, cards, settings, .. } = self;
        let Some(viewer) = viewer.as_mut() else {
            actions.push(Action::Back);
            return;
        };
        let Some(card) = cards.iter().find(|c| c.id == viewer.card) else {
            actions.push(Action::Back);
            return;
        };
        let Some(info) = card.pano() else {
            actions.push(Action::Back);
            return;
        };
        let full_width = info.full_size().0.max(info.full_size().1 * 2);
        let aspect = ASPECTS[viewer.aspect].1;

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("< Back").clicked() {
                actions.push(Action::Back);
            }
            ui.label(RichText::new(describe(info)).strong());
            ui.separator();
            ui.toggle_value(&mut viewer.crop, "Extract crop")
                .on_hover_text("Frame a part of the panorama and save it as an ordinary photo");
            if viewer.crop {
                for (index, (label, _)) in ASPECTS.iter().enumerate() {
                    ui.selectable_value(&mut viewer.aspect, index, *label);
                }
            }
        });
        ui.add_space(4.0);

        // The bottom line: what the crop will be, and the button.
        egui::Panel::bottom("viewer-bottom").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if viewer.crop {
                    let frame_fraction = crop_frame(viewer.area, aspect).width() / viewer.area.x.max(1.0);
                    let hfov = 2.0 * ((viewer.view.hfov / 2.0).tan() * frame_fraction as f64).atan();
                    let crop = Crop { view: View { hfov, ..viewer.view }, aspect };
                    ui.label(format!("The photo will be {} px", size_text(crop.size(full_width))));
                    match &card.job {
                        Some(job) => {
                            ui.add(egui::ProgressBar::new(job.step.fraction()).desired_width(220.0).text(job.step.label()));
                            if ui.button("Cancel").clicked() {
                                actions.push(Action::Cancel(card.id));
                            }
                        }
                        None => {
                            if ui.button(RichText::new("Extract").strong()).clicked() {
                                actions.push(Action::ExtractCrop(card.id, crop));
                            }
                        }
                    }
                    saved_line(ui, &card.cropped, actions, false);
                } else {
                    ui.label(RichText::new("Drag to look around, use the wheel to zoom.").weak());
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let size = ui.available_size();
            let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
            let rect = response.rect;
            viewer.area = rect.size();
            let Some((_, preview)) = &card.preview else {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Loading the panorama...", egui::FontId::proportional(16.0), ui.visuals().text_color());
                return;
            };
            // Look around.
            let per_pixel = viewer.view.hfov / rect.width().max(1.0) as f64;
            if response.dragged() {
                let delta = response.drag_delta();
                viewer.view.yaw -= delta.x as f64 * per_pixel;
                viewer.view.pitch = (viewer.view.pitch + delta.y as f64 * per_pixel).clamp(-1.55, 1.55);
            }
            if response.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let zoomed = viewer.view.hfov * (-scroll as f64 * 0.0025).exp();
                    viewer.view.hfov = zoomed.clamp(8f64.to_radians(), 125f64.to_radians());
                }
            }
            viewer.view.yaw = (viewer.view.yaw + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI;

            // Draw at screen resolution, but not larger than is quick to compute.
            let scale = ui.ctx().pixels_per_point().min(1280.0 / rect.width().max(1.0));
            let pixels = [(rect.width() * scale).max(16.0) as u32, (rect.height() * scale).max(16.0) as u32];
            let state = (viewer.view, pixels, preview.width() as usize, settings.level);
            if viewer.drawn != Some(state) {
                let level = if settings.level { pano::level_of(info) } else { None };
                let image = geom::perspective(preview, &viewer.view, pixels[0], pixels[1], level.as_ref(), Quality::Fast);
                let colour = egui::ColorImage::from_rgb([pixels[0] as usize, pixels[1] as usize], image.as_raw());
                match &mut viewer.texture {
                    Some(texture) => texture.set(colour, egui::TextureOptions::LINEAR),
                    None => viewer.texture = Some(ui.ctx().load_texture("viewer", colour, egui::TextureOptions::LINEAR)),
                }
                viewer.drawn = Some(state);
            }
            if let Some(texture) = &viewer.texture {
                painter.image(texture.id(), rect, whole(), Color32::WHITE);
            }
            if viewer.crop {
                let frame = crop_frame(rect.size(), aspect).translate(rect.min.to_vec2());
                let shade = Color32::from_black_alpha(140);
                painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.max.x, frame.min.y)), 0.0, shade);
                painter.rect_filled(Rect::from_min_max(pos2(rect.min.x, frame.max.y), rect.max), 0.0, shade);
                painter.rect_filled(Rect::from_min_max(pos2(rect.min.x, frame.min.y), pos2(frame.min.x, frame.max.y)), 0.0, shade);
                painter.rect_filled(Rect::from_min_max(pos2(frame.max.x, frame.min.y), pos2(rect.max.x, frame.max.y)), 0.0, shade);
                painter.rect_stroke(frame, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
                for third in [1.0 / 3.0, 2.0 / 3.0] {
                    let faint = Stroke::new(0.5, Color32::from_white_alpha(70));
                    let x = frame.min.x + frame.width() * third;
                    let y = frame.min.y + frame.height() * third;
                    painter.line_segment([pos2(x, frame.min.y), pos2(x, frame.max.y)], faint);
                    painter.line_segment([pos2(frame.min.x, y), pos2(frame.max.x, y)], faint);
                }
            }
        });
    }

    fn hdri_screen(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let reporter = Reporter { tx: self.tx.clone(), ctx: ui.ctx().clone() };
        let App { hdri: panel, settings, .. } = self;
        let Some(panel) = panel.as_mut() else {
            actions.push(Action::Back);
            return;
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("< Back").clicked() {
                actions.push(Action::Back);
            }
            ui.label(RichText::new("HDRI").strong());
            // Only the name: a long path would push the rest of the screen out of the window.
            ui.add(egui::Label::new(RichText::new(file_name(&panel.path)).weak()).truncate())
                .on_hover_text(panel.path.display().to_string());
        });
        ui.add_space(4.0);
        if let Some(error) = &panel.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return;
        }
        let (Some(source), Some(small)) = (panel.source.clone(), panel.small.as_ref()) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Opening the image...");
            });
            return;
        };

        egui::Panel::bottom("hdri-controls").show(ui, |ui| {
            ui.add_space(6.0);
            egui::Grid::new("hdri-grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Sun strength");
                ui.add(egui::Slider::new(&mut panel.params.sun_strength, 0.0..=12.0).fixed_decimals(1)).on_hover_text(
                    "The light the sun gives compared with the rest of the sky. Clear day 4 to 8, haze 1 to 3, overcast 0.",
                );
                ui.end_row();
                ui.label("Sun size");
                ui.add(egui::Slider::new(&mut panel.params.sun_size_deg, 0.5..=10.0).fixed_decimals(1).suffix(" deg"))
                    .on_hover_text("Smaller sun, sharper shadows.");
                ui.end_row();
                ui.label("Sun position");
                ui.horizontal(|ui| {
                    match panel.sun {
                        Some(_) => ui.label("Found. Click on the image if it is somewhere else."),
                        None => ui.label("No sun. Click on the image where it is."),
                    };
                    if panel.params.sun != Sun::Auto && ui.button("Find it again").clicked() {
                        panel.params.sun = Sun::Auto;
                    }
                    if panel.params.sun != Sun::None && ui.button("No sun").clicked() {
                        panel.params.sun = Sun::None;
                    }
                });
                ui.end_row();
                ui.label("Look with exposure");
                ui.horizontal(|ui| {
                    ui.add_enabled(!panel.only_sun, egui::Slider::new(&mut panel.ev, -10.0..=4.0).fixed_decimals(1).suffix(" EV"));
                    ui.checkbox(&mut panel.only_sun, "See the sun (-8 EV)")
                        .on_hover_text("Only changes the preview. At -8 EV only the sun should be left shining.");
                });
                ui.end_row();
                ui.label("Resolution");
                ui.horizontal(|ui| {
                    for (label, width) in HDRI_WIDTHS {
                        let fits = width.map_or(true, |w| w <= source.rgb.width());
                        ui.add_enabled_ui(fits, |ui| ui.selectable_value(&mut panel.width, width, label));
                    }
                    ui.separator();
                    ui.selectable_value(&mut panel.format, HdrFormat::Exr, "EXR");
                    ui.selectable_value(&mut panel.format, HdrFormat::Hdr, "HDR");
                    ui.label(RichText::new(format!("source {}", size_text(source.rgb.dimensions()))).weak());
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if panel.saving {
                    ui.spinner();
                    ui.label("Writing the HDRI...");
                } else if ui.button(RichText::new("Create HDRI").strong()).clicked() {
                    panel.saving = true;
                    panel.result = None;
                    settings.sun_strength = panel.params.sun_strength;
                    settings.sun_size_deg = panel.params.sun_size_deg;
                    settings.hdri_width = panel.width;
                    settings.hdri_format = panel.format;
                    let width = panel.width.filter(|w| *w < source.rgb.width());
                    let (params, format, token) = (panel.params, panel.format, panel.token);
                    // The sun that is on screen is the one that is written.
                    let params = hdri::Params { sun: panel.sun.map_or(Sun::None, |(x, y)| Sun::At(x, y)), ..params };
                    let out = hdri::output_path(&panel.path, format);
                    let (source, reporter) = (source.clone(), reporter.clone());
                    std::thread::spawn(move || {
                        let result = hdri::write(&source, &params, width, format, &out)
                            .map(|size| (out, size))
                            .map_err(|e| e.to_string());
                        reporter.send(Msg::HdriSaved(token, result));
                    });
                }
                match &panel.result {
                    Some(Ok((path, size))) => {
                        ui.label(format!("Saved {} ({})", file_name(path), size_text(*size)));
                        if ui.button("Show in folder").clicked() {
                            actions.push(Action::Reveal(path.clone()));
                        }
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    None => {}
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let ev = if panel.only_sun { -8.0 } else { panel.ev };
            if panel.drawn != Some((panel.params, ev)) {
                let model = hdri::Model::new(small, &panel.params, small.width());
                panel.sun = model.sun;
                let image = model.preview(small, ev);
                let colour = egui::ColorImage::from_rgb([image.width() as usize, image.height() as usize], image.as_raw());
                match &mut panel.texture {
                    Some(texture) => texture.set(colour, egui::TextureOptions::LINEAR),
                    None => panel.texture = Some(ui.ctx().load_texture("hdri", colour, egui::TextureOptions::LINEAR)),
                }
                panel.drawn = Some((panel.params, ev));
            }
            let space = ui.available_rect_before_wrap();
            let width = space.width().min(space.height() * 2.0);
            let rect = Rect::from_center_size(space.center(), vec2(width, width / 2.0));
            let response = ui.allocate_rect(space, Sense::click());
            let painter = ui.painter_at(space);
            if let Some(texture) = &panel.texture {
                painter.image(texture.id(), rect, whole(), Color32::WHITE);
            }
            if let Some((x, y)) = panel.sun {
                let at = pos2(rect.min.x + x * rect.width(), rect.min.y + y * rect.height());
                painter.circle_stroke(at, 12.0, Stroke::new(2.0, Color32::BLACK));
                painter.circle_stroke(at, 12.0, Stroke::new(1.0, ACCENT));
            }
            if response.clicked() {
                if let Some(pos) = response.interact_pointer_pos().filter(|p| rect.contains(*p)) {
                    panel.params.sun =
                        Sun::At((pos.x - rect.min.x) / rect.width(), (pos.y - rect.min.y) / rect.height());
                }
            }
        });
    }
}

/// The crop frame inside an area: as large as fits with a margin, centred.
fn crop_frame(area: Vec2, aspect: f64) -> Rect {
    let (max_w, max_h) = (area.x * 0.86, area.y * 0.86);
    let (w, h) = if max_w / max_h > aspect as f32 { (max_h * aspect as f32, max_h) } else { (max_w, max_w / aspect as f32) };
    Rect::from_center_size(pos2(area.x / 2.0, area.y / 2.0), vec2(w, h))
}

fn describe(info: &PanoInfo) -> String {
    let mut text = String::from(match info.kind {
        Kind::Official => "Street view",
        Kind::User => "360 photo",
    });
    if info.date.is_some() {
        text.push_str(&format!("  ·  {}", info.date_text()));
    }
    text.push_str(&format!("  ·  {}", size_text(info.full_size())));
    if let Some(author) = &info.author {
        text.push_str(&format!("  ·  {author}"));
    }
    text
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Opens the folder of a file with the file selected.
fn reveal(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
}

fn saved_line(ui: &mut egui::Ui, outcome: &Option<Result<Saved, String>>, actions: &mut Vec<Action>, offer_hdri: bool) {
    match outcome {
        Some(Ok(saved)) => {
            ui.label(format!("Saved {} ({})", file_name(&saved.path), size_text(saved.size)));
            if saved.missing_tiles > 0 {
                ui.colored_label(ui.visuals().warn_fg_color, format!("{} tiles were missing", saved.missing_tiles))
                    .on_hover_text("The service did not have them; those parts of the image are black.");
            }
            if ui.small_button("Show in folder").clicked() {
                actions.push(Action::Reveal(saved.path.clone()));
            }
            if offer_hdri && saved.size.0 == saved.size.1 * 2 && ui.small_button("Create HDRI").clicked() {
                actions.push(Action::OpenHdri(saved.path.clone()));
            }
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("Failed: {e}"));
        }
        None => {}
    }
}

fn card_ui(ui: &mut egui::Ui, card: &Card, actions: &mut Vec<Action>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            // The picture.
            let (rect, _) = ui.allocate_exact_size(THUMB, Sense::hover());
            ui.painter().rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
            match &card.thumb {
                Some(texture) => {
                    let size = texture.size_vec2();
                    let fit = (rect.width() / size.x).min(rect.height() / size.y);
                    let image = Rect::from_center_size(rect.center(), size * fit);
                    ui.painter().image(texture.id(), image, whole(), Color32::WHITE);
                }
                None if card.error.is_none() => {
                    ui.put(Rect::from_center_size(rect.center(), vec2(20.0, 20.0)), egui::Spinner::new());
                }
                None => {}
            }

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    match &card.found {
                        Some(Found::Pano(info)) => {
                            ui.label(RichText::new(describe(info)).strong());
                            if info.is_partial() {
                                ui.label(RichText::new("Partial").color(ui.visuals().warn_fg_color))
                                    .on_hover_text("This 360 photo does not cover the whole sphere.");
                            }
                        }
                        Some(Found::Photo(photo)) => {
                            let size = photo.size.map(|s| format!("  ·  {}", size_text(s))).unwrap_or_default();
                            ui.label(RichText::new(format!("Photo{size}")).strong());
                        }
                        None if card.error.is_none() => {
                            ui.label("Looking up the link...");
                        }
                        None => {}
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            actions.push(Action::Remove(card.id));
                        }
                    });
                });
                ui.add(egui::Label::new(RichText::new(&card.link).weak().small()).truncate());
                if let Some(error) = &card.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }

                // Other dates of the same place.
                if let Some(info) = card.pano().filter(|i| !i.history.is_empty()) {
                    egui::ScrollArea::horizontal().id_salt(("history", card.id)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for capture in &info.history {
                                ui.vertical(|ui| {
                                    let (rect, response) = ui.allocate_exact_size(HISTORY_THUMB, Sense::click());
                                    ui.painter().rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
                                    if let Some(texture) = card.history_thumbs.get(&capture.id) {
                                        ui.painter().image(texture.id(), rect, whole(), Color32::WHITE);
                                    }
                                    if response.hovered() {
                                        ui.painter().rect_stroke(rect, 3.0, Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
                                    }
                                    let mut label = format!("{:04}-{:02}", capture.year, capture.month);
                                    if capture.kind == Kind::User {
                                        label.push_str(" user");
                                    }
                                    ui.label(RichText::new(label).small());
                                    if response.on_hover_text("Switch to this date").clicked() {
                                        actions.push(Action::SwitchDate(card.id, capture.clone()));
                                    }
                                });
                            }
                        });
                    });
                }

                if card.found.is_some() {
                    ui.horizontal(|ui| {
                        let is_pano = card.pano().is_some();
                        match &card.job {
                            Some(job) => {
                                ui.add(egui::ProgressBar::new(job.step.fraction()).desired_width(260.0).text(job.step.label()));
                                if ui.button("Cancel").clicked() {
                                    actions.push(Action::Cancel(card.id));
                                }
                            }
                            None if card.switching => {
                                ui.spinner();
                                ui.label("Switching date...");
                            }
                            None => {
                                let label = if card.downloaded.is_some() { "Download again" } else { "Download" };
                                if ui.button(RichText::new(label).strong()).clicked() {
                                    actions.push(Action::Download(card.id));
                                }
                                if is_pano {
                                    if ui.button("View in 360").clicked() {
                                        actions.push(Action::OpenViewer(card.id, false));
                                    }
                                    if ui.button("Extract crop").clicked() {
                                        actions.push(Action::OpenViewer(card.id, true));
                                    }
                                }
                            }
                        }
                    });
                    ui.horizontal_wrapped(|ui| saved_line(ui, &card.downloaded, actions, true));
                    if card.cropped.is_some() {
                        ui.horizontal_wrapped(|ui| saved_line(ui, &card.cropped, actions, false));
                    }
                }
            });
        });
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(ctx);
        // Remember where the window is.
        let (outer, inner, maximized) =
            ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect, i.viewport().maximized.unwrap_or(false)));
        if let (Some(outer), Some(inner), false) = (outer, inner, maximized) {
            if inner.width() > 200.0 && inner.height() > 200.0 {
                self.settings.window = Some([outer.min.x, outer.min.y, inner.width(), inner.height()]);
                self.settings.window_scale = ctx.pixels_per_point();
            }
        }
        // Ask before closing with work in progress.
        if ctx.input(|i| i.viewport().close_requested()) && self.busy() && !self.close_anyway {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Ctrl+V anywhere adds links, unless the text is going into a text box.
        if self.screen == Screen::List && !ctx.text_edit_focused() {
            let pasted: Vec<String> = ctx.input(|i| {
                i.events.iter().filter_map(|e| if let egui::Event::Paste(text) = e { Some(text.clone()) } else { None }).collect()
            });
            for text in pasted {
                self.add_links(&ctx, &text);
            }
        }

        let mut actions = Vec::new();
        match self.screen {
            Screen::List => {
                egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
                egui::CentralPanel::default().show(ui, |ui| self.list(ui, &mut actions));
            }
            Screen::Viewer => {
                egui::CentralPanel::default().show(ui, |ui| self.viewer_screen(ui, &mut actions));
            }
            Screen::Hdri => {
                egui::CentralPanel::default().show(ui, |ui| self.hdri_screen(ui, &mut actions));
            }
        }
        for action in actions {
            self.apply(&ctx, action);
        }

        if self.confirm_close {
            egui::Modal::new(egui::Id::unique("confirm-close")).show(&ctx, |ui| {
                ui.label(RichText::new("Work is still in progress").strong());
                ui.label("Closing now stops the downloads that are running.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Keep working").clicked() {
                        self.confirm_close = false;
                    }
                    if ui.button("Close anyway").clicked() {
                        self.close_anyway = true;
                        self.confirm_close = false;
                        for card in &self.cards {
                            if let Some(job) = &card.job {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }

        if self.settings != self.saved && !ctx.input(|i| i.pointer.any_down()) {
            // The window's place changes all the time while it is dragged: only the options are
            // written straight away, the rest waits for the exit.
            let mut comparable = self.settings.clone();
            comparable.window = self.saved.window;
            comparable.window_scale = self.saved.window_scale;
            if comparable != self.saved {
                self.settings.save();
                self.saved = self.settings.clone();
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.settings.save();
    }
}
