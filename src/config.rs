//! Paths, user config and an opt-in file logger.
//!
//! Config lives in `$XDG_CONFIG_HOME/trill/config` as plain `key = value` lines:
//!
//! ```text
//! bitrate = 320          # 96 | 160 | 320
//! client_id = <id>       # optional: your own Spotify app, used only for search
//! audio_cache_mb = 0     # 0 disables the on-disk audio cache
//! normalise = false
//! backend = auto         # auto | soloist | librespot
//! device_name = trill  # name shown in Spotify Connect device lists (Soloist)
//! soloist_cache_mb = 200
//! soloist_path =         # default: ~/.local/share/trill/soloist/soloist, then $PATH
//! soloist_ws =           # host:port of an already-running Soloist daemon to use instead
//! ```
//!
//! The Soloist API key is read from `$SOLOIST_API_KEY` or `~/.config/trill/soloist_api_key`.

use std::{
    env, fs,
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

use librespot_playback::config::Bitrate;

pub const SOLOIST_EXE: &str = if cfg!(windows) { "soloist.exe" } else { "soloist" };

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
    let base = env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Windows has no HOME; USERPROFILE is C:\Users\<name>, so paths match the other systems.
            let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")).unwrap_or_else(|| "/tmp".into());
            PathBuf::from(home).join(fallback)
        });
    let dir = base.join("trill");
    // trill was called cachify: carry its login, Soloist pairing and settings over once.
    // shortcut: drop this migration once old installs are gone.
    let old = base.join("cachify");
    if !dir.exists() && old.is_dir() {
        let _ = fs::rename(&old, &dir);
    }
    dir
}

impl Config {
    pub fn load() -> Self {
        let config_dir = xdg("XDG_CONFIG_HOME", ".config");
        let cache_dir = xdg("XDG_CACHE_HOME", ".cache");
        let data_dir = xdg("XDG_DATA_HOME", ".local/share");
        let mut cfg = Config {
            backend: Backend::Auto,
            device_name: "trill".into(),
            soloist_cache_mb: 200,
            soloist_path: None,
            soloist_ws: None,
            data_dir,
            bitrate: Bitrate::Bitrate320,
            client_id: None,
            audio_cache_mb: 0,
            normalise: false,
            eq: cfg!(target_os = "linux"), // PipeWire filter-chain; there is none on macOS
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
        // Windows hides extensions, so a key saved from Notepad is often soloist_api_key.txt,
        // and some editors start the file with a byte-order mark.
        env::var("SOLOIST_API_KEY")
            .ok()
            .or_else(|| fs::read_to_string(self.config_dir.join("soloist_api_key")).ok())
            .or_else(|| fs::read_to_string(self.config_dir.join("soloist_api_key.txt")).ok())
            .map(|k| k.trim().trim_start_matches('\u{feff}').trim().to_string())
            .filter(|k| !k.is_empty())
    }

    /// Why `backend = auto` fell back to librespot, if it did: shown when tracks get skipped.
    pub fn soloist_missing(&self) -> Option<String> {
        if self.backend != Backend::Auto || self.soloist_ws.is_some() {
            return None;
        }
        if self.soloist_api_key().is_none() {
            return Some(format!("no Soloist API key in {}", self.config_dir.join("soloist_api_key").display()));
        }
        self.soloist_bin().is_none().then(|| "Soloist isn't installed: quit and run trill --update-soloist".into())
    }

    /// Where `--update-soloist` installs the binary.
    pub fn soloist_install_dir(&self) -> PathBuf {
        self.data_dir.join("soloist")
    }

    pub fn soloist_bin(&self) -> Option<PathBuf> {
        if let Some(p) = &self.soloist_path {
            return Some(p.clone());
        }
        let local = self.soloist_install_dir().join(SOLOIST_EXE);
        if local.is_file() {
            return Some(local);
        }
        env::split_paths(&env::var_os("PATH")?).map(|d| d.join(SOLOIST_EXE)).find(|p| p.is_file())
    }

    pub fn use_soloist(&self) -> bool {
        match self.backend {
            Backend::Soloist => true,
            Backend::Librespot => false,
            Backend::Auto => self.soloist_ws.is_some() || (self.soloist_api_key().is_some() && self.soloist_bin().is_some()),
        }
    }
}

/// Minimal logger: only active when `TRILL_LOG` is set (e.g. `TRILL_LOG=debug`).
/// Writes to `$XDG_CACHE_HOME/trill/trill.log` so it never disturbs the TUI.
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
    let Ok(level) = env::var("TRILL_LOG") else { return };
    let level = level.parse().unwrap_or(log::LevelFilter::Info);
    let _ = fs::create_dir_all(cache_dir);
    let Ok(file) = fs::File::create(cache_dir.join("trill.log")) else { return };
    let logger = Box::leak(Box::new(FileLogger { file: Mutex::new(file), level }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_windows_saved_key_and_says_what_is_missing() {
        if env::var_os("SOLOIST_API_KEY").is_some() {
            return; // the env var wins over the file
        }
        let dir = env::temp_dir().join(format!("trill-test-key-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            backend: Backend::Auto,
            device_name: "trill".into(),
            soloist_cache_mb: 200,
            soloist_path: Some(dir.join("no-such-soloist")),
            soloist_ws: None,
            data_dir: dir.clone(),
            bitrate: Bitrate::Bitrate320,
            client_id: None,
            audio_cache_mb: 0,
            normalise: false,
            eq: false,
            art: false,
            cache_dir: dir.clone(),
            config_dir: dir.clone(),
        };
        assert!(cfg.soloist_missing().unwrap().starts_with("no Soloist API key in"));
        // Notepad on Windows: .txt added, byte-order mark, CRLF
        fs::write(dir.join("soloist_api_key.txt"), "\u{feff}spak_test\r\n").unwrap();
        assert_eq!(cfg.soloist_api_key().as_deref(), Some("spak_test"));
        let _ = fs::remove_dir_all(&dir);
    }
}
