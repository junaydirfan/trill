//! Changes to the user's library (liking songs, adding to / removing from playlists) and
//! lyrics. Like the rest of trill this goes through librespot's spclient session, the
//! same private API the official apps use, so no Web API quota or rate limit applies.

use std::time::{SystemTime, UNIX_EPOCH};

use http::{HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use librespot_core::{Error, Session, SpotifyId, error::ErrorKind};
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, Delta, Item, ItemAttributes, ListChanges, Op, Rem, SelectedListContent, op::Kind,
};
use protobuf::{CodedInputStream, CodedOutputStream, EnumOrUnknown, Message, MessageField};
use serde_json::Value;

type Res<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn proto_headers(content_type: &'static str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    h
}

// ---------------------------------------------------------------- liked songs

/// collection2v2 `WriteRequest` (not compiled into librespot-protocol; it's 3 fields):
/// { 1: username, 2: set, 3: repeated CollectionItem { 1: uri, 2: added_at s, 3: is_removed } }
fn collection_write(user: &str, uri: &str, liked: bool) -> Res<Vec<u8>> {
    // Followed artists have their own set; liked songs and saved albums share "collection".
    let set = if uri.starts_with("spotify:artist:") { "artist" } else { "collection" };
    let mut item = Vec::new();
    {
        let mut os = CodedOutputStream::vec(&mut item);
        os.write_string(1, uri).map_err(e)?;
        os.write_int32(2, (now_ms() / 1000) as i32).map_err(e)?;
        if !liked {
            os.write_bool(3, true).map_err(e)?;
        }
        os.flush().map_err(e)?;
    }
    let mut body = Vec::new();
    {
        let mut os = CodedOutputStream::vec(&mut body);
        os.write_string(1, user).map_err(e)?;
        os.write_string(2, set).map_err(e)?;
        os.write_bytes(3, &item).map_err(e)?;
        os.flush().map_err(e)?;
    }
    Ok(body)
}

/// Like (`true`) or unlike a track, save or unsave an album, follow or unfollow an artist.
pub async fn set_liked(session: &Session, uri: &str, liked: bool) -> Res<()> {
    let body = collection_write(&session.username(), uri, liked)?;
    session
        .spclient()
        .request(
            &Method::POST,
            "/collection/v2/write",
            Some(proto_headers("application/vnd.collection-v2.spotify.proto")),
            Some(&body),
        )
        .await
        .map_err(e)?;
    Ok(())
}

/// Items pinned to the top of Your Library (playlists, albums, artists, Liked Songs…),
/// most recently pinned first. Set "ylpin" is read-only for us: writes to it are accepted
/// but ignored, so pinning stays in the apps.
pub async fn pins(session: &Session) -> Res<Vec<String>> {
    collection(session, "ylpin").await
}

