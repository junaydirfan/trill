//! Data access.
//!
//! Almost everything goes through librespot's own session (spclient), *not* the public
//! Web API, so we are not affected by Web API dev-mode quotas / restrictions:
//! - playlists list      -> spclient rootlist
//! - any list of tracks  -> context-resolve (playlists, albums, artists, liked songs)
//! - track names/artists -> batched extended-metadata (hundreds of tracks per request)
//! - artists and albums  -> the same batched metadata (an artist page is 3 requests)
//! - saved albums, followed artists -> collection2v2 paging
//!
//! Only search prefers the Web API (better results, albums/artists/playlists too), with a
//! fallback to context-resolve search that needs no token at all.

use std::{collections::HashMap, collections::VecDeque, sync::Arc, time::{Duration, Instant}};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::{Metadata, Playlist};
use librespot_protocol::{
    context_page::ContextPage,
    extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
    extension_kind::ExtensionKind,
    metadata::{Album as PAlbum, Artist as PArtist, Track as PTrack},
    playlist4_external::SelectedListContent,
};
use protobuf::{EnumOrUnknown, Message};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::auth::WebToken;

/// Upper bound on tracks pulled for a single list (keeps memory bounded for huge libraries).
const MAX_TRACKS: usize = 10_000;
const MAX_PAGES: usize = 60;
const META_CHUNK: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Track,
    Album,
    Playlist,
    Artist,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub kind: Kind,
    /// Cover image URL (album art), when known.
    pub art: Option<String>,
    /// When it was added to the playlist / liked songs (ms since epoch; 0 = unknown).
    pub added_at: u64,
    /// Blend playlists: the members this song was picked for ("jj, Cube"); else empty.
    pub members: String,
}

pub type Rows = Arc<Vec<Row>>;

