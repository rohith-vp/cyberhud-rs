//! Layout and drawing for the whole HUD.
//!
//! Two layers are drawn:
//!  - a background (stars, mountains, vignette), refreshed about 15 times a second;
//!  - an overlay (clock, cards, radar, map, spectrum), redrawn about 30 times a second.

use crate::audio::BANDS;
use crate::canvas::{blur, hex, text_mask, Canvas, Face, Rgb};
use crate::geo;
use crate::music::Media;
use crate::telemetry::Stats;
use chrono::{Datelike, Local, Timelike, Utc};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CW: f32 = 9.0;
const CH: f32 = 15.0;
const TERRAIN_FONT: f32 = 13.0;
const CARD_BG: f32 = 0.86;
const COL_W: f32 = 340.0;
const MARGIN: f32 = 24.0;
const GAP: f32 = 14.0;
const HIST: usize = 120;
const UI_SCALE: f32 = 0.86; // every UI text size is scaled by this (the clock is not)          // 30 seconds at 4 samples per second
const HIST_EVERY: Duration = Duration::from_millis(250);
const BG_EVERY: Duration = Duration::from_millis(100);
const GLOW_LAYERS: [(f32, f32); 3] = [(8.0, 0.45), (18.0, 0.20), (32.5, 0.12)]; // CSS blur 16/36/65px

#[derive(Clone)]
pub struct Shared {
    pub stats: Arc<Mutex<Stats>>,
    pub media: Arc<Mutex<Media>>,
    pub bands: Arc<Mutex<Vec<f32>>>,
    pub latency: Arc<Mutex<Option<f64>>>,
}

struct Glow {
    key: String,
    mask: Vec<f32>,
    mw: usize,
    mh: usize,
    pad: usize,
}

/// Position, size and title of one card. Used both for the cached background
/// and for the per-frame content, so the two always line up.
struct CardSpec {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    title: &'static str,
    accent: Rgb,
}

const CPU_H: f32 = 232.0;
const RADAR_H: f32 = 180.0;
const POWER_H: f32 = 150.0;
const LOCATION_H: f32 = 262.0;
const MEMORY_H: f32 = 150.0;
const NETWORK_H: f32 = 150.0;
const AUDIO_H: f32 = 190.0;

fn card_specs(w: f32) -> Vec<CardSpec> {
    let lx = MARGIN;
    let rx = w - MARGIN - COL_W;
    let (ly0, ly1, ly2) = (96.0, 96.0 + CPU_H + GAP, 96.0 + CPU_H + GAP + RADAR_H + GAP);
    let (ry0, ry1, ry2, ry3) = (
        96.0,
        96.0 + LOCATION_H + GAP,
        96.0 + LOCATION_H + GAP + MEMORY_H + GAP,
        96.0 + LOCATION_H + GAP + MEMORY_H + GAP + NETWORK_H + GAP,
    );
    let spec = |x, y, h, title, accent| CardSpec { x, y, w: COL_W, h, title, accent };
    vec![
        spec(lx, ly0, CPU_H, "CPU COMPUTE ARRAY", Palette::green()),
        spec(lx, ly1, RADAR_H, "TACTICAL RADAR", Palette::teal()),
        spec(lx, ly2, POWER_H, "POWER & SYSTEM", Palette::mauve()),
        spec(rx, ry0, LOCATION_H, "LOCATION", Palette::sapphire()),
        spec(rx, ry1, MEMORY_H, "MEMORY & STORAGE", Palette::lavender()),
        spec(rx, ry2, NETWORK_H, "NETWORK TRAFFIC", Palette::teal()),
        spec(rx, ry3, AUDIO_H, "AUDIO SPECTRUM", Palette::mauve()),
    ]
}

struct Vignette {
    w: usize,
    h: usize,
    alpha: Vec<u8>,
}

struct Geo {
    lat: f32,
    lon: f32,
    city: String,
    tz_label: String,
}

pub struct Scene {
    pub canvas: Canvas,
    shared: Shared,
    start: Instant,
    bg_latest: Arc<Mutex<Option<Arc<Vec<u32>>>>>,
    bg_size: Arc<Mutex<(usize, usize)>>,
    glow: Option<Glow>,
    vignette: Option<Vignette>,
    cpu_hist: VecDeque<f32>,
    rx_hist: VecDeque<f32>,
    last_hist: Instant,
    shown_bands: Vec<f32>,
    geo: Geo,
    tz: String,
}

struct Palette;
impl Palette {
    fn base() -> Rgb { hex("#11111b") }
    fn card() -> Rgb { hex("#181825") }
    fn surface0() -> Rgb { hex("#313244") }
    fn surface1() -> Rgb { hex("#585b70") }
    fn text() -> Rgb { hex("#f4f6ff") }
    fn subtext() -> Rgb { hex("#cdd3ea") }
    fn lavender() -> Rgb { hex("#c4ccff") }
    fn mauve() -> Rgb { hex("#d9b8ff") }
    fn blue() -> Rgb { hex("#9ccbff") }
    fn sapphire() -> Rgb { hex("#83d6f5") }
    fn sky() -> Rgb { hex("#a2eaf6") }
    fn teal() -> Rgb { hex("#a6f0e2") }
    fn green() -> Rgb { hex("#b4f0ab") }
    fn peach() -> Rgb { hex("#ffc39a") }
    fn red() -> Rgb { hex("#ff9db7") }
    fn rosewater() -> Rgb { hex("#ffeae6") }
}

