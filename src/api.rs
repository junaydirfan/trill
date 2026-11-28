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
    /// All-time plays as Spotify shows them on album and artist pages; 0 = unknown.
    pub plays: u64,
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
            plays: 0,
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
        // A page that fails (librespot has retried it already) fails the whole list: a
        // shortened one would replace the full copy on screen and on disk.
        if page.tracks.is_empty() && !page.page_url().is_empty() && fetched < MAX_PAGES {
            fetched += 1;
            page = fetch_page(session, page.page_url()).await?;
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
            pages.push_front(fetch_page(session, page.next_page_url()).await?);
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
        plays: 0,
    }
}

/// Batched extended-metadata of one kind: the raw proto per uri (missing ones left out).
/// A failed request is an error, not an empty answer, so it can't quietly drop songs from a list.
async fn ext_meta(session: &Session, uris: &[String], kind: ExtensionKind) -> Res<HashMap<String, Vec<u8>>> {
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
    let resp = session.spclient().get_extended_metadata(req).await.map_err(e)?;
    for arr in resp.extended_metadata {
        for d in arr.extension_data {
            if let Some(any) = d.extension_data.into_option() {
                map.insert(d.entity_uri, any.value);
            }
        }
    }
    Ok(map)
}

/// Spotify's web-player GraphQL ("pathfinder"), the only place play counts come from.
/// shortcut: the persisted-query hashes are copied from open.spotify.com's bundle (2026-11);
/// when Spotify retires one the request fails and plays just stay hidden. Re-copy them then.
const GQL_ALBUM: (&str, &str) = ("getAlbum", "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f");
const GQL_ARTIST: (&str, &str) = ("queryArtistOverview", "eafb1620d783ece118e72fbfede2b0064483e029d3c1aafafa1c2de0e25a89e7");

async fn gql(session: &Session, (op, hash): (&str, &str), vars: Value) -> Res<Value> {
    let token = session.login5().auth_token().await.map_err(e)?.access_token;
    let client = session.spclient().client_token().await.map_err(e)?;
    let body = serde_json::json!({
        "variables": vars,
        "operationName": op,
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
    });
    let req = http::Request::post("https://api-partner.spotify.com/pathfinder/v2/query")
        .header("Authorization", format!("Bearer {token}"))
        .header("client-token", client)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Bytes::from(body.to_string()))
        .map_err(e)?;
    let body = tokio::time::timeout(Duration::from_secs(8), session.http_client().request_body(req))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(e)?;
    serde_json::from_slice(&body).map_err(e)
}

/// Play count per track uri, from a list of GraphQL track items. Relinked tracks are listed
/// under both uris, since the list may hold either.
fn plays_of(items: &Value, out: &mut HashMap<String, u64>) {
    for it in items.as_array().into_iter().flatten() {
        let t = &it["track"];
        let Some(n) = t["playcount"].as_str().and_then(|p| p.parse().ok()) else { continue };
        for u in [&t["uri"], &t["relinkingInformation"]["linkedTrack"]["uri"]] {
            if let Some(u) = u.as_str() {
                out.insert(u.to_string(), n);
            }
        }
    }
}

/// Play counts for an album's tracks, or an artist's top tracks.
async fn play_counts(session: &Session, uri: &str) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let res = if uri.starts_with("spotify:artist:") {
        gql(session, GQL_ARTIST, serde_json::json!({ "uri": uri, "locale": "", "includePrerelease": false }))
            .await
            .map(|v| plays_of(&v["data"]["artistUnion"]["discography"]["topTracks"]["items"], &mut out))
    } else {
        // albums hold at most a few hundred tracks; 300 per page covers nearly all in one request
        let mut offset = 0;
        loop {
            let v = match gql(session, GQL_ALBUM, serde_json::json!({ "uri": uri, "locale": "", "offset": offset, "limit": 300 })).await {
                Ok(v) => v,
                Err(err) => break Err(err),
            };
            let tracks = &v["data"]["albumUnion"]["tracksV2"];
            let n = tracks["items"].as_array().map_or(0, Vec::len);
            plays_of(&tracks["items"], &mut out);
            offset += n;
            if n == 0 || offset as u64 >= tracks["totalCount"].as_u64().unwrap_or(0) {
                break Ok(());
            }
        }
    };
    if let Err(err) = res {
        log::warn!("play counts for {uri}: {err}");
    }
    out
}

