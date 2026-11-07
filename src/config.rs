//! Paths, user config and an opt-in file logger.
//!
//! Config lives in `$XDG_CONFIG_HOME/cachify/config` as plain `key = value` lines:
//!
//! ```text
//! bitrate = 320          # 96 | 160 | 320
//! client_id = <id>       # optional: your own Spotify app, used only for search
//! audio_cache_mb = 0     # 0 disables the on-disk audio cache
//! normalise = false
//! backend = auto         # auto | soloist | librespot
//! device_name = cachify  # name shown in Spotify Connect device lists (Soloist)
//! soloist_cache_mb = 200
//! soloist_path =         # default: ~/.local/share/cachify/soloist/soloist, then $PATH
//! soloist_ws =           # host:port of an already-running Soloist daemon to use instead
//! ```
//!
//! The Soloist API key is read from `$SOLOIST_API_KEY` or `~/.config/cachify/soloist_api_key`.

use std::{
    env, fs,
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

use librespot_playback::config::Bitrate;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Soloist,
    Librespot,
}

pub struct Config {
    pub backend: Backend,
    pub device_name: String,
    pub soloist_cache_mb: u64,
    pub soloist_path: Option<PathBuf>,
    pub soloist_ws: Option<String>,
    pub data_dir: PathBuf,
    pub bitrate: Bitrate,
    pub client_id: Option<String>,
    pub audio_cache_mb: u64,
    pub normalise: bool,
    /// Run the PipeWire equalizer (default on; `eq = off` to skip it entirely).
    pub eq: bool,
    /// Album art in the terminal (default on; `art = off` to hide it).
    pub art: bool,
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
        let data_dir = xdg("XDG_DATA_HOME", ".local/share");
        let mut cfg = Config {
            backend: Backend::Auto,
            device_name: "cachify".into(),
            soloist_cache_mb: 200,
            soloist_path: None,
            soloist_ws: None,
            data_dir,
            bitrate: Bitrate::Bitrate320,
            client_id: None,
            audio_cache_mb: 0,
            normalise: false,
            eq: true,
            art: true,
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
                "eq" => cfg.eq = !matches!(v, "off" | "false" | "no" | "0"),
                "art" => cfg.art = !matches!(v, "off" | "false" | "no" | "0"),
                "backend" => {
                    cfg.backend = match v {
                        "soloist" => Backend::Soloist,
                        "librespot" => Backend::Librespot,
                        _ => Backend::Auto,
                    }
                }
                "device_name" if !v.is_empty() => cfg.device_name = v.to_string(),
                "soloist_cache_mb" => cfg.soloist_cache_mb = v.parse().unwrap_or(200),
                "soloist_path" if !v.is_empty() => cfg.soloist_path = Some(PathBuf::from(v)),
                "soloist_ws" if !v.is_empty() => cfg.soloist_ws = Some(v.to_string()),
                _ => {}
            }
        }
        cfg
    }
}

impl Config {
    pub fn soloist_api_key(&self) -> Option<String> {
        env::var("SOLOIST_API_KEY")
            .ok()
            .or_else(|| fs::read_to_string(self.config_dir.join("soloist_api_key")).ok())
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    }

    /// Where `--update-soloist` installs the binary.
    pub fn soloist_install_dir(&self) -> PathBuf {
        self.data_dir.join("soloist")
    }

    pub fn soloist_bin(&self) -> Option<PathBuf> {
        if let Some(p) = &self.soloist_path {
            return Some(p.clone());
        }
        let local = self.soloist_install_dir().join("soloist");
        if local.is_file() {
            return Some(local);
        }
        env::var_os("PATH")?
            .to_str()?
            .split(':')
            .map(|d| PathBuf::from(d).join("soloist"))
            .find(|p| p.is_file())
    }

    pub fn use_soloist(&self) -> bool {
        match self.backend {
            Backend::Soloist => true,
            Backend::Librespot => false,
            Backend::Auto => self.soloist_ws.is_some() || (self.soloist_api_key().is_some() && self.soloist_bin().is_some()),
        }
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
