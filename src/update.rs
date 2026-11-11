//! Self-update from GitHub releases: `trill --update`, and a once-a-day check that puts
//! "vX is out" in the title bar.

use std::{
    env, fs,
    path::Path,
    process::Command,
    time::Duration,
};

use bytes::Bytes;
use librespot_core::Session;
use serde_json::Value;

const API: &str = "https://api.github.com/repos/junaydirfan/trill/releases/latest";
const DAY: Duration = Duration::from_secs(24 * 3600);

/// The release download for this system, as CI names it.
const ASSET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("trill-linux-x86_64.tar.gz")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("trill-macos-arm64.tar.gz")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("trill-windows-x86_64.zip")
} else {
    None
};
const EXE: &str = if cfg!(windows) { "trill.exe" } else { "trill" };

/// "v1.2.3" → (1, 2, 3); missing parts are 0, anything after a `-` is ignored.
fn parse(v: &str) -> (u32, u32, u32) {
    let mut n = v.trim().trim_start_matches('v').split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

fn is_newer(tag: &str) -> bool {
    parse(tag) > parse(env!("CARGO_PKG_VERSION"))
}

fn tag_of(json: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(json).ok()?["tag_name"].as_str().map(String::from)
}

/// The newest release's tag if it's newer than this build. Asks GitHub at most once a day
/// (the answer is kept in the cache dir); stays quiet when offline.
pub async fn check(session: &Session, cache: &Path) -> Option<String> {
    let file = cache.join("latest_release");
    let fresh = fs::metadata(&file).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|d| d < DAY));
    let tag = if fresh {
        fs::read_to_string(&file).ok()?
    } else {
        let req = http::Request::get(API).header("Accept", "application/vnd.github+json").body(Bytes::new()).ok()?;
        let body = tokio::time::timeout(Duration::from_secs(10), session.http_client().request_body(req))
            .await
            .ok()?
            .map_err(|e| log::debug!("update check: {e}"))
            .ok()?;
        let tag = tag_of(&body)?;
        let _ = fs::write(&file, &tag);
        tag
    };
    let tag = tag.trim();
    is_newer(tag).then(|| tag.to_string())
}

/// `trill --update`: download the latest release and put it in place of this binary.
pub fn run(cache: &Path) -> i32 {
    let Some(asset) = ASSET else {
        eprintln!("There's no ready-made trill for this system. Update by building it:");
        eprintln!("  cargo install --git https://github.com/junaydirfan/trill --force");
        return 1;
    };
    let _ = fs::create_dir_all(cache);
    let out = Command::new("curl").args(["-fsSL", "-H", "Accept: application/vnd.github+json", API]).output();
    let Some(tag) = out.ok().filter(|o| o.status.success()).and_then(|o| tag_of(&o.stdout)) else {
        eprintln!("Couldn't ask GitHub for the latest release (are you online?).");
        return 1;
    };
    let _ = fs::write(cache.join("latest_release"), &tag);
    let have = env!("CARGO_PKG_VERSION");
    if !is_newer(&tag) {
        println!("trill {have} is up to date (latest release: {tag}).");
        return 0;
    }

    let staging = cache.join("update");
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = fs::create_dir_all(&staging) {
        eprintln!("{}: {e}", staging.display());
        return 1;
    }
    let url = format!("https://github.com/junaydirfan/trill/releases/download/{tag}/{asset}");
    let archive = staging.join(asset);
    println!("Updating trill {have} → {tag}…");
    let ok = Command::new("curl").args(["-fL", "--progress-bar", "-o"]).arg(&archive).arg(&url).status().is_ok_and(|s| s.success())
        // tar detects gzip or zip itself (GNU tar, macOS bsdtar and Windows' tar.exe alike)
        && Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&staging).status().is_ok_and(|s| s.success());
    let Some(new) = ok.then(|| crate::walk_find(&staging, EXE)).flatten() else {
        eprintln!("Download or extract of {url} failed.");
        return 1;
    };

    let res = (|| -> std::io::Result<std::path::PathBuf> {
        let exe = env::current_exe()?.canonicalize()?;
        let dir = exe.parent().unwrap_or(Path::new("."));
        // copy next to the old one first so the swap below is a rename on one disk
        let tmp = dir.join(".trill-update");
        fs::copy(&new, &tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        // Windows can't replace a running .exe but can rename it; the old one goes next time.
        #[cfg(windows)]
        {
            let old = exe.with_extension("old");
            let _ = fs::remove_file(&old);
            fs::rename(&exe, &old)?;
        }
        if let Err(e) = fs::rename(&tmp, &exe) {
            #[cfg(windows)]
            let _ = fs::rename(exe.with_extension("old"), &exe);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(exe)
    })();
    let _ = fs::remove_dir_all(&staging);
    match res {
        Ok(exe) => {
            println!("Updated {} to {tag}. Restart trill to use it.", exe.display());
            0
        }
        Err(e) => {
            eprintln!("Couldn't replace trill: {e}");
            eprintln!("If a package manager installed it, update it there; otherwise see the README's Updating section.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert_eq!(parse("v1.2"), (1, 2, 0));
        assert_eq!(parse("v0.10.1-rc1"), (0, 10, 1));
        assert!(parse("v0.10.0") > parse("0.9.9"));
        assert!(is_newer("v999.0.0"));
        assert!(!is_newer(concat!("v", env!("CARGO_PKG_VERSION"))));
        assert_eq!(tag_of(br#"{"tag_name":"v0.3.0","name":"x"}"#).as_deref(), Some("v0.3.0"));
    }
}
