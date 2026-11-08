//! Audio settings: equalizer and crossfade.
//!
//! - **EQ**: a 10-band PipeWire filter-chain (`pipewire -c <conf>`) exposing a sink named
//!   `trill_eq`. Only trill's player is routed into it; it forwards to your normal output
//!   (following default-device changes). Gains change live via `pw-cli`, no audio gap.
//!   Cost: ~3 MB, 2 threads.
//! - **Crossfade**: Spotify's own crossfade inside Soloist, set through Soloist's prefs file
//!   before it starts (changing it restarts Soloist; trill resumes where you were).

use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use librespot_playback::config::Bitrate;

pub const EQ_NODE: &str = "trill_eq";
pub const BANDS: [u32; 10] = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const MAX_DB: f32 = 12.0;
pub const CROSSFADE_STEPS: [u32; 5] = [0, 3, 5, 8, 12];

pub const PRESETS: &[(&str, [f32; 10])] = &[
    ("Flat", [0.0; 10]),
    ("Bass Boost", [6.0, 5.0, 4.0, 2.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass Reducer", [-6.0, -5.0, -4.0, -2.0, -0.5, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble Boost", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0]),
    ("Vocal", [-2.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0]),
    ("Rock", [4.0, 3.0, 2.0, 0.0, -1.0, -1.0, 1.0, 2.0, 3.0, 4.0]),
    ("Pop", [-1.0, 0.0, 2.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0, -1.0]),
    ("Hip-Hop", [5.0, 4.0, 1.0, 3.0, -1.0, -1.0, 1.0, -1.0, 2.0, 3.0]),
    ("Electronic", [5.0, 4.0, 1.0, 0.0, -2.0, 1.0, 0.0, 1.0, 4.0, 5.0]),
    ("Acoustic", [3.0, 3.0, 2.0, 1.0, 1.0, 1.0, 2.0, 3.0, 2.0, 1.0]),
    ("Loudness", [6.0, 4.0, 0.0, 0.0, -2.0, 0.0, -1.0, -3.0, 4.0, 2.0]),
];
/// Index meaning "user-tuned bands".
pub const CUSTOM: usize = PRESETS.len();

/// Sort modes as named in the settings file (`app::Sort` keeps this order).
pub const SORTS: [&str; 6] = ["added", "playlist", "title", "artist", "album", "duration"];

/// Everything the user can change from the keyboard, kept across restarts
/// (`~/.local/share/trill/audio_settings`). Volume lives in librespot's cache.
pub struct AudioSettings {
    /// Selected preset index, or [`CUSTOM`].
    pub preset: usize,
    pub custom: [f32; 10],
    pub crossfade_s: u32,
    pub shuffle: bool,
    /// 0 off, 1 all, 2 one
    pub repeat: u8,
    /// Index into [`SORTS`].
    pub sort: u8,
    pub art: bool,
    /// Index into [`crate::theme::THEMES`], or `theme::COVER`.
    pub theme: usize,
    /// Last cover colour, so the `cover` theme starts in it instead of the default green.
    pub tint: Option<[u8; 3]>,
    path: PathBuf,
}

impl AudioSettings {
    pub fn load(dir: &Path) -> Self {
        let mut s = AudioSettings {
            preset: 0,
            custom: [0.0; 10],
            crossfade_s: 0,
            shuffle: false,
            repeat: 0,
            sort: 0,
            art: true,
            theme: 0,
            tint: None,
            path: dir.join("audio_settings"),
        };
        let Ok(text) = fs::read_to_string(&s.path) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "preset" => {
                    let v = v.trim();
                    s.preset = if v == "Custom" {
                        CUSTOM
                    } else {
                        PRESETS.iter().position(|(n, _)| *n == v).unwrap_or(0)
                    }
                }
                "custom" => {
                    for (i, g) in v.split(',').take(10).enumerate() {
                        s.custom[i] = g.trim().parse::<f32>().unwrap_or(0.0).clamp(-MAX_DB, MAX_DB);
                    }
                }
                "crossfade" => s.crossfade_s = v.trim().parse::<u32>().unwrap_or(0).min(12),
                "shuffle" => s.shuffle = v.trim() == "on",
                "repeat" => s.repeat = ["off", "all", "one"].iter().position(|r| *r == v.trim()).unwrap_or(0) as u8,
                "sort" => s.sort = SORTS.iter().position(|n| *n == v.trim()).unwrap_or(0) as u8,
                "art" => s.art = v.trim() != "off",
                "theme" => s.theme = crate::theme::by_name(v.trim()),
                "tint" => {
                    let c: Vec<u8> = v.split(',').filter_map(|c| c.trim().parse().ok()).collect();
                    s.tint = <[u8; 3]>::try_from(c).ok();
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let custom = self.custom.iter().map(|g| format!("{g:.1}")).collect::<Vec<_>>().join(",");
        let on = |b: bool| if b { "on" } else { "off" };
        let text = format!(
            "preset={}\ncustom={custom}\ncrossfade={}\nshuffle={}\nrepeat={}\nsort={}\nart={}\ntheme={}\n",
            self.preset_name(),
            self.crossfade_s,
            on(self.shuffle),
            ["off", "all", "one"][self.repeat.min(2) as usize],
            SORTS[(self.sort as usize).min(SORTS.len() - 1)],
            on(self.art),
            crate::theme::name(self.theme),
        );
        let text = match self.tint {
            Some([r, g, b]) => format!("{text}tint={r},{g},{b}\n"),
            None => text,
        };
        if let Some(d) = self.path.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::write(&self.path, text);
    }

    pub fn preset_name(&self) -> &'static str {
        PRESETS.get(self.preset).map(|p| p.0).unwrap_or("Custom")
    }