/// Every uri in a collection2v2 set, newest first: "collection" (liked songs and saved
/// albums), "artist" (followed artists), "ylpin" (pins).
/// PageRequest { 1: username, 2: set, 3: page token, 4: limit }
/// -> PageResponse { 1: repeated CollectionItem { 1: uri, 2: added_at s, 3: is_removed }, 2: next page token }
pub async fn collection(session: &Session, set: &str) -> Res<Vec<String>> {
    let mut items: Vec<(i64, String)> = Vec::new();
    let mut token = String::new();
    for _ in 0..100 {
        let mut body = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut body);
            os.write_string(1, &session.username()).map_err(e)?;
            os.write_string(2, set).map_err(e)?;
            if !token.is_empty() {
                os.write_string(3, &token).map_err(e)?;
            }
            os.write_int32(4, 5000).map_err(e)?;
            os.flush().map_err(e)?;
        }
        let bytes = session
            .spclient()
            .request(
                &Method::POST,
                "/collection/v2/paging",
                Some(proto_headers("application/vnd.collection-v2.spotify.proto")),
                Some(&body),
            )
            .await
            .map_err(e)?;
        token.clear();
        let mut is = CodedInputStream::from_bytes(&bytes);
        while let Some(tag) = is.read_raw_tag_or_eof().map_err(e)? {
            match tag {
                0x0a => {
                    let raw = is.read_bytes().map_err(e)?;
                    let mut it = CodedInputStream::from_bytes(&raw);
                    let (mut uri, mut at, mut removed) = (String::new(), 0i64, false);
                    while let Some(t) = it.read_raw_tag_or_eof().map_err(e)? {
                        match t {
                            0x0a => uri = it.read_string().map_err(e)?,
                            0x10 => at = it.read_int64().map_err(e)?,
                            0x18 => removed = it.read_bool().map_err(e)?,
                            _ => it.skip_field(protobuf::rt::WireType::new(t & 7).ok_or("bad wire type")?).map_err(e)?,
                        }
                    }
                    if !removed && !uri.is_empty() {
                        items.push((at, uri));
                    }
                }
                0x12 => token = is.read_string().map_err(e)?,
                _ => is.skip_field(protobuf::rt::WireType::new(tag & 7).ok_or("bad wire type")?).map_err(e)?,
            }
        }
        if token.is_empty() {
            break;
        }
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.0)); // stable: same-second items keep server order
    Ok(items.into_iter().map(|(_, u)| u).collect())
}

// ---------------------------------------------------------------- playlists

fn playlist_id(uri: &str) -> Res<SpotifyId> {
    let id = uri.rsplit(':').next().unwrap_or_default();
    SpotifyId::from_base62(id).map_err(e)
}

async fn fetch_playlist(session: &Session, uri: &str) -> Res<SelectedListContent> {
    let bytes = session.spclient().get_playlist(&playlist_id(uri)?).await.map_err(e)?;
    SelectedListContent::parse_from_bytes(&bytes).map_err(e)
}

async fn send_changes(session: &Session, uri: &str, base_revision: Vec<u8>, op: Op) -> Res<()> {
    let id = uri.rsplit(':').next().unwrap_or_default();
    let mut info = ChangeInfo::new();
    info.set_user(session.username());
    info.set_timestamp(now_ms());
    let mut delta = Delta::new();
    delta.ops.push(op);
    delta.info = MessageField::some(info);
    let mut changes = ListChanges::new();
    changes.set_base_revision(base_revision);
    changes.deltas.push(delta);
    let body = changes.write_to_bytes().map_err(e)?;
    session
        .spclient()
        .request(
            &Method::POST,
            &format!("/playlist/v2/playlist/{id}/changes"),
            Some(proto_headers("application/x-protobuf")),
            Some(&body),
        )
        .await
        .map_err(e)?;
    Ok(())
}

fn item(uri: &str, with_time: bool) -> Item {
    let mut it = Item::new();
    it.set_uri(uri.to_string());
    if with_time {
        let mut a = ItemAttributes::new();
        a.set_timestamp(now_ms());
        it.attributes = MessageField::some(a);
    }
    it
}

/// Append a track to the end of a playlist.
pub async fn playlist_add(session: &Session, playlist: &str, track: &str) -> Res<()> {
    let list = fetch_playlist(session, playlist).await?;
    let mut add = Add::new();
    add.items.push(item(track, true));
    add.set_add_last(true);
    let mut op = Op::new();
    op.kind = Some(EnumOrUnknown::new(Kind::ADD));
    op.add = MessageField::some(add);
    send_changes(session, playlist, list.revision().to_vec(), op).await
}