/// Sidebar views of the library sets (":collection" makes them sortable like Liked Songs).
pub const SAVED_ALBUMS: &str = "trill:collection:albums";
pub const FOLLOWED_ARTISTS: &str = "trill:collection:artists";
type Res<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// The user's playlists (in sidebar order), as `(uri, name, editable, pinned)`.
/// `uri<TAB>name<TAB>editable<TAB>pinned` per line; a stale list is fine since it's replaced within a second.
pub fn read_playlist_cache(dir: &std::path::Path) -> Option<Vec<(String, String, bool, bool)>> {
    let text = std::fs::read_to_string(dir.join("playlists")).ok()?;
    let list: Vec<_> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            Some((f.next()?.to_string(), f.next()?.to_string(), f.next() == Some("1"), f.next() == Some("1")))
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

pub fn write_playlist_cache(dir: &std::path::Path, list: &[(String, String, bool, bool)]) {
    let mut text = String::with_capacity(list.len() * 50);
    for (uri, name, editable, pinned) in list {
        let name = name.replace(['\t', '\n', '\r'], " ");
        text.push_str(uri);
        text.push('\t');
        text.push_str(&name);
        text.push_str(if *editable { "\t1" } else { "\t0" });
        text.push_str(if *pinned { "\t1\n" } else { "\t0\n" });
    }
    let tmp = dir.join("playlists.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("playlists"));
    }
}

/// (uri, name, editable, pinned): editable = we may add/remove songs (own or collaborative).
/// Pinned playlists and folders come first, in the order the official apps show them.
pub async fn playlists(session: &Session) -> Res<Vec<(String, String, bool, bool)>> {
    let (list, pins) = tokio::join!(rootlist(session), crate::library::pins(session));
    let pins = pins.unwrap_or_else(|err| {
        log::warn!("pins: {err}");
        Vec::new()
    });
    // Folders only as pins; their playlists are listed anyway.
    // shortcut: pinned albums and artists aren't shown (the sidebar holds playlists only).
    let mut out: Vec<_> = list?
        .into_iter()
        .filter(|p| !p.0.starts_with("spotify:folder:") || pins.contains(&p.0))
        .map(|(uri, name, editable, _)| (uri, name, editable, false))
        .collect();
    let rank = |uri: &str| pins.iter().position(|p| p == uri).unwrap_or(usize::MAX);
    out.sort_by_cached_key(|p| rank(&p.0)); // stable: the rest keep rootlist order
    for p in &mut out {
        p.3 = rank(&p.0) != usize::MAX;
    }
    Ok(out)
}

/// The playlists inside a folder (and its subfolders), for opening a pinned folder.
pub async fn folder_rows(session: &Session, uri: &str) -> Res<Vec<Row>> {
    let id = uri.trim_start_matches("spotify:folder:");
    Ok(rootlist(session)
        .await?
        .into_iter()
        .filter(|p| !p.0.starts_with("spotify:folder:") && p.3.iter().any(|f| f == id))
        .map(|(uri, title, ..)| Row {
            uri,
            title,
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
            kind: Kind::Playlist,
            art: None,
            added_at: 0,
            members: String::new(),
        })
        .collect())
}

/// Every rootlist entry as (uri, name, editable, ids of the folders it's in). A folder shows
/// up as `spotify:folder:<id>` where it starts.
async fn rootlist(session: &Session) -> Res<Vec<(String, String, bool, Vec<String>)>> {
    let bytes = session.spclient().get_rootlist(0, Some(1000)).await.map_err(e)?;
    let list = SelectedListContent::parse_from_bytes(&bytes).map_err(e)?;
    let mut out = Vec::new();
    let mut folders: Vec<String> = Vec::new();
    if let Some(contents) = list.contents.as_ref() {
        for (i, item) in contents.items.iter().enumerate() {
            let uri = item.uri();
            // spotify:start-group:<id>:<url-encoded name> … spotify:end-group:<id>
            if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
                let (id, name) = rest.split_once(':').unwrap_or((rest, ""));
                out.push((format!("spotify:folder:{id}"), url_decode(name), false, folders.clone()));
                folders.push(id.to_string());
                continue;
            }
            if uri.starts_with("spotify:end-group:") {
                folders.pop();
                continue;
            }
            if !uri.starts_with("spotify:playlist:") && !uri.contains(":playlist:") {
                continue; // anything odd
            }
            let meta = contents.meta_items.get(i);
            let name = meta.and_then(|m| m.attributes.as_ref()).map(|a| a.name().to_string()).unwrap_or_default();
            let editable = meta
                .and_then(|m| m.capabilities.as_ref())
                .map(|c| c.can_edit_items())
                .unwrap_or_else(|| meta.is_some_and(|m| m.owner_username() == session.username()));
            out.push((uri.to_string(), name, editable, folders.clone()));
        }
    }

    // Fill in any names the rootlist didn't decorate (a handful of small requests, 8 at a time).
    let missing: Vec<usize> = (0..out.len()).filter(|&i| out[i].1.is_empty()).collect();
    if !missing.is_empty() {
        let names: Vec<(usize, Option<String>)> = stream::iter(missing)
            .map(|i| {
                let uri = out[i].0.clone();
                let session = session.clone();
                async move {
                    let name = match SpotifyUri::from_uri(&uri) {
                        Ok(u) => Playlist::get(&session, &u).await.ok().map(|p| p.name().to_string()),
                        Err(_) => None,
                    };
                    (i, name)
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;
        for (i, name) in names {
            out[i].1 = name.filter(|n| !n.is_empty()).unwrap_or_else(|| "(untitled)".into());
        }
    }
    Ok(out)
}

/// Folder names in the rootlist are form-encoded ("My+Mix%21").
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match (b[i], hex) {
            (_, Some(c)) => {
                out.push(c);
                i += 3;
                continue;
            }
            (b'+', _) => out.push(b' '),
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn fetch_page(session: &Session, url: &str) -> Res<ContextPage> {
    let bytes = session.spclient().get_next_page(url).await.map_err(e)?;
    let text = std::str::from_utf8(&bytes).map_err(e)?;
    protobuf_json_mapping::parse_from_str::<ContextPage>(text).map_err(e)
}

/// Resolve any context URI (playlist, album, artist, `spotify:user:<u>:collection`,
/// `spotify:search:<q>`) into a list of playable track URIs.
pub async fn context_track_uris(session: &Session, uri: &str) -> Res<Vec<String>> {
    Ok(context_tracks(session, uri).await?.into_iter().map(|t| t.uri).collect())
}

pub struct CtxTrack {
    pub uri: String,
    /// ms since epoch, 0 if absent
    pub added: u64,
    /// Blend playlists: who the song is for, e.g. "jj, Cube"; else empty.
    pub members: String,
}

/// Names of the blend members a track is attributed to, in Spotify's order.
fn blend_members(meta: &HashMap<String, String>) -> String {
    let Some(json) = meta.get("multiUserAttributionMetadata") else { return String::new() };
    let Ok(v) = serde_json::from_str::<Value>(json) else { return String::new() };
    v["attributed_users"]
        .as_array()
        .map(|users| users.iter().filter_map(|u| u["display_name"].as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Like [`context_track_uris`], with each track's `added_at` and blend members.
pub async fn context_tracks(session: &Session, uri: &str) -> Res<Vec<CtxTrack>> {
    let ctx = session.spclient().get_context(uri).await.map_err(e)?;
    let mut pages: VecDeque<ContextPage> = ctx.pages.into_iter().collect();
    let mut out = Vec::new();
    let mut fetched = 0;

    while let Some(mut page) = pages.pop_front() {
        if out.len() >= MAX_TRACKS {
            break;
        }
        if page.tracks.is_empty() && !page.page_url().is_empty() && fetched < MAX_PAGES {
            fetched += 1;
            match fetch_page(session, page.page_url()).await {
                Ok(p) => page = p,
                Err(_) => continue,
            }
        }
        for t in &page.tracks {
            let u = t.uri();
            if u.starts_with("spotify:track:") {
                // context-resolve sometimes appends a query (`?...`) to track uris
                let added = t.metadata.get("added_at").and_then(|v| v.parse().ok()).unwrap_or(0);
                out.push(CtxTrack {
                    uri: u.split('?').next().unwrap_or(u).to_string(),
                    added,
                    members: blend_members(&t.metadata),
                });
            }
        }
        if !page.next_page_url().is_empty() && fetched < MAX_PAGES {
            fetched += 1;
            if let Ok(p) = fetch_page(session, page.next_page_url()).await {
                pages.push_front(p);
            }
        }
    }
    out.truncate(MAX_TRACKS);
    Ok(out)
}

/// Pick a ~640px (LARGE) cover, falling back to the default ~300px one.
fn cover_url(album: &librespot_protocol::metadata::Album) -> Option<String> {
    use librespot_protocol::metadata::image::Size;
    let imgs: Vec<_> = album
        .cover_group
        .as_ref()
        .map(|g| g.image.iter().collect())
        .filter(|v: &Vec<_>| !v.is_empty())
        .unwrap_or_else(|| album.cover.iter().collect());
    let rank = |i: &&&librespot_protocol::metadata::Image| match i.size() {
        Size::LARGE => 3,
        Size::DEFAULT => 2,
        Size::XLARGE => 1,
        Size::SMALL => 0,
    };
    let best = imgs.iter().max_by_key(rank)?;
    let id = best.file_id();
    if id.is_empty() {
        return None;
    }
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("https://i.scdn.co/image/{hex}"))
}

fn track_row(uri: &str, t: &PTrack) -> Row {
    let artist = t
        .artist
        .iter()
        .map(|a| a.name())
        .collect::<Vec<_>>()
        .join(", ");
    Row {
        uri: uri.to_string(),
        title: t.name().to_string(),
        artist,
        album: t.album.as_ref().map(|a| a.name().to_string()).unwrap_or_default(),
        duration_ms: t.duration().max(0) as u32,
        kind: Kind::Track,
        art: t.album.as_ref().and_then(cover_url),
        added_at: 0,
        members: String::new(),
    }
}

/// Batched extended-metadata of one kind: the raw proto per uri (missing ones left out).
async fn ext_meta(session: &Session, uris: &[String], kind: ExtensionKind) -> HashMap<String, Vec<u8>> {
    let req = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|u| EntityRequest {
                entity_uri: u.clone(),
                query: vec![ExtensionQuery { extension_kind: EnumOrUnknown::new(kind), ..Default::default() }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let mut map = HashMap::with_capacity(uris.len());
    let Ok(resp) = session.spclient().get_extended_metadata(req).await else {
        return map;
    };
    for arr in resp.extended_metadata {
        for d in arr.extension_data {
            if let Some(any) = d.extension_data.into_option() {
                map.insert(d.entity_uri, any.value);
            }
        }
    }
    map
}

async fn metadata_chunk(session: Session, uris: Vec<String>) -> HashMap<String, Row> {
    ext_meta(&session, &uris, ExtensionKind::TRACK_V4)
        .await
        .into_iter()
        .filter_map(|(u, b)| PTrack::parse_from_bytes(&b).ok().map(|t| (u.clone(), track_row(&u, &t))))
        .collect()
}

fn gid_uri(kind: &str, gid: &[u8]) -> Option<String> {
    let id = SpotifyId::from_raw(gid).ok()?.to_base62().ok()?;
    Some(format!("spotify:{kind}:{id}"))
}

/// Metadata of one kind for many uris, 300 per request with 16 in flight; parsed in order.
async fn entities<T: Message>(session: &Session, uris: &[String], kind: ExtensionKind) -> Vec<(String, T)> {
    let maps: Vec<HashMap<String, Vec<u8>>> =
        stream::iter(uris.chunks(META_CHUNK).map(<[String]>::to_vec))
            .map(|c| {
                let s = session.clone();
                async move { ext_meta(&s, &c, kind).await }
            })
            .buffered(16)
            .collect()
            .await;
    let mut all: HashMap<String, Vec<u8>> = maps.into_iter().flatten().collect();
    uris.iter().filter_map(|u| Some((u.clone(), T::parse_from_bytes(&all.remove(u)?).ok()?))).collect()
}

fn album_row(uri: String, a: &PAlbum) -> Row {
    use librespot_protocol::metadata::album::Type;
    let kind = match a.type_() {
        Type::SINGLE => "single",
        Type::EP => "EP",
        Type::COMPILATION => "compilation",
        _ => "album",
    };
    let year = a.date.as_ref().map(|d| d.year()).filter(|&y| y > 0);
    Row {
        uri,
        title: a.name().to_string(),
        artist: a.artist.iter().map(|x| x.name()).collect::<Vec<_>>().join(", "),
        album: year.map_or_else(|| kind.to_string(), |y| format!("{y} · {kind}")),
        duration_ms: 0,
        kind: Kind::Album,
        art: cover_url(a),
        added_at: 0,
        members: String::new(),
    }
}

/// Album rows in the given order (name, artists, "2021 · single" in the album column).
pub async fn album_rows(session: &Session, uris: &[String]) -> Vec<Row> {
    entities::<PAlbum>(session, uris, ExtensionKind::ALBUM_V4).await.into_iter().map(|(u, a)| album_row(u, &a)).collect()
}

/// An artist's page: top tracks, then albums, singles and compilations, newest first as
/// Spotify lists them. Two metadata requests in parallel, after the artist's own.
pub async fn artist_rows(session: &Session, uri: &str) -> Res<Vec<Row>> {
    let raw = ext_meta(session, &[uri.to_string()], ExtensionKind::ARTIST_V4).await.remove(uri).ok_or("artist not found")?;
    let a = PArtist::parse_from_bytes(&raw).map_err(e)?;
    let country = session.country();
    let top: Vec<String> = a
        .top_track
        .iter()
        .find(|t| t.country() == country)
        .or(a.top_track.first())
        .map(|t| t.track.iter().filter_map(|t| gid_uri("track", t.gid())).collect())
        .unwrap_or_default();
    // Each group is one release; its first album is the main edition (the rest are regional).
    let releases: Vec<String> = [&a.album_group, &a.single_group, &a.compilation_group]
        .into_iter()
        .flatten()
        .filter_map(|g| g.album.first())
        .filter_map(|al| gid_uri("album", al.gid()))
        .collect();
    let (mut rows, albums) = tokio::join!(track_rows(session, top), album_rows(session, &releases));
    rows.extend(albums);
    Ok(rows)
}

/// Every track in a list, in order: tracks as they are, albums and playlists opened
/// (8 at a time), duplicates dropped. Plays an artist's whole discography.
pub async fn expand(session: &Session, rows: &[Row]) -> Vec<Row> {
    let items: Vec<(Kind, String)> = rows.iter().map(|r| (r.kind, r.uri.clone())).collect();
    let lists: Vec<Vec<String>> = stream::iter(items)
        .map(|(kind, uri)| {
            let s = session.clone();
            async move {
                match kind {
                    Kind::Track => vec![uri],
                    Kind::Album | Kind::Playlist => context_track_uris(&s, &uri).await.unwrap_or_default(),
                    Kind::Artist => Vec::new(),
                }
            }
        })
        .buffered(8)
        .collect()
        .await;
    let mut seen = std::collections::HashSet::new();
    let uris: Vec<String> = lists.into_iter().flatten().filter(|u| seen.insert(u.clone())).take(MAX_TRACKS).collect();
    track_rows(session, uris).await
}

/// Followed artists, most recently followed first.
async fn followed_artist_rows(session: &Session) -> Res<Vec<Row>> {
    let uris = crate::library::collection(session, "artist").await?;
    Ok(entities::<PArtist>(session, &uris, ExtensionKind::ARTIST_V4)
        .await
        .into_iter()
        .map(|(uri, a)| Row {
            uri,
            title: a.name().to_string(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
            kind: Kind::Artist,
            art: None,
            added_at: 0,
            members: String::new(),
        })
        .collect())
}

/// Turn track URIs into display rows, keeping order. Batched + 4 requests in flight.
pub async fn track_rows(session: &Session, uris: Vec<String>) -> Vec<Row> {
    let chunks: Vec<Vec<String>> = uris.chunks(META_CHUNK).map(<[String]>::to_vec).collect();
    let maps: Vec<HashMap<String, Row>> = stream::iter(chunks)
        .map(|c| metadata_chunk(session.clone(), c))
        .buffered(16)
        .collect()
        .await;
    let mut rows = Vec::with_capacity(uris.len());
    let mut maps = maps.into_iter();
    for chunk in uris.chunks(META_CHUNK) {
        let map = maps.next().unwrap_or_default();
        for u in chunk {
            // tracks with no metadata are region-locked/removed; drop them
            if let Some(r) = map.get(u) {
                rows.push(r.clone());
            }
        }
    }
    rows
}

/// Rows in the context's own order (playlist order, album order…), with `added_at` set.
pub async fn context_rows(session: &Session, uri: &str) -> Res<Vec<Row>> {
    if uri.starts_with("spotify:folder:") {
        return folder_rows(session, uri).await;
    }
    if uri.starts_with("spotify:artist:") {
        return artist_rows(session, uri).await;
    }
    if uri == SAVED_ALBUMS {
        let uris: Vec<String> =
            crate::library::collection(session, "collection").await?.into_iter().filter(|u| u.starts_with("spotify:album:")).collect();
        return Ok(album_rows(session, &uris).await);
    }
    if uri == FOLLOWED_ARTISTS {
        return followed_artist_rows(session).await;
    }
    let tracks = context_tracks(session, uri).await?;
    let uris = tracks.iter().map(|t| t.uri.clone()).collect();
    let mut extra: HashMap<String, (u64, String)> =
        tracks.into_iter().map(|t| (t.uri, (t.added, t.members))).collect();
    let mut rows = track_rows(session, uris).await;
    for r in &mut rows {
        if let Some((added, members)) = extra.get_mut(&r.uri) {
            r.added_at = *added;
            r.members = std::mem::take(members);
        }
    }
    Ok(rows)
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn names(v: &Value) -> String {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Web API image arrays are sorted largest first; take one around 300-640px.
fn image(v: &Value) -> Option<String> {
    let imgs = v.as_array()?;
    imgs.iter()
        .find(|i| i["width"].as_u64().is_some_and(|w| w <= 640))
        .or_else(|| imgs.first())
        .and_then(|i| i["url"].as_str())
        .map(String::from)
}

fn parse_search(v: &Value) -> Vec<Row> {
    let mut rows = Vec::new();
    let items = |k: &str| v[k]["items"].as_array().cloned().unwrap_or_default();
    for t in items("tracks") {
        if let Some(uri) = t["uri"].as_str() {
            rows.push(Row {
                uri: uri.into(),
                title: t["name"].as_str().unwrap_or("").into(),
                artist: names(&t["artists"]),
                album: t["album"]["name"].as_str().unwrap_or("").into(),
                duration_ms: t["duration_ms"].as_u64().unwrap_or(0) as u32,
                kind: Kind::Track,
                art: image(&t["album"]["images"]),
                added_at: 0,
                members: String::new(),
            });
        }
    }
    let mut push = |x: &Value, kind: Kind, sub: String| {
        if let Some(uri) = x["uri"].as_str() {
            rows.push(Row {
                uri: uri.into(),
                title: x["name"].as_str().unwrap_or("").into(),
                artist: sub,
                album: String::new(),
                duration_ms: 0,
                kind,
                art: image(&x["images"]),
                added_at: 0,
                members: String::new(),
            });
        }
    };
    for a in items("albums") {
        push(&a, Kind::Album, names(&a["artists"]));
    }
    for p in items("playlists") {
        // the API returns `null` entries for some playlists
        let owner = p["owner"]["display_name"].as_str().unwrap_or("").to_string();
        push(&p, Kind::Playlist, owner);
    }
    for a in items("artists") {
        push(&a, Kind::Artist, String::new());
    }
    rows
}

async fn web_search(session: &Session, token: &str, q: &str) -> Res<Vec<Row>> {
    let url = format!(
        "https://api.spotify.com/v1/search?q={}&type=track,album,playlist,artist&limit=10",
        pct(q)
    );
    let req = http::Request::get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .body(Bytes::new())
        .map_err(e)?;
    // librespot's http client sleeps on 429 Retry-After; never let that stall search.
    let body = tokio::time::timeout(Duration::from_secs(6), session.http_client().request_body(req))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(e)?;
    let v: Value = serde_json::from_slice(&body).map_err(e)?;
    Ok(parse_search(&v))
}

/// The session-token Web API search shares librespot's client id with every other
/// librespot app, so it is rate limited (429) most of the time. After a failure we stop
/// asking for a while instead of paying for it on every search.
static SESSION_WEB_BLOCKED: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
const WEB_BACKOFF: Duration = Duration::from_secs(120);
/// How long a Web API search may take before we settle for the basic results.
const WEB_BUDGET: Duration = Duration::from_millis(1500);

async fn rich_search(session: &Session, own: Option<Arc<Mutex<WebToken>>>, q: &str) -> Option<(Vec<Row>, &'static str)> {
    if let Some(own) = own {
        let tok = own.lock().await.get().await;
        match tok {
            Ok(t) => match web_search(session, &t, q).await {
                Ok(r) => return Some((r, "web api")),
                Err(err) => log::warn!("own-client search failed: {err}"),
            },
            Err(err) => log::warn!("own-client token failed: {err}"),
        }
    }
    let blocked = SESSION_WEB_BLOCKED.lock().ok()?.is_some_and(|t| t > Instant::now());
    if blocked {
        return None;
    }
    let res = match session.login5().auth_token().await {
        Ok(t) => web_search(session, &t.access_token, q).await,
        Err(err) => Err(err.to_string()),
    };
    match res {
        Ok(r) => Some((r, "web api*")),
        Err(err) => {
            log::warn!("session-token search failed: {err}");
            if let Ok(mut b) = SESSION_WEB_BLOCKED.lock() {
                *b = Some(Instant::now() + WEB_BACKOFF);
            }
            None
        }
    }
}

/// Search: the Web API (own app if configured, else the session token) gives tracks,
/// albums, artists and playlists; context-resolve search gives tracks only but needs no
/// token and isn't rate limited. Both run at once; the richer one wins if it's quick.
pub async fn search(
    session: &Session,
    own: Option<Arc<Mutex<WebToken>>>,
    q: &str,
) -> Res<(Vec<Row>, &'static str)> {
    // Once basic results are in, the Web API only gets this much longer.
    const GRACE: Duration = Duration::from_millis(250);
    let rich = tokio::time::timeout(WEB_BUDGET, rich_search(session, own, q));
    let basic = async {
        let uri = format!("spotify:search:{}", pct(q));
        let mut uris = context_track_uris(session, &uri).await?;
        uris.truncate(50);
        Ok::<_, String>((track_rows(session, uris).await, "basic"))
    };
    tokio::pin!(rich, basic);
    let (rich, basic) = tokio::select! {
        r = &mut rich => (r, basic.await),
        b = &mut basic => (tokio::time::timeout(GRACE, &mut rich).await.unwrap_or_else(Err), b),
    };
    match rich {
        Ok(Some(r)) if !r.0.is_empty() => Ok(r),
        Ok(_) => basic,
        Err(_) => {
            log::warn!("web search too slow, using basic results");
            if let Ok(mut b) = SESSION_WEB_BLOCKED.lock() {
                *b = Some(Instant::now() + WEB_BACKOFF);
            }
            basic
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_rows_show_year_and_type() {
        use librespot_protocol::metadata::{Date, album::Type};
        let mut a = PAlbum::new();
        a.set_name("Is This It".into());
        a.set_type(Type::SINGLE);
        let mut d = Date::new();
        d.set_year(2001);
        a.date = protobuf::MessageField::some(d);
        let mut ar = PArtist::new();
        ar.set_name("The Strokes".into());
        a.artist.push(ar);
        let r = album_row("spotify:album:x".into(), &a);
        assert_eq!((r.title.as_str(), r.artist.as_str(), r.album.as_str(), r.kind), ("Is This It", "The Strokes", "2001 · single", Kind::Album));
        a.date = protobuf::MessageField::none();
        a.set_type(Type::ALBUM);
        assert_eq!(album_row("spotify:album:x".into(), &a).album, "album");
    }

    #[test]
    fn pct_encodes_query() {
        assert_eq!(pct("never gonna"), "never+gonna");
        assert_eq!(pct("AC/DC & co"), "AC%2FDC+%26+co");
        assert_eq!(pct("é"), "%C3%A9");
    }

    #[test]
    fn decodes_folder_names() {
        assert_eq!(url_decode("My+Mix%21"), "My Mix!");
        assert_eq!(url_decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode(&pct("AC/DC & co")), "AC/DC & co");
    }

    #[test]
    fn parses_web_search_including_null_playlists() {
        let v: Value = serde_json::from_str(r#"{
          "tracks": {"items": [{"uri":"spotify:track:1","name":"Song","duration_ms":201000,
                     "artists":[{"name":"A"},{"name":"B"}],"album":{"name":"Alb"}}]},
          "albums": {"items": [{"uri":"spotify:album:2","name":"Alb","artists":[{"name":"A"}]}]},
          "playlists": {"items": [null, {"uri":"spotify:playlist:3","name":"Mix","owner":{"display_name":"me"}}]},
          "artists": {"items": [{"uri":"spotify:artist:4","name":"A"}]}
        }"#).unwrap();
        let rows = parse_search(&v);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, Kind::Track);
        assert_eq!(rows[0].artist, "A, B");
        assert_eq!(rows[0].duration_ms, 201000);
        assert_eq!(rows[1].kind, Kind::Album);
        assert_eq!(rows[2].kind, Kind::Playlist);
        assert_eq!(rows[2].artist, "me");
        assert_eq!(rows[3].kind, Kind::Artist);
    }
}
