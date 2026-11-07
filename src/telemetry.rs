//! System telemetry read straight from /proc and /sys. No helper process.

use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub cpu_total: f32,
    pub cores: Vec<f32>,
    pub cpu_ghz: Option<f32>,
    pub cpu_temp: Option<f32>,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
    pub disk_used_gb: f64,
    pub disk_total_gb: f64,
    pub disk_read_mbps: f64,
    pub disk_write_mbps: f64,
    pub net_rx_mbps: f64,
    pub net_tx_mbps: f64,
    pub uptime_secs: f64,
    pub battery: Option<Battery>,
    pub ac_online: Option<bool>,
    pub gateway: Option<String>,
    pub online: bool,
}

pub struct Collector {
    prev_cpu: Vec<(u64, u64)>, // (idle, total) for the aggregate line, then each core
    prev_disk: Option<(u64, u64, Instant)>,
    prev_net: Option<(u64, u64, Instant)>,
    gateway: Option<String>,
    gateway_checked: Option<Instant>,
}

impl Collector {
    pub fn new() -> Self {
        Collector { prev_cpu: Vec::new(), prev_disk: None, prev_net: None, gateway: None, gateway_checked: None }
    }

    pub fn sample(&mut self, latency_online: bool) -> Stats {
        let mut s = Stats::default();
        self.cpu(&mut s);
        s.cpu_ghz = cpu_freq_ghz();
        s.cpu_temp = cpu_temp();
        memory(&mut s);
        disk_usage(&mut s);
        self.disk_io(&mut s);
        self.network(&mut s);
        s.uptime_secs = fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);
        power(&mut s);
        s.gateway = self.gateway();
        s.online = latency_online;
        s
    }

    fn cpu(&mut self, s: &mut Stats) {
        let text = match fs::read_to_string("/proc/stat") {
            Ok(t) => t,
            Err(_) => return,
        };
        let mut now: Vec<(u64, u64)> = Vec::new();
        for line in text.lines().filter(|l| l.starts_with("cpu")) {
            let f: Vec<u64> = line.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
            if f.len() < 8 {
                continue;
            }
            let idle = f[3] + f[4];
            let total: u64 = f[..8].iter().sum();
            now.push((idle, total));
        }
        if now.is_empty() {
            return;
        }
        let pct = |prev: (u64, u64), cur: (u64, u64)| -> f32 {
            let dt = cur.1.saturating_sub(prev.1) as f32;
            let di = cur.0.saturating_sub(prev.0) as f32;
            if dt <= 0.0 { 0.0 } else { (100.0 * (1.0 - di / dt)).clamp(0.0, 100.0) }
        };
        if self.prev_cpu.len() == now.len() {
            s.cpu_total = pct(self.prev_cpu[0], now[0]);
            s.cores = (1..now.len()).map(|i| pct(self.prev_cpu[i], now[i])).collect();
        }
        self.prev_cpu = now;
    }

    fn disk_io(&mut self, s: &mut Stats) {
        let text = match fs::read_to_string("/proc/diskstats") {
            Ok(t) => t,
            Err(_) => return,
        };
        let (mut read, mut written) = (0u64, 0u64);
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || !is_whole_disk(f[2]) {
                continue;
            }
            read += f[5].parse::<u64>().unwrap_or(0) * 512;
            written += f[9].parse::<u64>().unwrap_or(0) * 512;
        }
        let now = Instant::now();
        if let Some((pr, pw, pt)) = self.prev_disk {
            let dt = now.duration_since(pt).as_secs_f64().max(0.001);
            s.disk_read_mbps = read.saturating_sub(pr) as f64 / dt / 1e6;
            s.disk_write_mbps = written.saturating_sub(pw) as f64 / dt / 1e6;
        }
        self.prev_disk = Some((read, written, now));
    }

    fn network(&mut self, s: &mut Stats) {
        let (mut rx, mut tx) = (0u64, 0u64);
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == "lo" {
                    continue;
                }
                let base = e.path();
                let up = fs::read_to_string(base.join("operstate")).map(|v| v.trim() == "up").unwrap_or(false);
                if !up {
                    continue;
                }
                rx += read_u64(&base.join("statistics/rx_bytes"));
                tx += read_u64(&base.join("statistics/tx_bytes"));
            }
        }
        let now = Instant::now();
        if let Some((pr, pt_, pt)) = self.prev_net {
            let dt = now.duration_since(pt).as_secs_f64().max(0.001);
            s.net_rx_mbps = rx.saturating_sub(pr) as f64 / dt / 1e6;
            s.net_tx_mbps = tx.saturating_sub(pt_) as f64 / dt / 1e6;
        }
        self.prev_net = Some((rx, tx, now));
    }

    fn gateway(&mut self) -> Option<String> {
        // The default route rarely changes, so check it once a minute.
        let due = self.gateway_checked.map_or(true, |t| t.elapsed() > Duration::from_secs(60));
        if due {
            self.gateway = read_default_gateway();
            self.gateway_checked = Some(Instant::now());
        }
        self.gateway.clone()
    }
}

