# CyberHUD (native)

A sci-fi HUD wallpaper for Wayland compositors that support wlr-layer-shell
(KDE Plasma, Sway, Hyprland, and others). This is the native version of the
web-based CyberHUD. It draws the wallpaper itself, with no browser engine and
no helper process.

Status: the layout renders in headless Sway. It has not yet been tested as a
KDE Plasma wallpaper, and the audio and track-info paths have not been tested
with real playback. See "Known limitations".

## What it shows

- Clock, date, UTC time, day of year
- Animated ASCII mountains and twinkling stars (same rules as the web version)
- Clock in Orbitron Black with the same green glow as the web version, on a soft vignette
- CPU load, per-core bars, frequency, temperature, 30-second anti-aliased load graph
- Tactical radar sweep (smooth, per-pixel), default gateway, latency
- Battery, AC state, viewport size, system uptime
- World map with a pulsing marker at your timezone's location
- Timezone and UTC offset
- RAM and root disk usage, disk I/O
- Network throughput with a 30-second graph, online state
- Audio spectrum (24 bands) with the current track, from MPRIS

## Where the data comes from

| Display | Source |
|---|---|
| CPU load, cores | `/proc/stat` (deltas between samples) |
| CPU frequency | `/sys/devices/system/cpu/*/cpufreq/scaling_cur_freq` |
| CPU temperature | `/sys/class/hwmon/*` (coretemp, k10temp, zenpower) |
| RAM | `/proc/meminfo` (MemTotal minus MemAvailable) |
| Disk usage | `statvfs("/")` |
| Disk I/O | `/proc/diskstats` (whole disks only) |
| Network | `/sys/class/net/*/statistics` (interfaces that are up, excluding lo) |
| Gateway | `/proc/net/route` |
| Latency | TCP connect time to 1.1.1.1:443, every 15 s (no data is sent) |
| Battery, AC | `/sys/class/power_supply/*` |
| Uptime | `/proc/uptime` |
| Track title, artist, status | MPRIS over D-Bus, polled every 2 s |
| Spectrum | libpulse recording of the default output monitor (`@DEFAULT_MONITOR@`), FFT with a Hann window |

The recording only runs while a player reports "Playing", so the recording
indicator in the tray is off the rest of the time.

## Build

You need Rust 1.85 or newer, plus the development packages for libpulse,
D-Bus, and pkg-config.

On Arch / CachyOS:

```bash
sudo pacman -S --needed rust pkgconf libpulse dbus
```

On Ubuntu / Debian:

```bash
sudo apt install rustc cargo pkg-config libpulse-dev libdbus-1-dev
```

Then:

```bash
cargo build --release
install -Dm755 target/release/cyberhud-rs ~/.local/bin/cyberhud-rs
```

## Run

Run it directly to test:

```bash
cyberhud-rs
```

To start it at login with systemd (user service):

```bash
mkdir -p ~/.config/systemd/user
cp cyberhud-rs.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now cyberhud-rs.service
```

## Diagnostics

Run with `CYBERHUD_DEBUG=1` to print a status line every five seconds:

```bash
CYBERHUD_DEBUG=1 ./target/release/cyberhud-rs
```

Each line shows the frame rate, how long each frame takes to draw, how many times the background was rebuilt, and how many spectrum updates arrived from the audio capture. If the bars freeze, the spectrum-update count shows whether the audio data stopped or the drawing did.

## Tuning

- Frame rate: `FRAME` in `src/main.rs` (default 66 ms, about 15 fps).
- Stats interval: the `sleep` in the telemetry thread in `src/main.rs` (2 s).
- Latency interval: the `sleep` in the latency thread (15 s).
- Spectrum: `BANDS`, `WINDOW`, `HOP` and the frequency range in `src/audio.rs`.
- Terrain rate: `BG_EVERY` in `src/scene.rs` (default 100 ms). Raising it cuts CPU use but makes the mountains stutter.

## Measured (headless Sway, 1920x1080, release build)

- CPU: about 19% of one core on average over 20 seconds. The sandbox has a single core shared with the compositor, so this overstates the cost on a multi-core laptop.
- Memory: about 55 MB resident.

Rendering is split into two layers. The terrain, stars, vignette, glow, and card frames are rebuilt about 10 times a second. The cards' content, radar, map, spectrum, and clock are redrawn 30 times a second.

These numbers come from a headless test on a virtual machine, so they are only
a rough guide. Please measure on your own machine.

## Known limitations

- Not tested on KDE Plasma yet. Plasma's own desktop layer may draw above or
  below this surface differently from Sway.
- The radar shows the sweep only. It has no contacts.
- GPU temperature and fan speed are not shown.
- The mountains are drawn on the CPU, which is why the frame rate is 15 fps.
  Moving them to the GPU would reduce CPU use.
- Track info and the spectrum were not tested with a real player or real audio.

## Licence

The bundled fonts, JetBrains Mono and Orbitron, are licensed under the SIL Open
Font License (see `assets/OFL-JetBrainsMono.txt` and `assets/OFL-Orbitron.txt`).
The Orbitron file is a static Black (weight 900) instance of the variable font. The project code has no licence yet.