fn set_plays(rows: &mut [Row], plays: &HashMap<String, u64>) {
    for r in rows {
        if let Some(&n) = plays.get(&r.uri) {
            r.plays = n;
        }
    }
}

async fn metadata_chunk(session: Session, uris: Vec<String>) -> Res<HashMap<String, Row>> {
    Ok(ext_meta(&session, &uris, ExtensionKind::TRACK_V4)
        .await?
        .into_iter()
        .filter_map(|(u, b)| PTrack::parse_from_bytes(&b).ok().map(|t| (u.clone(), track_row(&u, &t))))
        .collect())
}

/// Where a song lives: its album (uri, "Album — Artist") or, with `artist`, its first
/// artist (uri, name).
pub async fn track_home(session: &Session, uri: &str, artist: bool) -> Res<(String, String)> {
    let bytes = ext_meta(session, &[uri.to_string()], ExtensionKind::TRACK_V4).await?.remove(uri).ok_or("Spotify has no details for this song")?;
    let t = PTrack::parse_from_bytes(&bytes).map_err(e)?;
    let first = t.artist.first();
    if artist {
        let a = first.ok_or("this song has no artist")?;
        return Ok((gid_uri("artist", a.gid()).ok_or("bad artist id")?, a.name().to_string()));
    }
    let al = t.album.as_ref().ok_or("this song has no album")?;
    let by = al.artist.first().or(first).map(|a| a.name()).unwrap_or_default();
    let title = if by.is_empty() { al.name().to_string() } else { format!("{} — {by}", al.name()) };
    Ok((gid_uri("album", al.gid()).ok_or("bad album id")?, title))
}

fn gid_uri(kind: &str, gid: &[u8]) -> Option<String> {
    let id = SpotifyId::from_raw(gid).ok()?.to_base62().ok()?;
    Some(format!("spotify:{kind}:{id}"))
}

/// Metadata of one kind for many uris, 300 per request with 16 in flight; parsed in order.
async fn entities<T: Message>(session: &Session, uris: &[String], kind: ExtensionKind) -> Res<Vec<(String, T)>> {
    let maps: Vec<Res<HashMap<String, Vec<u8>>>> =
        stream::iter(uris.chunks(META_CHUNK).map(<[String]>::to_vec))
            .map(|c| {
                let s = session.clone();
                async move { ext_meta(&s, &c, kind).await }
            })
            .buffered(16)
            .collect()
            .await;
    let mut all = HashMap::new();
    for m in maps {
        all.extend(m?);
    }
    Ok(uris.iter().filter_map(|u| Some((u.clone(), T::parse_from_bytes(&all.remove(u)?).ok()?))).collect())
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
        plays: 0,
    }
}

/// Album rows in the given order (name, artists, "2021 · single" in the album column).
pub async fn album_rows(session: &Session, uris: &[String]) -> Res<Vec<Row>> {
    Ok(entities::<PAlbum>(session, uris, ExtensionKind::ALBUM_V4).await?.into_iter().map(|(u, a)| album_row(u, &a)).collect())
}

/// An artist's page: top tracks, then albums, singles and compilations, newest first as
/// Spotify lists them. Two metadata requests in parallel, after the artist's own.
pub async fn artist_rows(session: &Session, uri: &str) -> Res<Vec<Row>> {
    let raw = ext_meta(session, &[uri.to_string()], ExtensionKind::ARTIST_V4).await?.remove(uri).ok_or("artist not found")?;
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
    let (rows, albums, plays) = tokio::join!(track_rows(session, top), album_rows(session, &releases), play_counts(session, uri));
    let mut rows = rows?;
    set_plays(&mut rows, &plays);
    rows.extend(albums?);
    Ok(rows)
}