fn is_whole_disk(name: &str) -> bool {
    // sda, vda, nvme0n1 are whole disks. sda1, nvme0n1p1 are partitions.
    if let Some(rest) = name.strip_prefix("nvme") {
        return rest.contains('n') && !rest.contains('p');
    }
    if let Some(rest) = name.strip_prefix("sd").or_else(|| name.strip_prefix("vd")) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase());
    }
    false
}

fn read_u64(p: &Path) -> u64 {
    fs::read_to_string(p).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

fn memory(s: &mut Stats) {
    let text = match fs::read_to_string("/proc/meminfo") {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut map: HashMap<&str, f64> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if let Ok(v) = v.parse::<f64>() {
                map.insert(k.trim_end_matches(':'), v);
            }
        }
    }
    let total = map.get("MemTotal").copied().unwrap_or(0.0) * 1024.0;
    let avail = map.get("MemAvailable").copied().unwrap_or(0.0) * 1024.0;
    s.ram_total_gb = total / 1024f64.powi(3);
    s.ram_used_gb = (total - avail) / 1024f64.powi(3);
}

fn disk_usage(s: &mut Stats) {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let path = b"/\0";
    if unsafe { libc::statvfs(path.as_ptr() as *const libc::c_char, &mut st) } == 0 {
        let frsize = st.f_frsize as f64;
        let total = st.f_blocks as f64 * frsize;
        let free = st.f_bfree as f64 * frsize;
        s.disk_total_gb = total / 1024f64.powi(3);
        s.disk_used_gb = (total - free) / 1024f64.powi(3);
    }
}

fn cpu_freq_ghz() -> Option<f32> {
    let mut sum = 0f64;
    let mut n = 0;
    for entry in fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let p = entry.path().join("cpufreq/scaling_cur_freq");
        if let Ok(v) = fs::read_to_string(p) {
            if let Ok(khz) = v.trim().parse::<f64>() {
                sum += khz;
                n += 1;
            }
        }
    }
    if n == 0 { None } else { Some((sum / n as f64 / 1e6) as f32) }
}

fn cpu_temp() -> Option<f32> {
    for entry in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let base = entry.path();
        let name = fs::read_to_string(base.join("name")).unwrap_or_default();
        if matches!(name.trim(), "coretemp" | "k10temp" | "zenpower") {
            if let Ok(v) = fs::read_to_string(base.join("temp1_input")) {
                if let Ok(milli) = v.trim().parse::<f32>() {
                    return Some(milli / 1000.0);
                }
            }
        }
    }
    None
}

fn power(s: &mut Stats) {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else { return };
    for e in entries.flatten() {
        let base = e.path();
        let kind = fs::read_to_string(base.join("type")).unwrap_or_default();
        match kind.trim() {
            "Battery" if s.battery.is_none() => {
                let percent = read_u64(&base.join("capacity")) as u8;
                let status = fs::read_to_string(base.join("status")).unwrap_or_default();
                let charging = matches!(status.trim(), "Charging" | "Full" | "Not charging");
                s.battery = Some(Battery { percent, charging });
            }
            "Mains" => {
                let online = read_u64(&base.join("online")) == 1;
                s.ac_online = Some(s.ac_online.unwrap_or(false) || online);
            }
            _ => {}
        }
    }
}

fn read_default_gateway() -> Option<String> {
    let text = fs::read_to_string("/proc/net/route").ok()?;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let v = u32::from_str_radix(f[2], 16).ok()?;
            return Some(Ipv4Addr::from(v.to_le_bytes()).to_string());
        }
    }
    None
}

/// Measures the TCP connect time to a public resolver, in milliseconds.
pub fn measure_latency() -> Option<f64> {
    let addr = "1.1.1.1:443".to_socket_addrs().ok()?.next()?;
    let start = Instant::now();
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    Some(start.elapsed().as_secs_f64() * 1000.0)
}

/// Local timezone name from the /etc/localtime symlink, e.g. "Asia/Kolkata".
pub fn timezone_name() -> String {
    fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            s.split("zoneinfo/").nth(1).map(|t| t.to_string())
        })
        .unwrap_or_else(|| "local".to_string())
}