/// Remove the first occurrence of a track. The current playlist is fetched first so the
/// index is never stale (e.g. edited from the phone meanwhile).
pub async fn playlist_remove(session: &Session, playlist: &str, track: &str) -> Res<()> {
    let list = fetch_playlist(session, playlist).await?;
    let items = list.contents.as_ref().map(|c| &c.items[..]).unwrap_or_default();
    let index = items.iter().position(|it| it.uri() == track).ok_or("song isn't in that playlist any more")?;
    let mut rem = Rem::new();
    rem.set_from_index(index as i32);
    rem.set_length(1);
    rem.items.push(item(track, false));
    let mut op = Op::new();
    op.kind = Some(EnumOrUnknown::new(Kind::REM));
    op.rem = MessageField::some(rem);
    send_changes(session, playlist, list.revision().to_vec(), op).await
}

// ---------------------------------------------------------------- lyrics

pub struct Lyrics {
    /// Lines have real start times (karaoke-style highlighting is possible).
    pub synced: bool,
    pub lines: Vec<(u32, String)>,
    pub provider: String,
}

impl Lyrics {
    /// Index of the line being sung at `pos_ms` (synced lyrics only).
    pub fn current(&self, pos_ms: u32) -> Option<usize> {
        if !self.synced {
            return None;
        }
        let i = self.lines.partition_point(|(t, _)| *t <= pos_ms);
        i.checked_sub(1)
    }

    /// When the next line starts, so the UI can redraw exactly then instead of polling.
    pub fn next_change(&self, pos_ms: u32) -> Option<u32> {
        if !self.synced {
            return None;
        }
        self.lines.iter().map(|(t, _)| *t).find(|t| *t > pos_ms)
    }
}

pub fn parse_lyrics(v: &Value) -> Option<Lyrics> {
    let l = &v["lyrics"];
    let synced = l["syncType"].as_str() == Some("LINE_SYNCED");
    let lines: Vec<(u32, String)> = l["lines"]
        .as_array()?
        .iter()
        .map(|line| {
            let t = line["startTimeMs"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
            let words = line["words"].as_str().unwrap_or_default();
            // Spotify uses "♪" for instrumental breaks
            (t, if words.is_empty() { "♪".to_string() } else { words.to_string() })
        })
        .collect();
    (!lines.is_empty()).then(|| Lyrics {
        synced,
        lines,
        provider: l["providerDisplayName"].as_str().unwrap_or_default().to_string(),
    })
}

/// `Ok(None)` when Spotify has no lyrics for the track.
pub async fn lyrics(session: &Session, track_uri: &str) -> Res<Option<Lyrics>> {
    let id = track_uri.rsplit(':').next().unwrap_or_default();
    let endpoint = format!("/color-lyrics/v2/track/{id}?format=json&market=from_token");
    match session.spclient().request_as_json(&Method::GET, &endpoint, None, None).await {
        Ok(body) => {
            let v: Value = serde_json::from_slice(&body).map_err(e)?;
            Ok(parse_lyrics(&v))
        }
        Err(Error { kind: ErrorKind::NotFound, .. }) => Ok(None),
        Err(err) => Err(e(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_collection_write() {
        let b = collection_write("u", "spotify:track:x", false).unwrap();
        // field 1 "u", field 2 "collection", field 3 = item{1: uri, 2: ts, 3: true}
        assert_eq!(&b[..3], &[0x0a, 1, b'u']);
        assert_eq!(&b[3..5], &[0x12, 10]);
        assert!(b.ends_with(&[0x18, 1]));
    }

    #[test]
    fn finds_current_line() {
        let v: Value = serde_json::from_str(
            r#"{"lyrics":{"syncType":"LINE_SYNCED","lines":[
                {"startTimeMs":"1000","words":"a"},{"startTimeMs":"5000","words":""},
                {"startTimeMs":"9000","words":"c"}],"providerDisplayName":"P"}}"#,
        )
        .unwrap();
        let l = parse_lyrics(&v).unwrap();
        assert_eq!(l.current(500), None);
        assert_eq!(l.current(1000), Some(0));
        assert_eq!(l.current(6000), Some(1));
        assert_eq!(l.lines[1].1, "♪");
        assert_eq!(l.next_change(6000), Some(9000));
        assert_eq!(l.next_change(9500), None);
    }
}
