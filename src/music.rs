//! Track info over MPRIS (Spotify, browsers, mpv, ...). Prefers a player
//! that is actually playing over one that is merely paused.

use mpris::{PlaybackStatus, Player, PlayerFinder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Media {
    /// "Playing", "Paused", "Stopped", "NO PLAYER", or "ERROR".
    pub status: String,
    pub title: String,
    pub artist: String,
}

pub fn spawn(shared: Arc<Mutex<Media>>, playing: Arc<AtomicBool>, force: bool) {
    thread::spawn(move || loop {
        let media = poll();
        if !force {
            playing.store(media.status == "Playing", Ordering::Relaxed);
        }
        *shared.lock().unwrap() = media;
        thread::sleep(Duration::from_secs(2));
    });
}

fn rank(s: PlaybackStatus) -> u8 {
    match s {
        PlaybackStatus::Playing => 0,
        PlaybackStatus::Paused => 1,
        PlaybackStatus::Stopped => 2,
    }
}

fn poll() -> Media {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return Media { status: "ERROR".into(), title: "D-Bus unavailable".into(), artist: String::new() },
    };
    let players = match finder.find_all() {
        Ok(p) => p,
        Err(_) => return Media { status: "NO PLAYER".into(), title: "No active media player".into(), artist: String::new() },
    };
    if players.is_empty() {
        return Media { status: "NO PLAYER".into(), title: "No active media player".into(), artist: String::new() };
    }

    let mut best: Option<(u8, Player, PlaybackStatus)> = None;
    for p in players {
        let Ok(status) = p.get_playback_status() else { continue };
        let r = rank(status);
        if best.as_ref().map_or(true, |(br, _, _)| r < *br) {
            best = Some((r, p, status));
        }
    }
    let Some((_, player, status)) = best else {
        return Media { status: "NO PLAYER".into(), title: "No active media player".into(), artist: String::new() };
    };

    let label = match status {
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Stopped => "Stopped",
    };
    let (title, artist) = match player.get_metadata() {
        Ok(m) => (
            m.title().unwrap_or("Unknown track").to_string(),
            m.artists().map(|a| a.join(", ")).unwrap_or_default(),
        ),
        Err(_) => ("Unknown track".to_string(), String::new()),
    };
    Media { status: label.into(), title, artist }
}
