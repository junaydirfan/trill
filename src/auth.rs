//! Login.
//!
//! 1. Playback session: librespot OAuth (browser, once). librespot then stores reusable
//!    credentials in the cache dir, so later starts are instant and offline from the browser.
//! 2. Optional: your own Spotify developer app (`client_id` in config) used only for search,
//!    so we don't share a rate-limit bucket with every other librespot user. Its refresh
//!    token is stored next to the credentials.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use librespot_core::{Session, SessionConfig, authentication::Credentials, cache::Cache};
use librespot_oauth::{OAuthClient, OAuthClientBuilder};

const SESSION_REDIRECT: &str = "http://127.0.0.1:5588/login";
pub const WEBAPI_REDIRECT: &str = "http://127.0.0.1:8898/login";

const SESSION_SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "user-library-read",
    "user-library-modify",
    "playlist-read-private",
    "playlist-read-collaborative",
    "user-read-playback-state",
    "user-modify-playback-state",
];

fn oauth_login(client_id: &str, redirect: &str, scopes: &[&str]) -> Result<librespot_oauth::OAuthToken, String> {
    let client = OAuthClientBuilder::new(client_id, redirect, scopes.to_vec())
        .open_in_browser()
        .with_custom_message("Logged in to cachify. You can close this tab.")
        .build()
        .map_err(|e| e.to_string())?;
    client.get_access_token().map_err(|e| e.to_string())
}

/// Connect a session, using cached credentials when available. May print to the terminal
/// and open a browser; if the startup frame is up it's put away first (main redraws later).
pub async fn connect(session: &Session, cache: &Cache) -> Result<(), String> {
    if let Some(creds) = cache.credentials() {
        match session.connect(creds, true).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
                    ratatui::restore();
                }
                eprintln!("Cached login failed ({e}), logging in again…")
            }
        }
    }
    eprintln!("Opening your browser to log in to Spotify (Premium required)…");
    eprintln!("If it doesn't open, copy the URL printed below into a browser.");
    let client_id = SessionConfig::default().client_id;
    let token = oauth_login(&client_id, SESSION_REDIRECT, SESSION_SCOPES)?;
    session
        .connect(Credentials::with_access_token(token.access_token), true)
        .await
        .map_err(|e| e.to_string())
}

/// Reconnect with stored reusable credentials (after a network drop).
pub async fn reconnect(session: &Session, cache: &Cache) -> Result<(), String> {
    let creds = cache.credentials().ok_or("no cached credentials")?;
    session.connect(creds, true).await.map_err(|e| e.to_string())
}

/// Token for the user's own developer app (search only).
pub struct WebToken {
    client: OAuthClient,
    access: String,
    refresh: String,
    expires: Instant,
    path: PathBuf,
}

impl WebToken {
    fn client(client_id: &str) -> Result<OAuthClient, String> {
        OAuthClientBuilder::new(client_id, WEBAPI_REDIRECT, vec!["user-read-private"])
            .open_in_browser()
            .with_custom_message("cachify search is authorised. You can close this tab.")
            .build()
            .map_err(|e| e.to_string())
    }

    pub async fn load(client_id: &str, dir: &Path) -> Result<Self, String> {
        let path = dir.join("webapi_refresh_token");
        let client = Self::client(client_id)?;
        let token = match fs::read_to_string(&path).ok().filter(|s| !s.trim().is_empty()) {
            Some(rt) => match client.refresh_token_async(rt.trim()).await {
                Ok(t) => t,
                Err(_) => Self::interactive(&client)?,
            },
            None => Self::interactive(&client)?,
        };
        let me = WebToken {
            client,
            access: token.access_token,
            refresh: token.refresh_token,
            expires: token.expires_at,
            path,
        };
        me.save();
        Ok(me)
    }

    fn interactive(client: &OAuthClient) -> Result<librespot_oauth::OAuthToken, String> {
        eprintln!("Authorising your own Spotify app for search (one time)…");
        eprintln!("Make sure {WEBAPI_REDIRECT} is a Redirect URI in your app settings.");
        client.get_access_token().map_err(|e| e.to_string())
    }

    fn save(&self) {
        let _ = fs::write(&self.path, &self.refresh);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
    }

    pub async fn get(&mut self) -> Result<String, String> {
        if Instant::now() + std::time::Duration::from_secs(30) >= self.expires {
            let t = self
                .client
                .refresh_token_async(&self.refresh)
                .await
                .map_err(|e| e.to_string())?;
            self.access = t.access_token;
            if !t.refresh_token.is_empty() {
                self.refresh = t.refresh_token;
            }
            self.expires = t.expires_at;
            self.save();
        }
        Ok(self.access.clone())
    }
}