/// Every track in a list, in order: tracks as they are, albums and playlists opened
/// (8 at a time), duplicates dropped. Plays an artist's whole discography.
pub async fn expand(session: &Session, rows: &[Row]) -> Res<Vec<Row>> {
    let items: Vec<(Kind, String)> = rows.iter().map(|r| (r.kind, r.uri.clone())).collect();
    let lists: Vec<Res<Vec<String>>> = stream::iter(items)
        .map(|(kind, uri)| {
            let s = session.clone();
            async move {
                match kind {
                    Kind::Track => Ok(vec![uri]),
                    Kind::Album | Kind::Playlist => context_track_uris(&s, &uri).await,
                    Kind::Artist => Ok(Vec::new()),
                }
            }
        })
        .buffered(8)
        .collect()
        .await;
    let mut seen = std::collections::HashSet::new();
    let mut uris = Vec::new();
    for list in lists {
        uris.extend(list?.into_iter().filter(|u| seen.insert(u.clone())));
    }
    uris.truncate(MAX_TRACKS);
    track_rows(session, uris).await
}

/// Followed artists, most recently followed first.
async fn followed_artist_rows(session: &Session) -> Res<Vec<Row>> {
    let uris = crate::library::collection(session, "artist").await?;
    Ok(entities::<PArtist>(session, &uris, ExtensionKind::ARTIST_V4)
        .await?
        .into_iter()
        .map(|(uri, a)| artist_row(uri, a.name()))
        .collect())
}

fn artist_row(uri: String, name: &str) -> Row {
    Row {
        uri,
        title: name.to_string(),
        artist: String::new(),
        album: String::new(),
        duration_ms: 0,
        kind: Kind::Artist,
        art: None,
        added_at: 0,
        members: String::new(),
        plays: 0,
    }
}

/// Turn track URIs into display rows, keeping order. Batched, 16 requests in flight.
pub async fn track_rows(session: &Session, uris: Vec<String>) -> Res<Vec<Row>> {
    let chunks: Vec<Vec<String>> = uris.chunks(META_CHUNK).map(<[String]>::to_vec).collect();
    let maps: Vec<Res<HashMap<String, Row>>> = stream::iter(chunks)
        .map(|c| metadata_chunk(session.clone(), c))
        .buffered(16)
        .collect()
        .await;
    let mut rows = Vec::with_capacity(uris.len());
    let mut maps = maps.into_iter();
    for chunk in uris.chunks(META_CHUNK) {
        let map = maps.next().transpose()?.unwrap_or_default();
        for u in chunk {
            // tracks with no metadata are region-locked/removed; drop them
            if let Some(r) = map.get(u) {
                rows.push(r.clone());
            }
        }
    }
    Ok(rows)
}

/// Songs Spotify recommends for a list (its radio station), for smart shuffle. Seeded by
/// the list (playlists, albums, artists), else by `track` (Liked Songs, search). Up to 50.
pub async fn recommendations(session: &Session, list: &str, track: &str) -> Vec<Row> {
    let mut uris = if list.starts_with("spotify:") { station(session, list).await } else { Vec::new() };
    if uris.is_empty() {
        uris = station(session, track).await;
    }
    track_rows(session, uris).await.unwrap_or_default()
}

/// Up to 50 songs of Spotify's radio for a list or a song (empty if it has none).
pub async fn station(session: &Session, seed: &str) -> Vec<String> {
    let scope = if seed.starts_with("spotify:track:") { "tracks" } else { "stations" };
    let Ok(body) = session.spclient().get_apollo_station(scope, seed, Some(50), Vec::new(), false).await else {
        return Vec::new();
    };
    let v: Value = serde_json::from_slice(&body).unwrap_or_default();
    station_uris(&v)
}