    pub fn is_flat(&self) -> bool {
        self.gains().iter().all(|g| *g == 0.0)
    }

    pub fn gains(&self) -> [f32; 10] {
        PRESETS.get(self.preset).map(|p| p.1).unwrap_or(self.custom)
    }

    pub fn cycle_preset(&mut self, back: bool) {
        let n = PRESETS.len() + 1; // + Custom
        self.preset = if back { (self.preset + n - 1) % n } else { (self.preset + 1) % n };
    }

    /// Adjust one band; switches to Custom starting from whatever is currently applied.
    pub fn nudge(&mut self, band: usize, delta: f32) {
        if self.preset != CUSTOM {
            self.custom = self.gains();
            self.preset = CUSTOM;
        }
        self.custom[band] = (self.custom[band] + delta).clamp(-MAX_DB, MAX_DB);
    }

    pub fn set_band(&mut self, band: usize, value: f32) {
        self.nudge(band, value - self.gains()[band]);
    }

    pub fn cycle_crossfade(&mut self) {
        let i = CROSSFADE_STEPS.iter().position(|&s| s == self.crossfade_s).unwrap_or(0);
        self.crossfade_s = CROSSFADE_STEPS[(i + 1) % CROSSFADE_STEPS.len()];
    }
}

/// Linear gain that keeps the loudest boost from clipping.
fn preamp(gains: &[f32; 10]) -> f32 {
    let max = gains.iter().cloned().fold(0.0f32, f32::max);
    10f32.powf(-max / 20.0)
}

fn props(gains: &[f32; 10]) -> String {
    let mut p = format!("\"pre:Mult\" {:.4}", preamp(gains));
    for (i, g) in gains.iter().enumerate() {
        p.push_str(&format!(" \"b{}:Gain\" {:.1}", i + 1, g));
    }
    format!("{{ params = [ {p} ] }}")
}

fn conf(gains: &[f32; 10]) -> String {
    let mut nodes = format!(
        "          {{ type = builtin name = pre label = linear control = {{ \"Mult\" = {:.4} \"Add\" = 0.0 }} }}\n",
        preamp(gains)
    );
    let mut links = String::from("          { output = \"pre:Out\" input = \"b1:In\" }\n");
    for (i, (f, g)) in BANDS.iter().zip(gains).enumerate() {
        let (label, q) = match i {
            0 => ("bq_lowshelf", 0.7),
            9 => ("bq_highshelf", 0.7),
            _ => ("bq_peaking", 1.4),
        };
        nodes.push_str(&format!(
            "          {{ type = builtin name = b{n} label = {label} control = {{ \"Freq\" = {f} \"Q\" = {q} \"Gain\" = {g:.1} }} }}\n",
            n = i + 1
        ));
        if i > 0 {
            links.push_str(&format!("          {{ output = \"b{}:Out\" input = \"b{}:In\" }}\n", i, i + 1));
        }
    }
    format!(
        r#"# Generated by trill — 10-band EQ for trill's player only.
context.properties = {{ log.level = 0 }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
  {{ name = libpipewire-module-rt args = {{ nice.level = -11 }} flags = [ ifexists nofail ] }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = "trill EQ"
      media.name = "trill EQ"
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
        inputs = [ "pre:In" ]
        outputs = [ "b10:Out" ]
      }}
      audio.channels = 2
      audio.position = [ FL FR ]
      # Music doesn't need low latency: bigger chunks = fewer wakeups.
      capture.props = {{ node.name = "{EQ_NODE}" media.class = Audio/Sink node.passive = true node.latency = 2048/48000 }}
      playback.props = {{ node.name = "{EQ_NODE}.output" node.passive = true media.role = Music node.latency = 2048/48000 }}
    }}
  }}
]
"#
    )
}

fn node_exists() -> bool {
    Command::new("pw-cli")
        .args(["info", EQ_NODE])
        .output()
        .map(|o| {
            let all = [o.stdout, o.stderr].concat();
            let s = String::from_utf8_lossy(&all);
            s.contains("id:") && !s.contains("unknown global")
        })
        .unwrap_or(false)
}

/// The running EQ process.
pub struct Eq {
    pid: u32,
    tx: mpsc::Sender<[f32; 10]>,
}

