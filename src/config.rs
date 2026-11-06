//! Paths, user config and an opt-in file logger.
//!
//! Config lives in `$XDG_CONFIG_HOME/cachify/config` as plain `key = value` lines:
//!
//! ```text
//! bitrate = 320          # 96 | 160 | 320
//! client_id = <id>       # optional: your own Spotify app, used only for search
//! audio_cache_mb = 0     # 0 disables the on-disk audio cache
//! normalise = false
//! ```

use std::{
    env, fs,
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

use librespot_playback::config::Bitrate;

pub struct Config {
    pub bitrate: Bitrate,
    pub client_id: Option<String>,
    pub audio_cache_mb: u64,
    pub normalise: bool,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(fallback)
        })
        .join("cachify")
}

impl Config {
    pub fn load() -> Self {
        let config_dir = xdg("XDG_CONFIG_HOME", ".config");
        let cache_dir = xdg("XDG_CACHE_HOME", ".cache");
        let mut cfg = Config {
            bitrate: Bitrate::Bitrate320,
            client_id: None,
            audio_cache_mb: 0,
            normalise: false,
            cache_dir,
            config_dir,
        };

        let Ok(text) = fs::read_to_string(cfg.config_dir.join("config")) else {
            return cfg;
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            match k {
                "bitrate" => {
                    cfg.bitrate = match v {
                        "96" => Bitrate::Bitrate96,
                        "160" => Bitrate::Bitrate160,
                        _ => Bitrate::Bitrate320,
                    }
                }
                "client_id" if !v.is_empty() => cfg.client_id = Some(v.to_string()),
                "audio_cache_mb" => cfg.audio_cache_mb = v.parse().unwrap_or(0),
                "normalise" | "normalize" => cfg.normalise = v == "true",
                _ => {}
            }
        }
        cfg
    }
}

/// Minimal logger: only active when `CACHIFY_LOG` is set (e.g. `CACHIFY_LOG=debug`).
/// Writes to `$XDG_CACHE_HOME/cachify/cachify.log` so it never disturbs the TUI.
struct FileLogger {
    file: Mutex<fs::File>,
    level: log::LevelFilter,
}

impl log::Log for FileLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata())
            && let Ok(mut f) = self.file.lock()
        {
            let _ = writeln!(f, "[{} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

pub fn init_logger(cache_dir: &std::path::Path) {
    let Ok(level) = env::var("CACHIFY_LOG") else { return };
    let level = level.parse().unwrap_or(log::LevelFilter::Info);
    let _ = fs::create_dir_all(cache_dir);
    let Ok(file) = fs::File::create(cache_dir.join("cachify.log")) else { return };
    let logger = Box::leak(Box::new(FileLogger { file: Mutex::new(file), level }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
