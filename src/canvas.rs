//! Software canvas. Pixels are 0xAARRGGBB, the Argb8888 layout on little-endian.

use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::sync::Arc;

static MONO_BYTES: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
static HUD_BYTES: &[u8] = include_bytes!("../assets/Orbitron-Black.ttf");

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    Mono = 0,
    Hud = 1,
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    fonts: [Font; 2],
    pub face: Face,
    pub spacing: f32,
    glyphs: HashMap<(u8, char, u32), Arc<(fontdue::Metrics, Vec<u8>)>>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        let mono = Font::from_bytes(MONO_BYTES, FontSettings::default()).expect("bundled mono font");
        let hud = Font::from_bytes(HUD_BYTES, FontSettings::default()).expect("bundled HUD font");
        Canvas {
            w,
            h,
            px: vec![0xff000000; w * h],
            fonts: [mono, hud],
            face: Face::Mono,
            spacing: 0.0,
            glyphs: HashMap::new(),
        }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if self.w != w || self.h != h {
            self.w = w;
            self.h = h;
            self.px = vec![0xff000000; w * h];
        }
    }

    pub fn clear(&mut self, c: Rgb) {
        let v = pack(c, 255);
        self.px.iter_mut().for_each(|p| *p = v);
    }

    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a <= 0.0 {
            return;
        }
        // Alpha as 0..256 so the blend is integer arithmetic.
        let a = (a.min(1.0) * 256.0) as u32;
        let inv = 256 - a;
        let idx = y as usize * self.w + x as usize;
        let d = self.px[idx];
        let r = (c.0 as u32 * a + ((d >> 16) & 255) * inv) >> 8;
        let g = (c.1 as u32 * a + ((d >> 8) & 255) * inv) >> 8;
        let b = (c.2 as u32 * a + (d & 255) * inv) >> 8;
        self.px[idx] = 0xff00_0000 | (r << 16) | (g << 8) | b;
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb, a: f32) {
        let (x0, y0) = (x.round() as i32, y.round() as i32);
        let (x1, y1) = ((x + w).round() as i32, (y + h).round() as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                self.blend(px, py, c, a);
            }
        }
    }

    pub fn frame(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb, a: f32) {
        self.rect(x, y, w, 1.0, c, a);
        self.rect(x, y + h - 1.0, w, 1.0, c, a);
        self.rect(x, y, 1.0, h, c, a);
        self.rect(x + w - 1.0, y, 1.0, h, c, a);
    }

    /// Anti-aliased line (Xiaolin Wu).
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgb, a: f32) {
        let (mut x0, mut y0, mut x1, mut y1) = (x0, y0, x1, y1);
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        if steep {
            std::mem::swap(&mut x0, &mut y0);
            std::mem::swap(&mut x1, &mut y1);
        }
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
        }
        let dx = x1 - x0;
        let gradient = if dx.abs() < 1e-6 { 0.0 } else { (y1 - y0) / dx };
        let mut y = y0;
        for xi in (x0.round() as i32)..=(x1.round() as i32) {
            let yf = y.floor();
            let frac = y - yf;
            let yi = yf as i32;
            if steep {
                self.blend(yi, xi, c, a * (1.0 - frac));
                self.blend(yi + 1, xi, c, a * frac);
            } else {
                self.blend(xi, yi, c, a * (1.0 - frac));
                self.blend(xi, yi + 1, c, a * frac);
            }
            y += gradient;
        }
    }

    /// Fills a polygon with even-odd scanlines (no anti-aliasing).
    pub fn fill_poly(&mut self, pts: &[(f32, f32)], c: Rgb, a: f32) {
        if pts.len() < 3 {
            return;
        }
        let min_y = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
        let max_y = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
        let mut xs: Vec<f32> = Vec::with_capacity(16);
        for y in min_y..=max_y {
            let fy = y as f32 + 0.5;
            xs.clear();
            for i in 0..pts.len() {
                let (ax, ay) = pts[i];
                let (bx, by) = pts[(i + 1) % pts.len()];
                if (ay <= fy && by > fy) || (by <= fy && ay > fy) {
                    xs.push(ax + (fy - ay) / (by - ay) * (bx - ax));
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks(2) {
                if pair.len() == 2 {
                    for x in (pair[0].round() as i32)..(pair[1].round() as i32) {
                        self.blend(x, y, c, a);
                    }
                }
            }
        }
    }

    pub fn set_face(&mut self, face: Face) {
        self.face = face;
    }

    fn font(&self) -> &Font {
        &self.fonts[self.face as usize]
    }

    fn glyph(&mut self, ch: char, size: f32) -> Arc<(fontdue::Metrics, Vec<u8>)> {
        let key = (self.face as u8, ch, (size * 10.0) as u32);
        if let Some(g) = self.glyphs.get(&key) {
            return Arc::clone(g);
        }
        let (m, bitmap) = self.font().rasterize(ch, size);
        let g = Arc::new((m, bitmap));
        self.glyphs.insert(key, Arc::clone(&g));
        g
    }

    /// Ascent in pixels for the current face at a font size.
    pub fn ascent(&self, size: f32) -> f32 {
        self.font().horizontal_line_metrics(size).map(|m| m.ascent).unwrap_or(size * 0.8)
    }

    /// Draws one character with its top edge at `y`.
    pub fn char_at(&mut self, ch: char, x: f32, y: f32, size: f32, c: Rgb, a: f32) {
        let baseline = y + self.ascent(size);
        let g = self.glyph(ch, size);
        let (m, bitmap) = (&g.0, &g.1);
        let gx0 = x.round() as i32 + m.xmin;
        let gy0 = baseline.round() as i32 - (m.ymin + m.height as i32);
        for by in 0..m.height {
            for bx in 0..m.width {
                let cov = bitmap[by * m.width + bx] as f32 / 255.0;
                if cov > 0.0 {
                    self.blend(gx0 + bx as i32, gy0 + by as i32, c, cov * a);
                }
            }
        }
    }

    /// Draws a string with the current face and spacing. Returns its width.
    pub fn text(&mut self, s: &str, x: f32, y: f32, size: f32, c: Rgb, a: f32) -> f32 {
        let mut cx = x;
        for ch in s.chars() {
            self.char_at(ch, cx, y, size, c, a);
            cx += self.advance(ch, size) + self.spacing;
        }
        cx - x
    }

    pub fn advance(&self, ch: char, size: f32) -> f32 {
        self.font().metrics(ch, size).advance_width
    }

    pub fn text_width(&self, s: &str, size: f32) -> f32 {
        let n = s.chars().count() as f32;
        s.chars().map(|ch| self.advance(ch, size)).sum::<f32>() + self.spacing * n
    }
}

/// Returns a coverage mask (0..1) of `text` drawn with the given face and size,
/// plus its width and height. The mask is padded by `pad` pixels on each side.
pub fn text_mask(canvas: &mut Canvas, text: &str, size: f32, face: Face, spacing: f32, pad: usize) -> (Vec<f32>, usize, usize) {
    let saved_face = canvas.face;
    let saved_spacing = canvas.spacing;
    canvas.face = face;
    canvas.spacing = spacing;
    let tw = canvas.text_width(text, size).ceil() as usize;
    let th = (size * 1.5).ceil() as usize;
    let (mw, mh) = (tw + pad * 2, th + pad * 2);

    // Draw white text on black in a temporary canvas, then read one channel.
    let (saved_w, saved_h) = (canvas.w, canvas.h);
    let saved_px = std::mem::replace(&mut canvas.px, vec![0xff000000; mw * mh]);
    canvas.w = mw;
    canvas.h = mh;
    canvas.text(text, pad as f32, pad as f32, size, (255, 255, 255), 1.0);
    let mask: Vec<f32> = canvas.px.iter().map(|p| ((p >> 16) & 255) as f32 / 255.0).collect();
    canvas.px = saved_px;
    canvas.w = saved_w;
    canvas.h = saved_h;
    canvas.face = saved_face;
    canvas.spacing = saved_spacing;
    (mask, mw, mh)
}

/// Gaussian blur approximated by three box passes, each O(n).
pub fn blur(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let r = (((12.0 * sigma * sigma / 3.0 + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    let mut a = src.to_vec();
    let mut tmp = vec![0.0f32; w * h];
    for _ in 0..3 {
        box_pass_h(&a, &mut tmp, w, h, r);
        box_pass_v(&tmp, &mut a, w, h, r);
    }
    a
}

fn box_pass_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let mut acc = 0.0f32;
        for x in 0..=r.min(w - 1) {
            acc += row[x];
        }
        for x in 0..w {
            dst[y * w + x] = acc * norm;
            let add = x + r + 1;
            let sub = x as isize - r as isize;
            if add < w {
                acc += row[add];
            }
            if sub >= 0 {
                acc -= row[sub as usize];
            }
        }
    }
}

fn box_pass_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for x in 0..w {
        let mut acc = 0.0f32;
        for y in 0..=r.min(h - 1) {
            acc += src[y * w + x];
        }
        for y in 0..h {
            dst[y * w + x] = acc * norm;
            let add = y + r + 1;
            let sub = y as isize - r as isize;
            if add < h {
                acc += src[add * w + x];
            }
            if sub >= 0 {
                acc -= src[sub as usize * w + x];
            }
        }
    }
}

#[inline]
pub fn pack(c: Rgb, a: u8) -> u32 {
    ((a as u32) << 24) | ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32
}

pub fn hex(s: &str) -> Rgb {
    let n = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0);
    ((n >> 16) as u8, (n >> 8) as u8, n as u8)
}