impl Scene {
    pub fn new(w: usize, h: usize, shared: Shared) -> Self {
        let start = Instant::now();
        let bg_latest: Arc<Mutex<Option<Arc<Vec<u32>>>>> = Arc::new(Mutex::new(None));
        let bg_size = Arc::new(Mutex::new((w, h)));

        // The background (terrain, glow, card frames) is rebuilt on its own thread,
        // so a slow rebuild can't stall the overlay or the visualizer.
        let mut worker = Scene::bare(w, h, shared.clone(), start);
        let latest = Arc::clone(&bg_latest);
        let size = Arc::clone(&bg_size);
        thread::spawn(move || loop {
            let began = Instant::now();
            let (bw, bh) = *size.lock().unwrap();
            if bw > 0 && bh > 0 {
                let t = worker.start.elapsed().as_secs_f32();
                let px = Arc::new(worker.render_bg(bw, bh, t));
                crate::stats_debug::bump(&crate::stats_debug::BG_REBUILDS);
                *latest.lock().unwrap() = Some(px);
            }
            let spent = began.elapsed();
            if spent < BG_EVERY {
                thread::sleep(BG_EVERY - spent);
            }
        });

        let mut scene = Scene::bare(w, h, shared, start);
        scene.bg_latest = bg_latest;
        scene.bg_size = bg_size;
        scene
    }

    /// A scene without a background worker. Used by the worker itself.
    fn bare(w: usize, h: usize, shared: Shared, start: Instant) -> Self {
        let now = Instant::now();
        let tz = crate::telemetry::timezone_name();
        let offset_secs = Local::now().offset().local_minus_utc();
        let geo = resolve_geo(&tz, offset_secs);
        Scene {
            canvas: Canvas::new(w, h),
            shared,
            start,
            bg_latest: Arc::new(Mutex::new(None)),
            bg_size: Arc::new(Mutex::new((w, h))),
            glow: None,
            vignette: None,
            cpu_hist: VecDeque::from(vec![0.0; HIST]),
            rx_hist: VecDeque::from(vec![0.0; HIST]),
            last_hist: now,
            shown_bands: vec![0.0; BANDS],
            geo,
            tz,
        }
    }

    /// Renders the background into a fresh pixel buffer.
    fn render_bg(&mut self, w: usize, h: usize, t: f32) -> Vec<u32> {
        self.canvas.resize(w, h);
        self.canvas.clear(Palette::base());
        self.terrain(t);
        self.clock_vignette(w as f32, h as f32);
        self.clock_glow(w as f32, h as f32);
        for c in card_specs(w as f32) {
            self.card_chrome(&c);
        }
        self.canvas.px.clone()
    }

    pub fn render(&mut self, w: usize, h: usize) {
        self.canvas.resize(w, h);
        *self.bg_size.lock().unwrap() = (w, h);
        let t = self.start.elapsed().as_secs_f32();

        // Copy the newest finished background, or a plain fill until one is ready.
        let latest = self.bg_latest.lock().unwrap().clone();
        match latest {
            Some(px) if px.len() == w * h => self.canvas.px.copy_from_slice(&px),
            _ => self.canvas.clear(Palette::base()),
        }

        let now = Instant::now();
        if now.duration_since(self.last_hist) >= HIST_EVERY {
            self.last_hist = now;
            let stats = self.shared.stats.lock().unwrap().clone();
            push(&mut self.cpu_hist, stats.cpu_total);
            push(&mut self.rx_hist, stats.net_rx_mbps as f32);
        }

        let stats = self.shared.stats.lock().unwrap().clone();
        let media = self.shared.media.lock().unwrap().clone();
        let target = self.shared.bands.lock().unwrap().clone();
        for (s, tgt) in self.shown_bands.iter_mut().zip(target.iter()) {
            *s += (tgt - *s) * 0.45;
        }
        let latency = *self.shared.latency.lock().unwrap();

        let (wf, hf) = (w as f32, h as f32);
        self.clock(wf, hf);
        self.left_column(&stats, t);
        self.right_column(&stats, &media, latency, wf, t);
    }

    // -------------------------------------------------------------------
    // Background
    // -------------------------------------------------------------------

    fn terrain(&mut self, t: f32) {
        let (w, h) = (self.canvas.w as f32, self.canvas.h as f32);
        let cols = (w / CW).ceil() + 2.0;
        let rows = (h / CH).ceil() + 2.0;
        self.canvas.set_face(Face::Mono);
        let mut r = 0.0;
        while r < rows {
            let mut c = 0.0;
            while c < cols {
                if let Some((ch, col, a)) = star(c, r, t, rows) {
                    self.canvas.char_at(ch, c * CW, r * CH, TERRAIN_FONT, col, a);
                }
                if let Some((ch, col)) = far_glyph(c, r, t, cols, rows) {
                    self.canvas.char_at(ch, c * CW, r * CH, TERRAIN_FONT, col, 0.65);
                }
                if let Some((ch, col)) = mid_glyph(c, r, t, cols, rows) {
                    self.canvas.char_at(ch, c * CW, r * CH, TERRAIN_FONT, col, 0.85);
                }
                if let Some((ch, col)) = near_glyph(c, r, t, cols, rows) {
                    self.canvas.char_at(ch, c * CW, r * CH, TERRAIN_FONT, col, 0.95);
                }
                c += 1.0;
            }
            r += 1.0;
        }
    }