/// Each song's first artist, as (artist uri, name), from one batched metadata request.
pub async fn track_artists(session: &Session, uris: &[String]) -> Res<HashMap<String, (String, String)>> {
    Ok(entities::<PTrack>(session, uris, ExtensionKind::TRACK_V4)
        .await?
        .into_iter()
        .filter_map(|(u, t)| {
            let a = t.artist.first()?;
            Some((u, (gid_uri("artist", a.gid())?, a.name().to_string())))
        })
        .collect())
}

/// What an artist's page lists besides their music: related artists (uri, name), and the
/// uris of their top songs. The same GraphQL query the artist page uses for play counts.
pub async fn artist_overview(session: &Session, uri: &str) -> Res<(Vec<(String, String)>, Vec<String>)> {
    let v = gql(session, GQL_ARTIST, serde_json::json!({ "uri": uri, "locale": "", "includePrerelease": false })).await?;
    Ok(overview_of(&v["data"]["artistUnion"]))
}

fn overview_of(a: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let items = |v: &Value| v["items"].as_array().cloned().unwrap_or_default();
    let related = items(&a["relatedContent"]["relatedArtists"])
        .iter()
        .filter_map(|r| Some((r["uri"].as_str()?.to_string(), r["profile"]["name"].as_str().unwrap_or("").to_string())))
        .collect();
    let top = items(&a["discography"]["topTracks"])
        .iter()
        .filter_map(|t| t["track"]["uri"].as_str().filter(|u| u.starts_with("spotify:track:")).map(String::from))
        .collect();
    (related, top)
}

/// The header over an open album or artist: a picture, a line of facts and, for an artist,
/// the biography Spotify shows on their page (copyright lines for an album).
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Info {
    pub image: Option<String>,
    pub facts: String,
    pub about: String,
}

pub async fn info(session: &Session, uri: &str) -> Res<Info> {
    if uri.starts_with("spotify:artist:") {
        let v = gql(session, GQL_ARTIST, serde_json::json!({ "uri": uri, "locale": "", "includePrerelease": false })).await?;
        return Ok(artist_info(&v["data"]["artistUnion"]));
    }
    let (_, a) = entities::<PAlbum>(session, &[uri.to_string()], ExtensionKind::ALBUM_V4).await?.pop().ok_or("album not found")?;
    Ok(album_info(&a))
}

