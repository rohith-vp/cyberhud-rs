//! Spectrum from the default output monitor (what you hear), via libpulse.
//! Capture runs only while `active` is set, so the tray's recording
//! indicator is only shown while something is playing.

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const BANDS: usize = 24;
const RATE: u32 = 44_100;
const WINDOW: usize = 1024;
const HOP: usize = 512;
const F_MIN: f32 = 40.0;
const F_MAX: f32 = 16_000.0;

pub fn spawn(bands: Arc<Mutex<Vec<f32>>>, active: Arc<AtomicBool>) {
    thread::spawn(move || {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(WINDOW);
        let hann: Vec<f32> = (0..WINDOW)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (WINDOW as f32 - 1.0)).cos())
            .collect();
        let edges = band_edges();
        let mut smoothed = vec![0.0f32; BANDS];
        let mut published = vec![0.0f32; BANDS];
        let mut last_publish = std::time::Instant::now();

        loop {
            if !active.load(Ordering::Relaxed) {
                // Not playing: no capture stream, bars fall to zero.
                if bands.lock().unwrap().iter().any(|v| *v > 0.0) {
                    *bands.lock().unwrap() = vec![0.0; BANDS];
                    smoothed.iter_mut().for_each(|v| *v = 0.0);
                }
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            let spec = Spec { format: Format::S16NE, channels: 1, rate: RATE };
            // Small record fragments (512 samples, about 12 ms) so audio arrives steadily.
            // With the default settings the server sends large blocks, which makes the bars
            // jump in steps and lag behind the music.
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: u32::MAX,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: (HOP * 2) as u32,
            };
            // If the server rejects the custom settings, fall back to its defaults.
            let stream = match Simple::new(None, "cyberhud", Direction::Record, Some("@DEFAULT_MONITOR@"), "spectrum", &spec, None, Some(&attr))
                .or_else(|_| Simple::new(None, "cyberhud", Direction::Record, Some("@DEFAULT_MONITOR@"), "spectrum", &spec, None, None))
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("audio: cannot open the output monitor: {}", e);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            let mut ring: VecDeque<f32> = VecDeque::with_capacity(WINDOW);
            let mut since_hop = 0usize;
            let mut bytes = vec![0u8; HOP * 2];
            while active.load(Ordering::Relaxed) {
                if let Err(e) = stream.read(&mut bytes) {
                    eprintln!("audio: read failed: {}", e);
                    break;
                }
                for pair in bytes.chunks_exact(2) {
                    let s = i16::from_ne_bytes([pair[0], pair[1]]) as f32 / 32768.0;
                    if ring.len() == WINDOW {
                        ring.pop_front();
                    }
                    ring.push_back(s);
                    since_hop += 1;
                    // Analyse once the window is full and a hop of new samples has arrived.
                    if since_hop >= HOP && ring.len() == WINDOW {
                        since_hop = 0;
                        let raw = analyse(&ring, &hann, &*fft, &edges);
                        for (b, v) in raw.iter().enumerate() {
                            // Bars jump up at once and fall gradually.
                            smoothed[b] = v.max(smoothed[b] * 0.82);
                        }
                        let now = std::time::Instant::now();
                        let gap = now.duration_since(last_publish).as_micros() as u64;
                        last_publish = now;
                        crate::stats_debug::MAX_GAP_US.fetch_max(gap, std::sync::atomic::Ordering::Relaxed);
                        if smoothed != published {
                            crate::stats_debug::bump(&crate::stats_debug::BAND_CHANGES);
                            published.copy_from_slice(&smoothed);
                        }
                        *bands.lock().unwrap() = smoothed.clone();
                        crate::stats_debug::bump(&crate::stats_debug::BAND_UPDATES);
                    }
                }
            }
            // `stream` is dropped here, which stops the capture.
        }
    });
}

fn band_edges() -> Vec<(usize, usize)> {
    let bin_hz = RATE as f32 / WINDOW as f32;
    (0..BANDS)
        .map(|i| {
            let lo_hz = F_MIN * (F_MAX / F_MIN).powf(i as f32 / BANDS as f32);
            let hi_hz = F_MIN * (F_MAX / F_MIN).powf((i + 1) as f32 / BANDS as f32);
            let lo = ((lo_hz / bin_hz).floor() as usize).max(1);
            let hi = ((hi_hz / bin_hz).ceil() as usize).max(lo + 1);
            (lo, hi)
        })
        .collect()
}

fn analyse(samples: &VecDeque<f32>, hann: &[f32], fft: &dyn Fft<f32>, edges: &[(usize, usize)]) -> Vec<f32> {
    let mut buf: Vec<Complex<f32>> = samples
        .iter()
        .zip(hann.iter())
        .map(|(s, w)| Complex::new(s * w, 0.0))
        .collect();
    fft.process(&mut buf);
    edges
        .iter()
        .map(|&(lo, hi)| {
            let sum: f32 = (lo..hi).map(|k| buf[k].norm()).sum();
            let mag = sum / (hi - lo) as f32 / (WINDOW as f32 / 2.0);
            let db = 20.0 * (mag + 1e-12).log10();
            ((db + 70.0) / 55.0).clamp(0.0, 1.0)
        })
        .collect()
}