    /// Soft dark panel behind the clock so the digits stand out.
    /// The per-pixel alpha is computed once per screen size and reused.
    fn clock_vignette(&mut self, w: f32, h: f32) {
        let (wi, hi) = (w as usize, h as usize);
        if self.vignette.as_ref().map_or(true, |v| v.w != wi || v.h != hi) {
            let (cx, cy) = clock_center(w, h);
            let (x0, y0, rw, rh, feather) = (cx - 360.0, cy - 150.0, 720.0f32, 300.0f32, 120.0f32);
            let mut alpha = vec![0u8; wi * hi];
            let (ax0, ay0) = ((x0 - feather).max(0.0) as usize, (y0 - feather).max(0.0) as usize);
            let (ax1, ay1) = (((x0 + rw + feather) as usize).min(wi), ((y0 + rh + feather) as usize).min(hi));
            for py in ay0..ay1 {
                for px in ax0..ax1 {
                    let fx = px as f32 + 0.5;
                    let fy = py as f32 + 0.5;
                    let d = (fx - x0).min(x0 + rw - fx).min(fy - y0).min(y0 + rh - fy);
                    let t = ((d + feather) / feather).clamp(0.0, 1.0);
                    let v = t * t * (3.0 - 2.0 * t) * 0.62;
                    alpha[py * wi + px] = (v * 255.0) as u8;
                }
            }
            self.vignette = Some(Vignette { w: wi, h: hi, alpha });
        }
        let base = Palette::base();
        let v = self.vignette.as_ref().expect("vignette built above");
        for (i, &a) in v.alpha.iter().enumerate() {
            if a > 0 {
                let (x, y) = ((i % v.w) as i32, (i / v.w) as i32);
                self.canvas.blend(x, y, base, a as f32 / 255.0);
            }
        }
    }

    // -------------------------------------------------------------------
    // Clock
    // -------------------------------------------------------------------

    fn clock(&mut self, w: f32, h: f32) {
        let now = Local::now();
        let utc = Utc::now();
        let size = (w * 0.046).min(89.6);
        let time = format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second());
        let spacing = size * 0.05;

        self.canvas.set_face(Face::Hud);
        self.canvas.spacing = spacing;
        let tw = self.canvas.text_width(&time, size);
        let (cx, cy) = clock_center(w, h);
        let x0 = cx - tw / 2.0;
        let y0 = cy - size * 0.5;

        // Digits in green, colons in mauve.
        let mut x = x0;
        for ch in time.chars() {
            let col = if ch == ':' { Palette::mauve() } else { Palette::green() };
            self.canvas.char_at(ch, x, y0, size, col, 1.0);
            x += self.canvas.advance(ch, size) + spacing;
        }
        self.canvas.spacing = 0.0;

        // Date and UTC lines use the mono face.
        self.canvas.set_face(Face::Mono);
        let days = ["MONDAY", "TUESDAY", "WEDNESDAY", "THURSDAY", "FRIDAY", "SATURDAY", "SUNDAY"];
        let months = ["JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY", "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER"];
        let day = days[now.weekday().num_days_from_monday() as usize];
        let month = months[now.month0() as usize];
        let date = format!("{} · {} {}, {}", day, month, now.day(), now.year());
        let dw = self.ui_width(&date, 19.0);
        self.ui_text(&date, cx - dw / 2.0, y0 + size + 12.0, 19.0, Palette::lavender(), 1.0);