fn album_info(a: &PAlbum) -> Info {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let kind = album_row(String::new(), a).album; // "2001 · single"
    let date = a.date.as_ref().filter(|d| d.year() > 0).map(|d| match (d.day(), MONTHS.get((d.month() - 1).max(0) as usize)) {
        (day, Some(m)) if d.month() > 0 && day > 0 => format!("{day} {m} {}", d.year()),
        (_, Some(m)) if d.month() > 0 => format!("{m} {}", d.year()),
        _ => d.year().to_string(),
    });
    // "2001 · album" → "album · 12 Mar 2001": the full date replaces the bare year
    let kind = kind.rsplit(" · ").next().unwrap_or_default().to_string();
    let facts = [Some(kind), date, Some(a.label().to_string()).filter(|l| !l.is_empty())].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let about = a.copyright.iter().map(|c| c.text().trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ");
    Info { image: cover_url(a), facts, about }
}

fn artist_info(a: &Value) -> Info {
    let num = |v: &Value| v.as_u64().filter(|&n| n > 0);
    let s = &a["stats"];
    let facts = [
        num(&s["monthlyListeners"]).map(|n| format!("{} monthly listeners", short_count(n))),
        num(&s["followers"]).map(|n| format!("{} followers", short_count(n))),
        num(&s["worldRank"]).map(|n| format!("#{n} in the world")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let image = a["visuals"]["avatarImage"]["sources"]
        .as_array()
        .and_then(|v| v.iter().max_by_key(|i| i["width"].as_u64().unwrap_or(0)))
        .and_then(|i| i["url"].as_str())
        .map(String::from);
    Info { image, facts, about: strip_html(a["profile"]["biography"]["text"].as_str().unwrap_or_default()) }
}

/// 1234567 → "1.2M", 45300 → "45.3K".
fn short_count(n: u64) -> String {
    match n {
        1_000_000.. => format!("{:.1}M", n as f64 / 1e6),
        1_000.. => format!("{:.1}K", n as f64 / 1e3),
        _ => n.to_string(),
    }
}

/// Biographies link names (`<a href="spotify:artist:…">Name</a>`) and escape a few characters.
fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ")
}

fn station_uris(v: &Value) -> Vec<String> {
    v["tracks"].as_array().into_iter().flatten().filter_map(|t| t["uri"].as_str()).filter(|u| u.starts_with("spotify:track:")).map(String::from).collect()
}

/// Rows in the context's own order (playlist order, album order…), with `added_at` set.
/// `known`: rows this list had before (from the disk cache or memory); their tracks' details
/// are reused, so a refresh only fetches metadata for songs added since.
pub async fn context_rows(session: &Session, uri: &str, known: &[Row]) -> Res<Vec<Row>> {
    if uri.starts_with("spotify:folder:") {
        return folder_rows(session, uri).await;
    }
    if uri.starts_with("spotify:artist:") {
        return artist_rows(session, uri).await;
    }
    if uri == SAVED_ALBUMS {
        let uris: Vec<String> =
            crate::library::collection(session, "collection").await?.into_iter().filter(|u| u.starts_with("spotify:album:")).collect();
        return album_rows(session, &uris).await;
    }
    if uri == FOLLOWED_ARTISTS {
        return followed_artist_rows(session).await;
    }
    let album = uri.starts_with("spotify:album:");
    let (tracks, plays) = tokio::join!(context_tracks(session, uri), async {
        if album { play_counts(session, uri).await } else { HashMap::new() }
    });
    let tracks = tracks?;
    let have: HashMap<&str, &Row> = known.iter().filter(|r| r.kind == Kind::Track).map(|r| (r.uri.as_str(), r)).collect();
    let mut missing: Vec<String> = tracks.iter().filter(|t| !have.contains_key(t.uri.as_str())).map(|t| t.uri.clone()).collect();
    missing.dedup();
    let fetched: HashMap<String, Row> = track_rows(session, missing).await?.into_iter().map(|r| (r.uri.clone(), r)).collect();
    // tracks with no metadata are region-locked/removed; they stay dropped
    let mut rows: Vec<Row> = tracks
        .into_iter()
        .filter_map(|t| {
            let mut r = have.get(t.uri.as_str()).map(|r| (*r).clone()).or_else(|| fetched.get(&t.uri).cloned())?;
            r.added_at = t.added;
            r.members = t.members;
            Some(r)
        })
        .collect();
    set_plays(&mut rows, &plays);
    Ok(rows)
}

/// Lists as last loaded, one file each, so they show instantly after a restart while a fresh
/// copy loads in the background. Tab-separated like the playlist cache; a header names the format.
/// shortcut: files are never pruned (~3 KB per album, ~0.6 MB for 4k liked songs); add an
/// age-based sweep if the folder ever gets big.
const LIST_FORMAT: &str = "trill-list 1";

fn list_path(dir: &std::path::Path, uri: &str) -> std::path::PathBuf {
    dir.join("lists").join(uri.replace([':', '/', '\\'], "_"))
}

/// How long ago the list was saved.
pub fn list_age(dir: &std::path::Path, uri: &str) -> Option<Duration> {
    std::fs::metadata(list_path(dir, uri)).and_then(|m| m.modified()).ok()?.elapsed().ok()
}

pub fn read_list_cache(dir: &std::path::Path, uri: &str) -> Option<Vec<Row>> {
    let text = std::fs::read_to_string(list_path(dir, uri)).ok()?;
    let mut lines = text.lines();
    if lines.next() != Some(LIST_FORMAT) {
        return None;
    }
    Some(lines.filter_map(parse_list_line).collect())
}

fn parse_list_line(l: &str) -> Option<Row> {
    let mut f = l.split('\t');
    let mut next = || f.next();
    let (uri, title, artist, album) = (next()?, next()?, next()?, next()?);
    let (duration_ms, kind, art, added_at, members) = (next()?.parse().ok()?, next()?, next()?, next()?.parse().ok()?, next()?);
    let plays = next().and_then(|p| p.parse().ok()).unwrap_or(0); // absent in files from before 0.3.8
    let kind = match kind {
        "t" => Kind::Track,
        "a" => Kind::Album,
        "p" => Kind::Playlist,
        "r" => Kind::Artist,
        _ => return None,
    };
    Some(Row {
        uri: uri.into(),
        title: title.into(),
        artist: artist.into(),
        album: album.into(),
        duration_ms,
        kind,
        art: (!art.is_empty()).then(|| art.into()),
        added_at,
        members: members.into(),
        plays,
    })
}

pub fn write_list_cache(dir: &std::path::Path, uri: &str, rows: &[Row]) {
    use std::fmt::Write as _;
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut text = String::with_capacity(rows.len() * 160 + 16);
    text.push_str(LIST_FORMAT);
    text.push('\n');
    for r in rows {
        let kind = match r.kind {
            Kind::Track => "t",
            Kind::Album => "a",
            Kind::Playlist => "p",
            Kind::Artist => "r",
        };
        let _ = writeln!(
            text,
            "{}\t{}\t{}\t{}\t{}\t{kind}\t{}\t{}\t{}\t{}",
            r.uri,
            clean(&r.title),
            clean(&r.artist),
            clean(&r.album),
            r.duration_ms,
            r.art.as_deref().unwrap_or(""),
            r.added_at,
            clean(&r.members),
            r.plays
        );
    }
    let path = list_path(dir, uri);
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp"); // not with_extension: user ids in the name can contain dots
    let tmp = std::path::PathBuf::from(tmp);
    if std::fs::create_dir_all(dir.join("lists")).is_ok() && std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
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
                plays: 0,
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
                plays: 0,
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
/// (blocked until, failures in a row): the pause doubles per failure, 2 min up to 32 min,
/// since a token that's rate limited now usually still is a few minutes later.
static SESSION_WEB_BLOCKED: std::sync::Mutex<(Option<Instant>, u32)> = std::sync::Mutex::new((None, 0));

fn web_backoff(failures: u32) -> Duration {
    Duration::from_secs(120 << failures.saturating_sub(1).min(4))
}

fn session_web_failed() {
    if let Ok(mut b) = SESSION_WEB_BLOCKED.lock() {
        b.1 += 1;
        b.0 = Some(Instant::now() + web_backoff(b.1));
    }
}
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
    let blocked = SESSION_WEB_BLOCKED.lock().ok()?.0.is_some_and(|t| t > Instant::now());
    if blocked {
        return None;
    }
    let res = match session.login5().auth_token().await {
        Ok(t) => web_search(session, &t.access_token, q).await,
        Err(err) => Err(err.to_string()),
    };
    match res {
        Ok(r) => {
            if let Ok(mut b) = SESSION_WEB_BLOCKED.lock() {
                *b = (None, 0);
            }
            Some((r, "web api*"))
        }
        Err(err) => {
            log::warn!("session-token search failed: {err}");
            session_web_failed();
            None
        }
    }
}

/// Search without the Web API: context-resolve gives up to 50 matching tracks, and their
/// own metadata names the albums and artists, so those come at no extra request.
/// shortcut: no playlists; they need the Web API (`client_id`).
async fn basic_search(session: &Session, q: &str) -> Res<Vec<Row>> {
    let mut uris = context_track_uris(session, &format!("spotify:search:{}", pct(q))).await?;
    uris.truncate(50);
    let tracks = entities::<PTrack>(session, &uris, ExtensionKind::TRACK_V4).await?;
    Ok(search_rows(&tracks))
}

/// The tracks, then up to 10 albums and 10 artists, each in the order the tracks first name it.
fn search_rows(tracks: &[(String, PTrack)]) -> Vec<Row> {
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<Row> = tracks.iter().map(|(u, t)| track_row(u, t)).collect();
    let albums: Vec<Row> = tracks
        .iter()
        .filter_map(|(_, t)| Some((gid_uri("album", t.album.as_ref()?.gid())?, t.album.as_ref()?)))
        .filter(|(u, _)| seen.insert(u.clone()))
        .take(10)
        .map(|(u, a)| album_row(u, a))
        .collect();
    let artists: Vec<Row> = tracks
        .iter()
        .flat_map(|(_, t)| &t.artist)
        .filter_map(|a| Some((gid_uri("artist", a.gid())?, a.name())))
        .filter(|(u, _)| seen.insert(u.clone()))
        .take(10)
        .map(|(u, name)| artist_row(u, name))
        .collect();
    rows.extend(albums);
    rows.extend(artists);
    rows
}

/// Search: the Web API (own app if configured, else the session token) gives tracks,
/// albums, artists and playlists; context-resolve search gives tracks, albums and artists but needs no
/// token and isn't rate limited. Both run at once; the richer one wins if it's quick.
pub async fn search(
    session: &Session,
    own: Option<Arc<Mutex<WebToken>>>,
    q: &str,
) -> Res<(Vec<Row>, &'static str)> {
    // Once basic results are in, the Web API only gets this much longer.
    const GRACE: Duration = Duration::from_millis(250);
    let rich = tokio::time::timeout(WEB_BUDGET, rich_search(session, own, q));
    let basic = async { Ok::<_, String>((basic_search(session, q).await?, "basic")) };
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
            session_web_failed();
            basic
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_station_tracks() {
        let v: Value = serde_json::from_str(
            r#"{"tracks":[{"uri":"spotify:track:a"},{"uri":"spotify:episode:b"},{"name":"no uri"},{"uri":"spotify:track:c"}]}"#,
        )
        .unwrap();
        assert_eq!(station_uris(&v), ["spotify:track:a", "spotify:track:c"]);
        assert!(station_uris(&Value::Null).is_empty());
    }

    #[test]
    fn reads_artist_overview() {
        let v: Value = serde_json::from_str(r#"{
          "relatedContent": {"relatedArtists": {"items": [
            {"uri": "spotify:artist:a", "profile": {"name": "Nile Rodgers"}}, {"profile": {"name": "no uri"}}]}},
          "discography": {"topTracks": {"items": [
            {"track": {"uri": "spotify:track:1", "playcount": "5"}}, {"track": {"uri": "spotify:episode:2"}}]}}
        }"#).unwrap();
        let (related, top) = overview_of(&v);
        assert_eq!(related, [("spotify:artist:a".to_string(), "Nile Rodgers".to_string())]);
        assert_eq!(top, ["spotify:track:1"]);
        assert_eq!(overview_of(&Value::Null), (Vec::new(), Vec::new()));
    }

    #[test]
    fn reads_artist_info() {
        let v: Value = serde_json::from_str(r#"{
          "profile": {"biography": {"text": "Led by <a href=\"spotify:artist:x\">Julian</a> &amp; co."}},
          "stats": {"followers": 4567000, "monthlyListeners": 12345, "worldRank": 0},
          "visuals": {"avatarImage": {"sources": [{"url": "s", "width": 160}, {"url": "l", "width": 640}]}}
        }"#).unwrap();
        let i = artist_info(&v);
        assert_eq!(i.about, "Led by Julian & co.");
        assert_eq!(i.facts, "12.3K monthly listeners · 4.6M followers");
        assert_eq!(i.image.as_deref(), Some("l"));
        assert_eq!(artist_info(&Value::Null), Info::default());
    }

    #[test]
    fn album_info_has_date_label_and_copyright() {
        use librespot_protocol::metadata::{Copyright, Date};
        let mut a = PAlbum::new();
        a.set_label("RCA".into());
        let mut d = Date::new();
        d.set_year(2001);
        d.set_month(7);
        d.set_day(30);
        a.date = protobuf::MessageField::some(d);
        let mut c = Copyright::new();
        c.set_text("(P) 2001 RCA".into());
        a.copyright.push(c);
        let i = album_info(&a);
        assert_eq!((i.facts.as_str(), i.about.as_str()), ("album · 30 Jul 2001 · RCA", "(P) 2001 RCA"));
        a.date.as_mut().unwrap().set_day(0);
        assert_eq!(album_info(&a).facts, "album · Jul 2001 · RCA");
    }

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
    fn list_cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("trill-list-test-{}", std::process::id()));
        let row = |uri: &str, title: &str, kind, art: Option<&str>| Row {
            uri: uri.into(),
            title: title.into(),
            artist: "A, B".into(),
            album: "2001 · single".into(),
            duration_ms: 123_456,
            kind,
            art: art.map(String::from),
            added_at: 1_700_000_000_000,
            members: "jj, Cube".into(),
            plays: 1_204_589_367,
        };
        let rows = vec![
            row("spotify:track:1", "tab\there", Kind::Track, Some("https://i.scdn.co/image/ab")),
            row("spotify:album:2", "no art", Kind::Album, None),
            row("spotify:playlist:3", "p", Kind::Playlist, None),
            row("spotify:artist:4", "r", Kind::Artist, None),
        ];
        let uri = "spotify:user:a.b.c:collection";
        write_list_cache(&dir, uri, &rows);
        let back = read_list_cache(&dir, uri).expect("cached");
        assert_eq!(back.len(), 4);
        assert_eq!(back[0].title, "tab here"); // tabs would break the format
        for (a, b) in back.iter().zip(&rows).skip(1) {
            assert_eq!(
                (&a.uri, &a.title, &a.artist, &a.album, a.duration_ms, a.kind, &a.art, a.added_at, &a.members, a.plays),
                (&b.uri, &b.title, &b.artist, &b.album, b.duration_ms, b.kind, &b.art, b.added_at, &b.members, b.plays)
            );
        }
        // lines written before play counts existed still load, with none
        let old = parse_list_line("spotify:track:9\tt\ta\tal\t1000\tt\t\t0\t").expect("old line");
        assert_eq!(old.plays, 0);
        // another format version is ignored, not misread
        std::fs::write(list_path(&dir, uri), "trill-list 0\nx").unwrap();
        assert!(read_list_cache(&dir, uri).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_backoff_doubles_to_a_cap() {
        let mins: Vec<u64> = (1..=7).map(|n| web_backoff(n).as_secs() / 60).collect();
        assert_eq!(mins, [2, 4, 8, 16, 32, 32, 32]);
    }

    #[test]
    fn basic_search_lists_each_album_and_artist_once() {
        let track = |name: &str, album: u8, artists: &[u8]| {
            let mut t = PTrack::new();
            t.set_name(name.into());
            let mut a = PAlbum::new();
            a.set_gid(vec![album; 16]);
            a.set_name(format!("album {album}"));
            t.album = Some(a).into();
            t.artist = artists
                .iter()
                .map(|&x| {
                    let mut p = PArtist::new();
                    p.set_gid(vec![x; 16]);
                    p.set_name(format!("artist {x}"));
                    p
                })
                .collect();
            (format!("spotify:track:{name}"), t)
        };
        let rows = search_rows(&[track("a", 1, &[7]), track("b", 2, &[7, 8]), track("c", 1, &[8])]);
        let got: Vec<_> = rows.iter().map(|r| (r.kind, r.title.as_str())).collect();
        assert_eq!(got, [
            (Kind::Track, "a"), (Kind::Track, "b"), (Kind::Track, "c"),
            (Kind::Album, "album 1"), (Kind::Album, "album 2"),
            (Kind::Artist, "artist 7"), (Kind::Artist, "artist 8"),
        ]);
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