impl Eq {
    pub fn start(cache_dir: &Path, gains: &[f32; 10]) -> Result<Eq, String> {
        let path = cache_dir.join("trill-eq.conf");
        fs::write(&path, conf(gains)).map_err(|e| e.to_string())?;
        let log = fs::File::create(cache_dir.join("eq.log")).map_err(|e| e.to_string())?;
        let mut c = Command::new("pipewire");
        c.arg("-c").arg(&path).stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
        // SAFETY: prctl is async-signal-safe; the EQ goes away if trill dies.
        unsafe {
            c.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = c.spawn().map_err(|e| format!("can't run pipewire: {e}"))?;
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !node_exists() {
            if let Ok(Some(st)) = child.try_wait() {
                return Err(format!("EQ exited ({st}); see ~/.cache/trill/eq.log"));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err("EQ didn't come up in 5 s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Reap the child when it exits so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        // One worker applies gain changes in order, skipping to the newest if keys repeat.
        let (tx, rx) = mpsc::channel::<[f32; 10]>();
        let _ = std::thread::Builder::new().name("eq".into()).spawn(move || {
            while let Ok(mut g) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    g = newer;
                }
                let _ = Command::new("pw-cli")
                    .args(["set-param", EQ_NODE, "Props", &props(&g)])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        });
        Ok(Eq { pid, tx })
    }

    pub fn apply(&self, gains: &[f32; 10]) {
        let _ = self.tx.send(*gains);
    }

    pub fn stop(&self) {
        // SAFETY: plain signal to our own child.
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
    }
}

/// Write Soloist's per-user prefs (crossfade, bitrate) before it starts. Soloist only reads
/// these when the matching `*_migrated*` flag is false, so we reset the flags each time.
pub fn write_soloist_prefs(data_dir: &Path, user: &str, crossfade_s: u32, bitrate: Bitrate) {
    let dir = data_dir.join("settings").join("Users").join(format!("{user}-user"));
    let path = dir.join("prefs");
    let old = fs::read_to_string(&path).unwrap_or_default();
    let managed = ["audio.crossfade", "audio.play_bitrate", "audio.equalizer"];
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| !managed.iter().any(|m| l.starts_with(m)))
        .map(String::from)
        .collect();
    // Spotify's quality enumeration: 4 = very high (320 kbps, verified), 3 = high, 2 = normal.
    let q = match bitrate {
        Bitrate::Bitrate320 => 4,
        Bitrate::Bitrate160 => 3,
        Bitrate::Bitrate96 => 2,
    };
    lines.extend([
        "audio.crossfade_migrated_to_ps=false".into(),
        format!("audio.crossfade_v2={}", crossfade_s > 0),
        format!("audio.crossfade.time_v2={}", crossfade_s.max(1) * 1000),
        "audio.play_bitrate_non_metered_migrated=false".into(),
        format!("audio.play_bitrate_non_metered_enumeration={q}"),
        format!("audio.play_bitrate_enumeration={q}"),
    ]);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(&path, lines.join("\n") + "\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_and_custom_roundtrip() {
        let dir = std::env::temp_dir().join(format!("trill-test-{}", std::process::id()));
        let mut s = AudioSettings::load(&dir);
        assert_eq!(s.preset_name(), "Flat");
        s.cycle_preset(false);
        assert_eq!(s.preset_name(), "Bass Boost");
        s.nudge(9, 3.0); // tweak treble -> becomes Custom based on Bass Boost
        assert_eq!(s.preset, CUSTOM);
        assert_eq!(s.gains()[0], 6.0);
        assert_eq!(s.gains()[9], 3.0);
        s.nudge(0, 100.0);
        assert_eq!(s.gains()[0], MAX_DB);
        s.cycle_crossfade();
        s.save();
        let t = AudioSettings::load(&dir);
        assert_eq!(t.preset, CUSTOM);
        assert_eq!(t.gains(), s.gains());
        assert_eq!(t.crossfade_s, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preamp_prevents_clipping() {
        assert!((preamp(&[0.0; 10]) - 1.0).abs() < 1e-6);
        let mut g = [0.0; 10];
        g[0] = 6.0;
        assert!((preamp(&g) - 0.5012).abs() < 1e-3); // -6 dB
        assert!(props(&g).contains("\"b1:Gain\" 6.0"));
        assert!(conf(&g).contains("bq_lowshelf") && conf(&g).contains("inputs = [ \"pre:In\" ]"));
    }

    #[test]
    fn settings_survive_restart() {
        let dir = std::env::temp_dir().join(format!("trill-test-{}", std::process::id()));
        let mut a = AudioSettings::load(&dir);
        assert!(!a.shuffle && a.repeat == 0 && a.sort == 0 && a.art);
        (a.shuffle, a.repeat, a.sort, a.art, a.crossfade_s) = (true, 2, 4, false, 6);
        (a.theme, a.tint) = (crate::theme::COVER, Some([224, 135, 99]));
        a.save();
        let b = AudioSettings::load(&dir);
        let _ = fs::remove_dir_all(&dir);
        assert!(b.shuffle && b.repeat == 2 && b.sort == 4 && !b.art && b.crossfade_s == 6);
        assert!(b.theme == crate::theme::COVER && b.tint == Some([224, 135, 99]));
    }
}