        let line = format!("UTC {:02}:{:02}:{:02}Z  ·  DOY {}/365", utc.hour(), utc.minute(), utc.second(), now.ordinal());
        let lw = self.ui_width(&line, 13.0);
        self.ui_text(&line, cx - lw / 2.0, y0 + size + 42.0, 13.0, Palette::subtext(), 1.0);
    }

    /// Green glow behind the clock digits. Called when the background is
    /// rebuilt, so it's only recalculated when the displayed time changes.
    fn clock_glow(&mut self, w: f32, h: f32) {
        let now = Local::now();
        let size = (w * 0.046).min(89.6);
        let time = format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second());
        let spacing = size * 0.05;
        self.canvas.set_face(Face::Hud);
        self.canvas.spacing = spacing;
        let tw = self.canvas.text_width(&time, size);
        let (cx, cy) = clock_center(w, h);
        let x0 = cx - tw / 2.0;
        let y0 = cy - size * 0.5;

        let key = format!("{}|{}", time, size as i32);
        if self.glow.as_ref().map_or(true, |g| g.key != key) {
            self.glow = Some(build_glow(&mut self.canvas, &time, size, spacing, &key));
        }
        self.canvas.spacing = 0.0;
        self.canvas.set_face(Face::Mono);
        if let Some(g) = &self.glow {
            let (ox, oy) = (x0.round() as i32 - g.pad as i32, y0.round() as i32 - g.pad as i32);
            for my in 0..g.mh {
                for mx in 0..g.mw {
                    let m = g.mask[my * g.mw + mx];
                    if m > 0.002 {
                        self.canvas.blend(ox + mx as i32, oy + my as i32, Palette::green(), m);
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Cards
    // -------------------------------------------------------------------

    /// Card chrome (background, frame, title, divider). Drawn into the cached
    /// background, so it isn't repainted every frame.
    fn card_chrome(&mut self, c: &CardSpec) {
        let (x, y, w, h) = (c.x, c.y, c.w, c.h);
        self.canvas.rect(x, y, w, h, Palette::card(), CARD_BG);
        self.canvas.frame(x, y, w, h, Palette::surface1(), 0.9);
        self.canvas.rect(x + 10.0, y + 12.0, 3.0, 14.0, c.accent, 1.0);
        self.canvas.set_face(Face::Mono);
        self.ui_text(c.title, x + 20.0, y + 10.0, 14.0, Palette::text(), 1.0);
        self.canvas.rect(x + 12.0, y + 34.0, w - 24.0, 1.0, Palette::surface0(), 1.0);
    }

    /// Dynamic part of a card header: the meta text on the right.
    fn card(&mut self, x: f32, y: f32, w: f32, _h: f32, _title: &str, meta: &str, accent: Rgb) {
        if !meta.is_empty() {
            self.canvas.set_face(Face::Mono);
            let mw = self.ui_width(meta, 13.0);
            self.ui_text(meta, x + w - 14.0 - mw, y + 11.0, 13.0, accent, 1.0);
        }
    }

    /// Draws UI text at the scaled size (86% of the nominal size).
    fn ui_text(&mut self, s: &str, x: f32, y: f32, size: f32, c: Rgb, a: f32) -> f32 {
        self.canvas.text(s, x, y, size * UI_SCALE, c, a)
    }

    fn ui_width(&self, s: &str, size: f32) -> f32 {
        self.canvas.text_width(s, size * UI_SCALE)
    }

    fn bar(&mut self, x: f32, y: f32, w: f32, frac: f32, color: Rgb) {
        self.canvas.rect(x, y, w, 5.0, Palette::surface0(), 1.0);
        self.canvas.rect(x, y, w * frac.clamp(0.0, 1.0), 5.0, color, 1.0);
    }

    fn kv(&mut self, x: f32, y: f32, w: f32, key: &str, value: &str, color: Rgb) {
        self.canvas.set_face(Face::Mono);
        self.ui_text(key, x, y, 13.0, Palette::subtext(), 1.0);
        let vw = self.ui_width(value, 13.0);
        self.ui_text(value, x + w - vw, y, 13.0, color, 1.0);
    }

    fn left_column(&mut self, s: &Stats, t: f32) {
        let x = MARGIN;
        let mut y = 96.0;

        // CPU
        let ch = 232.0;
        let meta = s.cpu_ghz.map(|g| format!("{:.2} GHz", g)).unwrap_or_else(|| "N/A".into());
        self.card(x, y, COL_W, ch, "CPU COMPUTE ARRAY", &meta, Palette::green());
        self.canvas.set_face(Face::Mono);
        let total = format!("{:.0}", s.cpu_total);
        self.ui_text(&total, x + 20.0, y + 46.0, 36.0, Palette::green(), 1.0);
        let tw = self.ui_width(&total, 36.0);
        self.ui_text("%", x + 22.0 + tw, y + 58.0, 15.0, Palette::subtext(), 1.0);
        let temp = s.cpu_temp.map(|v| format!("{:.1}°C", v)).unwrap_or_else(|| "N/A".into());
        self.kv(x + 170.0, y + 50.0, 150.0, "DIE TEMP", &temp, Palette::peach());
        self.core_bars(x + 20.0, y + 100.0, COL_W - 40.0, &s.cores);
        let hist = self.cpu_hist.clone();
        self.sparkline(x + 20.0, y + 154.0, COL_W - 40.0, 40.0, &hist, Palette::green());
        self.ui_text("30s LOAD ACTIVITY", x + 20.0, y + ch - 24.0, 11.0, Palette::subtext(), 1.0);
        y += ch + GAP;

        // Tactical radar
        let rh = 180.0;
        let sweep = (t * 60.0) % 360.0;
        self.card(x, y, COL_W, rh, "TACTICAL RADAR", &format!("SWEEP: {:03.0}°", sweep), Palette::teal());
        self.radar(x + 84.0, y + 110.0, 54.0, sweep);
        let gw = s.gateway.clone().unwrap_or_else(|| "N/A".into());
        self.kv(x + 160.0, y + 62.0, 160.0, "GATEWAY", &gw, Palette::text());
        let lat = self.shared.latency.lock().unwrap().map(|v| format!("{:.0}ms", v)).unwrap_or_else(|| "N/A".into());
        self.kv(x + 160.0, y + 84.0, 160.0, "LATENCY", &lat, Palette::text());
        y += rh + GAP;

        // Power and system
        let ph = 150.0;
        let (state, percent_text, frac, color) = match &s.battery {
            Some(b) => (
                if b.charging { "CHARGING" } else { "ON BATTERY" }.to_string(),
                format!("{}%", b.percent),
                b.percent as f32 / 100.0,
                if b.charging { Palette::teal() } else { Palette::peach() },
            ),
            None => ("NO BATTERY".into(), "N/A".into(), 0.0, Palette::teal()),
        };
        let ac = match s.ac_online {
            Some(true) => "AC POWER",
            Some(false) => "ON BATTERY",
            None => "N/A",
        };
        self.card(x, y, COL_W, ph, "POWER & SYSTEM", ac, Palette::mauve());
        self.canvas.set_face(Face::Mono);
        self.ui_text("BATTERY", x + 20.0, y + 46.0, 12.0, Palette::subtext(), 1.0);
        self.ui_text(&percent_text, x + 20.0, y + 62.0, 24.0, color, 1.0);
        let sw = self.ui_width(&state, 12.0);
        self.ui_text(&state, x + COL_W - 20.0 - sw, y + 70.0, 12.0, color, 1.0);
        self.bar(x + 20.0, y + 92.0, COL_W - 40.0, frac, color);
        let up = format_uptime(s.uptime_secs);
        let vp = format!("{}×{}", self.canvas.w, self.canvas.h);
        self.kv(x + 20.0, y + 108.0, COL_W - 40.0, "VIEWPORT", &vp, Palette::text());
        self.kv(x + 20.0, y + 126.0, COL_W - 40.0, "SYSTEM UPTIME", &up, Palette::text());
    }

    fn right_column(&mut self, s: &Stats, m: &Media, latency: Option<f64>, w: f32, t: f32) {
        let x = w - MARGIN - COL_W;
        let mut y = 96.0;

        // Location with world map
        let lh = 262.0;
        let geo_meta = self.geo.tz_label.clone();
        self.card(x, y, COL_W, lh, "LOCATION", &geo_meta, Palette::sapphire());
        let (mx, my, mw, mh) = (x + 20.0, y + 44.0, COL_W - 40.0, 125.0);
        self.world_map(mx, my, mw, mh, t);
        self.canvas.set_face(Face::Mono);
        let city = self.geo.city.clone();
        self.ui_text(&city, x + 20.0, y + 178.0, 14.0, Palette::peach(), 1.0);
        let coords = format!(
            "{:.1}°{}, {:.1}°{}",
            self.geo.lat.abs(),
            if self.geo.lat >= 0.0 { "N" } else { "S" },
            self.geo.lon.abs(),
            if self.geo.lon >= 0.0 { "E" } else { "W" }
        );
        self.kv(x + 20.0, y + 200.0, COL_W - 40.0, "GEO COORDINATES", &coords, Palette::text());
        let tz = self.tz.clone();
        self.kv(x + 20.0, y + 220.0, COL_W - 40.0, "TIMEZONE", &tz, Palette::text());
        let offset = Local::now().offset().local_minus_utc() / 3600;
        self.kv(x + 20.0, y + 240.0, COL_W - 40.0, "UTC OFFSET", &format!("{:+}h", offset), Palette::text());
        y += lh + GAP;

        // Memory and storage
        let mh_card = 150.0;
        let ram_pct = pct(s.ram_used_gb, s.ram_total_gb);
        self.card(x, y, COL_W, mh_card, "MEMORY & STORAGE", &format!("{:.0}% RAM", ram_pct), Palette::lavender());
        self.canvas.set_face(Face::Mono);
        self.ui_text(&format!("RAM  {:.1} / {:.1} GB", s.ram_used_gb, s.ram_total_gb), x + 20.0, y + 44.0, 13.0, Palette::text(), 1.0);
        self.bar(x + 20.0, y + 64.0, COL_W - 40.0, ram_pct / 100.0, Palette::mauve());
        let disk_pct = pct(s.disk_used_gb, s.disk_total_gb);
        self.ui_text(&format!("DISK /  {:.0} / {:.0} GB", s.disk_used_gb, s.disk_total_gb), x + 20.0, y + 84.0, 13.0, Palette::text(), 1.0);
        self.bar(x + 20.0, y + 104.0, COL_W - 40.0, disk_pct / 100.0, Palette::peach());
        self.kv(x + 20.0, y + 124.0, COL_W - 40.0, "DISK I/O", &format!("R {:.1}  W {:.1} MB/s", s.disk_read_mbps, s.disk_write_mbps), Palette::text());
        y += mh_card + GAP;

        // Network
        let nh = 150.0;
        let online = latency.is_some();
        let (meta, mc) = if online { ("ONLINE", Palette::teal()) } else { ("OFFLINE", Palette::red()) };
        self.card(x, y, COL_W, nh, "NETWORK TRAFFIC", meta, mc);
        let rx = format!("{:.1}", s.net_rx_mbps);
        self.canvas.set_face(Face::Mono);
        self.ui_text(&rx, x + 20.0, y + 44.0, 32.0, Palette::sapphire(), 1.0);
        let rw = self.ui_width(&rx, 32.0);
        self.ui_text("MB/s", x + 26.0 + rw, y + 62.0, 13.0, Palette::subtext(), 1.0);
        self.kv(x + 200.0, y + 44.0, 120.0, "RX", &format!("▼ {:.1} MB/s", s.net_rx_mbps), Palette::sapphire());
        self.kv(x + 200.0, y + 64.0, 120.0, "TX", &format!("▲ {:.1} MB/s", s.net_tx_mbps), Palette::peach());
        let hist = self.rx_hist.clone();
        self.sparkline(x + 20.0, y + 96.0, COL_W - 40.0, 40.0, &hist, Palette::sapphire());
        y += nh + GAP;

        // Audio spectrum
        let ah = 190.0;
        let (status_label, color) = match m.status.as_str() {
            "Playing" => ("PLAYING", Palette::green()),
            "Paused" => ("PAUSED", Palette::peach()),
            "Stopped" => ("STOPPED", Palette::subtext()),
            "NO PLAYER" => ("NO MEDIA", Palette::subtext()),
            _ => ("ERROR", Palette::red()),
        };
        self.card(x, y, COL_W, ah, "AUDIO SPECTRUM", status_label, color);
        self.canvas.set_face(Face::Mono);
        let title = fit(&self.canvas, &m.title, COL_W - 40.0, 14.0 * UI_SCALE);
        self.ui_text(&title, x + 20.0, y + 42.0, 14.0, Palette::text(), 1.0);
        if !m.artist.is_empty() {
            let artist = fit(&self.canvas, &m.artist, COL_W - 40.0, 12.0 * UI_SCALE);
            self.ui_text(&artist, x + 20.0, y + 62.0, 12.0, Palette::subtext(), 1.0);
        }
        let bands = self.shown_bands.clone();
        self.spectrum(x + 20.0, y + 88.0, COL_W - 40.0, 84.0, &bands);
    }

    fn core_bars(&mut self, x: f32, y: f32, w: f32, cores: &[f32]) {
        let n = cores.len().min(8).max(1);
        let gap = 6.0;
        let bw = (w - gap * (n as f32 - 1.0)) / n as f32;
        self.canvas.set_face(Face::Mono);
        for (i, load) in cores.iter().take(n).enumerate() {
            let bx = x + i as f32 * (bw + gap);
            self.canvas.rect(bx, y, bw, 22.0, Palette::surface0(), 1.0);
            let fh = 22.0 * load / 100.0;
            self.canvas.rect(bx, y + 22.0 - fh, bw, fh, Palette::green(), 1.0);
            let label = format!("C{}", i + 1);
            let lw = self.ui_width(&label, 11.0);
            self.ui_text(&label, bx + (bw - lw) / 2.0, y + 26.0, 11.0, Palette::subtext(), 1.0);
        }
    }

    /// Smooth line graph: a Catmull-Rom curve through the samples, drawn as a
    /// 2px anti-aliased stroke with a light fill underneath.
    fn sparkline(&mut self, x: f32, y: f32, w: f32, h: f32, data: &VecDeque<f32>, color: Rgb) {
        let maxv = data.iter().cloned().fold(0.5f32, f32::max);
        let samples: Vec<f32> = data.iter().map(|v| (v / maxv).clamp(0.0, 1.0)).collect();
        let n = samples.len();
        if n < 2 {
            return;
        }
        let step = (w / (n - 1) as f32).max(f32::EPSILON);
        let at = |k: isize| samples[k.clamp(0, n as isize - 1) as usize];
        let mut prev: Option<(f32, f32)> = None;
        let cols = w.ceil() as usize;
        for col in 0..=cols {
            let fx = (col as f32 / step).min((n - 1) as f32);
            let k = fx.floor() as isize;
            let t = fx - k as f32;
            // Catmull-Rom between the four nearest samples.
            let (p0, p1, p2, p3) = (at(k - 1), at(k), at(k + 1), at(k + 2));
            let v = 0.5 * (2.0 * p1 + (-p0 + p2) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t);
            let px = x + col as f32;
            let py = y + h - v.clamp(0.0, 1.0) * h;
            self.canvas.rect(px, py, 1.0, y + h - py, color, 0.10);
            if let Some((ax, ay)) = prev {
                self.canvas.line(ax, ay - 0.5, px, py - 0.5, color, 0.55);
                self.canvas.line(ax, ay + 0.5, px, py + 0.5, color, 0.55);
            }
            prev = Some((px, py));
        }
    }

    fn spectrum(&mut self, x: f32, y: f32, w: f32, h: f32, bands: &[f32]) {
        let gap = 3.0;
        let bw = (w - gap * (BANDS as f32 - 1.0)) / BANDS as f32;
        for i in 0..BANDS {
            let v = bands.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let bh = h * v;
            if bh < 1.0 {
                continue;
            }
            let color = if v > 0.75 { Palette::peach() } else if v > 0.4 { Palette::green() } else { Palette::teal() };
            self.canvas.rect(x + i as f32 * (bw + gap), y + h - bh, bw, bh, color, 1.0);
        }
    }

    /// Radar drawn per pixel: anti-aliased edge, range rings, and a sweep with a
    /// soft leading edge and a fading trail.
    fn radar(&mut self, cx: f32, cy: f32, r: f32, sweep_deg: f32) {
        let x0 = (cx - r - 1.0).floor() as i32;
        let x1 = (cx + r + 1.0).ceil() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let y1 = (cy + r + 1.0).ceil() as i32;
        for py in y0..=y1 {
            for px in x0..=x1 {
                let dx = px as f32 + 0.5 - cx;
                let dy = py as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                // Coverage of the disc at this pixel: 1 inside, fading over one pixel at the edge.
                let cover = (r + 0.5 - d).clamp(0.0, 1.0);
                if cover <= 0.0 {
                    continue;
                }
                self.canvas.blend(px, py, Palette::base(), 0.35 * cover);
                // Range rings, 1px wide with soft edges.
                let mut ring = 0.0f32;
                for frac in [0.33f32, 0.66, 1.0] {
                    ring = ring.max((1.0 - (d - r * frac).abs()).clamp(0.0, 1.0));
                }
                if ring > 0.0 {
                    self.canvas.blend(px, py, Palette::surface1(), ring * 0.8 * cover);
                }
                // Sweep: a soft leading edge, then a trail fading over 70 degrees behind it.
                let theta = dy.atan2(dx).to_degrees().rem_euclid(360.0);
                let behind = (sweep_deg - theta).rem_euclid(360.0);
                let lead = ((2.0 - behind) / 2.0).clamp(0.0, 1.0);
                let k = (1.0 - behind / 70.0).clamp(0.0, 1.0);
                let trail = (0.95 * lead * lead).max(0.55 * k * k);
                if trail > 0.0 {
                    self.canvas.blend(px, py, Palette::teal(), trail * cover);
                }
            }
        }
        // Centre crosshair.
        self.canvas.line(cx - r, cy, cx + r, cy, Palette::surface1(), 0.5);
        self.canvas.line(cx, cy - r, cx, cy + r, Palette::surface1(), 0.5);
    }

    /// Equirectangular world map with the user's location marked.
    fn world_map(&mut self, x: f32, y: f32, w: f32, h: f32, t: f32) {
        let proj_x = |lon: f32| x + (lon + 180.0) / 360.0 * w;
        let proj_y = |lat: f32| y + (90.0 - lat) / 180.0 * h;

        // Grid
        let grid = Palette::surface0();
        self.canvas.line(x, y + h / 2.0, x + w, y + h / 2.0, grid, 0.9);
        self.canvas.line(x + w / 2.0, y, x + w / 2.0, y + h, grid, 0.9);
        for lon in [-120.0f32, -60.0, 60.0, 120.0] {
            let gx = proj_x(lon);
            self.canvas.line(gx, y, gx, y + h, grid, 0.45);
        }
        for lat in [-45.0f32, 45.0] {
            let gy = proj_y(lat);
            self.canvas.line(x, gy, x + w, gy, grid, 0.45);
        }

        // Landmasses
        for poly in geo::CONTINENTS {
            let pts: Vec<(f32, f32)> = poly.iter().map(|&(lon, lat)| (proj_x(lon), proj_y(lat))).collect();
            self.canvas.fill_poly(&pts, Palette::surface1(), 0.48);
            for i in 0..pts.len() {
                let (ax, ay) = pts[i];
                let (bx, by) = pts[(i + 1) % pts.len()];
                self.canvas.line(ax, ay, bx, by, Palette::surface1(), 1.0);
            }
        }

        // Pulsing beacon at the user's location.
        let bx = proj_x(self.geo.lon);
        let by = proj_y(self.geo.lat);
        let pulse = (t * 2.4).sin();
        let radius = 4.0 + (pulse + 1.0) * 6.0;
        let alpha = (1.0 - radius / 16.0).max(0.1);
        ring_aa(&mut self.canvas, bx, by, radius, Palette::peach(), alpha);
        self.canvas.line(bx - 7.0, by, bx + 7.0, by, Palette::peach(), 1.0);
        self.canvas.line(bx, by - 7.0, bx, by + 7.0, Palette::peach(), 1.0);
    }

}

fn ring_aa(canvas: &mut Canvas, cx: f32, cy: f32, r: f32, c: Rgb, a: f32) {
    let steps = (r * 10.0).max(40.0) as usize;
    let mut prev = (cx + r, cy);
    for i in 1..=steps {
        let t = i as f32 / steps as f32 * 2.0 * PI;
        let p = (cx + r * t.cos(), cy + r * t.sin());
        canvas.line(prev.0, prev.1, p.0, p.1, c, a);
        prev = p;
    }
}

fn clock_center(w: f32, h: f32) -> (f32, f32) {
    (w / 2.0, h / 2.0 - 60.0)
}

/// Builds the glow mask for the clock: three blurred copies of the text,
/// matching the web version's text-shadow layers.
fn build_glow(canvas: &mut Canvas, time: &str, size: f32, spacing: f32, key: &str) -> Glow {
    let pad = 110;
    let (mask, mw, mh) = text_mask(canvas, time, size, Face::Hud, spacing, pad);
    let mut total = vec![0.0f32; mw * mh];
    for (sigma, strength) in GLOW_LAYERS {
        let b = blur(&mask, mw, mh, sigma);
        for (t, v) in total.iter_mut().zip(b.iter()) {
            *t += v * strength;
        }
    }
    for t in total.iter_mut() {
        *t = t.min(1.0);
    }
    Glow { key: key.to_string(), mask: total, mw, mh, pad }
}

fn resolve_geo(tz: &str, offset_secs: i32) -> Geo {
    let tz_label = tz.rsplit('/').next().unwrap_or(tz).replace('_', " ");
    if let Some(z) = geo::ZONES.iter().find(|z| z.name == tz) {
        return Geo { lat: z.lat, lon: z.lon, city: z.city.to_string(), tz_label };
    }
    let part = tz.split('/').nth(1).unwrap_or("");
    if !part.is_empty() {
        if let Some(z) = geo::ZONES.iter().find(|z| z.name.contains(part)) {
            return Geo { lat: z.lat, lon: z.lon, city: z.city.to_string(), tz_label };
        }
    }
    // No match: estimate longitude from the UTC offset.
    Geo {
        lat: 32.0,
        lon: offset_secs as f32 / 3600.0 * 15.0,
        city: tz.replace('_', " "),
        tz_label,
    }
}

fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() >= HIST {
        q.pop_front();
    }
    q.push_back(v);
}

fn pct(used: f64, total: f64) -> f32 {
    if total <= 0.0 { 0.0 } else { (used / total * 100.0) as f32 }
}

fn format_uptime(secs: f64) -> String {
    let s = secs as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Truncates text with an ellipsis so it fits in `max_w` pixels.
fn fit(canvas: &Canvas, s: &str, max_w: f32, size: f32) -> String {
    if canvas.text_width(s, size) <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        out.push(ch);
        if canvas.text_width(&format!("{}…", out), size) > max_w {
            out.pop();
            break;
        }
    }
    format!("{}…", out)
}

// ---------------------------------------------------------------------------
// Terrain rules, ported from the web shader.
// ---------------------------------------------------------------------------

fn hash(x: f32, y: f32) -> f32 {
    let v = (x * 127.1 + y * 311.7).sin() * 43758.5453;
    v - v.floor()
}

fn far_h(c: f32, off: f32, cols: f32, rows: f32) -> f32 {
    let nx = (c + off) / cols;
    let n = 0.44 - ((nx * PI * 4.2 + 0.5).sin() * 0.16
        + (nx * PI * 8.6 + 1.2).sin() * 0.08
        + (nx * PI * 16.4).cos() * 0.035
        + (nx * PI * 2.0).sin() * 0.05);
    (n * rows).floor()
}

fn mid_h(c: f32, off: f32, cols: f32, rows: f32) -> f32 {
    let nx = (c + off) / cols;
    let n = 0.58 - ((nx * PI * 5.4 + 2.1).cos() * 0.14
        + (nx * PI * 11.2 + 0.8).sin() * 0.06
        + (nx * PI * 22.0).sin() * 0.025);
    (n * rows).floor()
}

fn near_h(c: f32, off: f32, cols: f32, rows: f32) -> f32 {
    let nx = (c + off) / cols;
    let n = 0.74 - ((nx * PI * 3.2 + 1.4).sin() * 0.09 + (nx * PI * 7.5).cos() * 0.04);
    (n * rows).floor()
}

fn far_glyph(c: f32, r: f32, t: f32, cols: f32, rows: f32) -> Option<(char, Rgb)> {
    let off = t * 1.05;
    let pk = far_h(c, off, cols, rows);
    if r < pk {
        return None;
    }
    let pv = far_h(c - 1.0, off, cols, rows);
    let nx = far_h(c + 1.0, off, cols, rows);
    let d = r - pk;
    if d < 0.5 {
        let ch = if pv > pk && nx > pk { '^' } else if pv > pk { '/' } else if nx > pk { '\\' } else { '_' };
        return Some((ch, Palette::lavender()));
    }
    if d <= 2.5 {
        return Some((if (c + r) % 2.0 < 0.5 { ':' } else { '.' }, Palette::text()));
    }
    if d <= 6.5 {
        let ch = if (c * 3.0 + r) % 4.0 < 0.5 { '#' } else if (c + r) % 3.0 < 0.5 { '/' } else { ':' };
        return Some((ch, Palette::surface1()));
    }
    if (c + r) % 5.0 < 0.5 { Some(('.', Palette::surface0())) } else { None }
}

fn mid_glyph(c: f32, r: f32, t: f32, cols: f32, rows: f32) -> Option<(char, Rgb)> {
    let off = t * 2.55;
    let pk = mid_h(c, off, cols, rows);
    if r < pk {
        return None;
    }
    let pv = mid_h(c - 1.0, off, cols, rows);
    let nx = mid_h(c + 1.0, off, cols, rows);
    let d = r - pk;
    if d < 0.5 {
        if pv > pk && nx > pk {
            return Some(('^', Palette::sapphire()));
        }
        if pv > pk {
            return Some(('/', Palette::sky()));
        }
        if nx > pk {
            return Some(('\\', Palette::blue()));
        }
        return Some(('^', Palette::sapphire()));
    }
    if d < 1.5 {
        return Some((if c % 2.0 < 0.5 { '/' } else { ':' }, Palette::blue()));
    }
    if d <= 4.5 {
        let m = (c * 2.0 + r) % 6.0;
        let ch = if m < 0.5 { '#' } else if m < 1.5 { '%' } else if m < 2.5 { '/' } else { ':' };
        return Some((ch, Palette::blue()));
    }
    if d <= 9.5 {
        let ch = if (c + r) % 3.0 < 0.5 { '|' } else if (c * 2.0 + r) % 4.0 < 0.5 { ':' } else { '.' };
        return Some((ch, Palette::surface1()));
    }
    if (c + r) % 7.0 < 0.5 { Some(('.', Palette::surface0())) } else { None }
}

fn near_glyph(c: f32, r: f32, t: f32, cols: f32, rows: f32) -> Option<(char, Rgb)> {
    let off = t * 4.8;
    let pk = near_h(c, off, cols, rows);
    if r < pk {
        return None;
    }
    let pv = near_h(c - 1.0, off, cols, rows);
    let nx = near_h(c + 1.0, off, cols, rows);
    let d = r - pk;
    if d < 0.5 {
        let ch = if pv > pk { '/' } else if nx > pk { '\\' } else { '_' };
        return Some((ch, Palette::green()));
    }
    if d <= 3.5 {
        let m = (c + r * 2.0) % 5.0;
        let ch = if m < 0.5 { '/' } else if m < 1.5 { '\\' } else if m < 2.5 { '~' } else { '=' };
        return Some((ch, Palette::teal()));
    }
    if (c + r) % 4.0 < 0.5 {
        return Some(('-', Palette::surface1()));
    }
    if (c * 3.0 + r) % 5.0 < 0.5 {
        return Some(('~', Palette::surface1()));
    }
    Some(('.', Palette::surface1()))
}

/// Twinkling star for one cell: (glyph, colour, alpha), or None.
fn star(c: f32, r: f32, t: f32, rows: f32) -> Option<(char, Rgb, f32)> {
    if r >= 0.42 * rows {
        return None;
    }
    let density = 1.6 / (0.42 * rows);
    if hash(c, r) > density {
        return None;
    }
    let speed = 1.5 + 3.5 * hash(c + 11.0, r + 3.0);
    let phase = 2.0 * PI * hash(c + 5.0, r + 17.0);
    let alpha = (((t * speed + phase).sin() + 1.0) * 0.5).clamp(0.12, 0.98);
    let hc = hash(c + 23.0, r + 7.0);
    let col = if hc < 0.2 {
        Palette::lavender()
    } else if hc < 0.4 {
        Palette::rosewater()
    } else if hc < 0.6 {
        Palette::sky()
    } else if hc < 0.8 {
        Palette::text()
    } else {
        Palette::subtext()
    };
    let ch = if alpha > 0.82 {
        '+'
    } else if alpha > 0.5 {
        match (hash(c + 31.0, r + 13.0) * 7.0).floor() as u32 {
            0 | 1 => '·',
            2 => '*',
            3 | 4 => '+',
            5 => '°',
            _ => '.',
        }
    } else {
        '·'
    };
    Some((ch, col, alpha))
}
