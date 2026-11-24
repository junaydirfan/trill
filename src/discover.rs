//! "For You": songs picked from what you actually play, worked out on this machine.
//!
//! Every song played for 30 s goes in a local log (`~/.local/share/trill/plays`), which
//! never leaves the machine. From it trill takes seeds, counting each play less the older it
//! is (half as much every 14 days), with your newest liked songs filling in while the log is
//! short. It then asks Spotify, with the same requests trill makes while browsing, for:
//! - the radio of your top seed songs (what smart shuffle uses), and
//! - the top songs of artists Spotify lists as related to your top artists.
//!
//! Those are ranked here. Songs you've liked or marked "not interested" are left out, songs
//! you've been playing a lot are pushed down, and each further song by the same artist
//! counts half as much (3 at most), so no one artist takes over. Every row says why it's there.
//!
//! What goes to Spotify: the ids of up to 4 seed songs (their radio), up to 40 of your top
//! songs (to look up their artists), up to 16 artist ids (their pages) and the ids of the
//! songs suggested (for their titles and covers). The play log itself stays here.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::Path,
};

use futures_util::{StreamExt, stream};
use librespot_core::Session;

use crate::api::{self, Row, Rows};

pub const FOR_YOU: &str = "trill:foryou";
const PLAYS: &str = "plays";
const NOT_INTERESTED: &str = "not_interested";
const HALF_LIFE_DAYS: f64 = 14.0;
const DAY: f64 = 86_400.0;
/// Suggestions shown.
const SHOW: usize = 50;
const SEED_SONGS: usize = 4;
const SEED_ARTISTS: usize = 4;
/// Related artists whose top songs are asked for.
const RELATED: usize = 12;
/// At most this many songs by one artist.
const PER_ARTIST: u32 = 3;
/// Below this many logged songs, the newest liked songs are seeds too.
const THIN_LOG: usize = 15;

type Res<T> = Result<T, String>;

/// One logged play: when (unix seconds), the song, and its title for the "why" column.
pub struct Play {
    pub at: u64,
    pub uri: String,
    pub title: String,
}

/// Note that `row` has played for 30 s: `unix secs<TAB>uri<TAB>title<TAB>artist`, appended.
/// shortcut: the log is never trimmed (~100 bytes a song, ~2 MB a year of daily listening); it's
/// only read when For You refreshes, so it costs no memory meanwhile.
pub fn log_play(dir: &Path, row: &Row) {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let line = format!("{}\t{}\t{}\t{}\n", crate::app::unix_now(), row.uri, clean(&row.title), clean(&row.artist));
    let _ = fs::create_dir_all(dir);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join(PLAYS)) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn read_plays(dir: &Path) -> Vec<Play> {
    let text = fs::read_to_string(dir.join(PLAYS)).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Play { at: f.next()?.parse().ok()?, uri: f.next()?.to_string(), title: f.next().unwrap_or("").to_string() })
        })
        .collect()
}

/// Songs you said you're not interested in (`X`), one uri per line.
pub fn not_interested(dir: &Path) -> HashSet<String> {
    fs::read_to_string(dir.join(NOT_INTERESTED)).unwrap_or_default().lines().map(String::from).filter(|l| !l.is_empty()).collect()
}

pub fn add_not_interested(dir: &Path, uri: &str) {
    let _ = fs::create_dir_all(dir);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join(NOT_INTERESTED)) {
        let _ = writeln!(f, "{uri}");
    }
}

/// How much each song counts as a seed: each play, halved for every 14 days since.
fn weights(plays: &[Play], now: u64) -> HashMap<String, f64> {
    let mut w = HashMap::new();
    for p in plays {
        let age = now.saturating_sub(p.at) as f64 / DAY;
        *w.entry(p.uri.clone()).or_insert(0.0) += 0.5f64.powf(age / HALF_LIFE_DAYS);
    }
    w
}

/// Seed songs, strongest first: the play log, plus (while it's short) the newest liked songs,
/// each counting as half a play that fades over a month.
fn seeds(plays: &[Play], liked: &[Row], now: u64) -> Vec<(String, f64)> {
    let mut w = weights(plays, now);
    if w.len() < THIN_LOG {
        for r in liked.iter().filter(|r| r.kind == api::Kind::Track).take(50) {
            let age = now.saturating_sub(r.added_at / 1000) as f64 / DAY;
            w.entry(r.uri.clone()).or_insert(0.5 * 0.5f64.powf(age / 30.0));
        }
    }
    let mut v: Vec<(String, f64)> = w.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Plays in the last 30 days, per song.
fn recent_counts(plays: &[Play], now: u64) -> HashMap<String, u32> {
    let mut c = HashMap::new();
    for p in plays.iter().filter(|p| now.saturating_sub(p.at) < 30 * 86_400) {
        *c.entry(p.uri.clone()).or_insert(0) += 1;
    }
    c
}

/// A song suggested by one source, with how strongly and why.
struct Candidate {
    uri: String,
    score: f64,
    why: String,
}

/// Merge the candidates for each song (scores add up, the strongest reason is kept), leave
/// out `exclude`, push down songs played lately (score / (1 + plays in 30 days)), then take
/// the best greedily, each further song by the same artist counting half as much.
/// The reason goes in `members`, which the table shows in its own column.
fn rank(cands: Vec<Candidate>, rows: &HashMap<String, Row>, exclude: &HashSet<String>, recent: &HashMap<String, u32>, n: usize) -> Vec<Row> {
    let mut merged: HashMap<String, (f64, f64, String)> = HashMap::new(); // uri -> (total, best, why)
    for c in cands.into_iter().filter(|c| !exclude.contains(&c.uri)) {
        let e = merged.entry(c.uri).or_insert((0.0, 0.0, String::new()));
        e.0 += c.score;
        if c.score > e.1 {
            (e.1, e.2) = (c.score, c.why);
        }
    }
    let mut pool: Vec<(f64, &Row, String)> = merged
        .into_iter()
        .filter_map(|(uri, (score, _, why))| {
            let row = rows.get(&uri)?;
            Some((score / (1.0 + *recent.get(&uri).unwrap_or(&0) as f64), row, why))
        })
        .collect();
    // ties (and equal damped scores) go by uri, so the same input gives the same list
    pool.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.uri.cmp(&b.1.uri)));
    let artist = |r: &Row| r.artist.split(", ").next().unwrap_or("").to_lowercase();
    let mut per_artist: HashMap<String, u32> = HashMap::new();
    let mut out = Vec::new();
    while out.len() < n {
        let damped = |(s, r, _): &(f64, &Row, String)| {
            let k = *per_artist.get(&artist(r)).unwrap_or(&0);
            (k < PER_ARTIST).then(|| s * 0.5f64.powi(k as i32))
        };
        let Some(i) = (0..pool.len()).filter(|&i| damped(&pool[i]).is_some()).max_by(|&a, &b| {
            damped(&pool[a]).unwrap_or(0.0).total_cmp(&damped(&pool[b]).unwrap_or(0.0)).then_with(|| pool[b].1.uri.cmp(&pool[a].1.uri))
        }) else {
            break;
        };
        let (_, row, why) = pool.remove(i);
        *per_artist.entry(artist(row)).or_insert(0) += 1;
        let mut r = row.clone();
        r.members = why;
        out.push(r);
    }
    out
}

/// Work out For You. `liked`: Liked Songs (seeds while the play log is short); `exclude`:
/// what's never suggested (your library and "not interested").
pub async fn for_you(session: &Session, dir: &Path, liked: Rows, exclude: HashSet<String>) -> Res<Vec<Row>> {
    let now = crate::app::unix_now();
    let plays = read_plays(dir);
    let seeds = seeds(&plays, &liked, now);
    let Some(max) = seeds.first().map(|s| s.1) else {
        return Err("play or like a few songs first: suggestions start from what you play".into());
    };
    let mut title: HashMap<&str, &str> = liked.iter().map(|r| (r.uri.as_str(), r.title.as_str())).collect();
    title.extend(plays.iter().filter(|p| !p.title.is_empty()).map(|p| (p.uri.as_str(), p.title.as_str())));

    // your top artists: the first artist of each top song, adding up the songs' weights
    let top: Vec<String> = seeds.iter().take(40).map(|s| s.0.clone()).collect();
    let artist_of = api::track_artists(session, &top).await?;
    let mut by_artist: HashMap<String, (f64, String)> = HashMap::new();
    for (uri, w) in seeds.iter().take(40) {
        if let Some((a, name)) = artist_of.get(uri) {
            by_artist.entry(a.clone()).or_insert((0.0, name.clone())).0 += w / max;
        }
    }
    let mut seed_artists: Vec<(String, f64, String)> = by_artist.into_iter().map(|(a, (w, n))| (a, w, n)).collect();
    seed_artists.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    seed_artists.truncate(SEED_ARTISTS);
    let artist_max = seed_artists.first().map_or(1.0, |a| a.1);

    // the radio of the top songs and the pages of the top artists, all at once (owned values:
    // the requests run in parallel inside a spawned task)
    let seed_songs: Vec<(String, f64)> = seeds.iter().take(SEED_SONGS).cloned().collect();
    let radios = stream::iter(seed_songs)
        .map(|(uri, w)| {
            let s = session.clone();
            async move {
                let songs = api::station(&s, &uri).await;
                (uri, w / max, songs)
            }
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>();
    let pages = stream::iter(seed_artists.clone())
        .map(|(a, w, name)| {
            let s = session.clone();
            async move {
                let page = api::artist_overview(&s, &a).await;
                (a, w / artist_max, name, page)
            }
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>();
    let (radios, pages) = tokio::join!(radios, pages);

    // related artists, scored by the top artists that list them (earlier in a list counts more)
    let seed_ids: HashSet<&str> = seed_artists.iter().map(|a| a.0.as_str()).collect();
    let mut related: HashMap<String, (f64, f64, String)> = HashMap::new(); // uri -> (score, best part, why)
    for (a, w, name, page) in &pages {
        let rel = match page {
            Ok((rel, _)) => rel,
            Err(e) => {
                log::warn!("For You: artist page {a}: {e}");
                continue;
            }
        };
        for (i, (uri, _)) in rel.iter().take(8).enumerate().filter(|(_, (u, _))| !seed_ids.contains(u.as_str())) {
            let part = w * 0.9f64.powi(i as i32);
            let e = related.entry(uri.clone()).or_insert((0.0, 0.0, String::new()));
            e.0 += part;
            if part > e.1 {
                (e.1, e.2) = (part, name.clone());
            }
        }
    }
    let mut related: Vec<(String, f64, String)> = related.into_iter().map(|(u, (s, _, why))| (u, s, why)).collect();
    related.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    related.truncate(RELATED);
    let related_pages = stream::iter(related)
        .map(|(uri, w, why)| {
            let s = session.clone();
            async move { (w, why, api::artist_overview(&s, &uri).await) }
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;

    let mut cands = Vec::new();
    for (seed, w, songs) in &radios {
        let why = format!("radio · {}", title.get(seed.as_str()).copied().unwrap_or("a song you play"));
        cands.extend(songs.iter().take(25).enumerate().map(|(i, u)| Candidate { uri: u.clone(), score: w * 0.97f64.powi(i as i32), why: why.clone() }));
    }
    for (w, why, page) in &related_pages {
        let Ok((_, top)) = page else { continue };
        let why = format!("like {why}");
        cands.extend(top.iter().take(4).enumerate().map(|(i, u)| Candidate { uri: u.clone(), score: 0.8 * w * 0.85f64.powi(i as i32), why: why.clone() }));
    }
    // the seeds are songs you play already
    let mut exclude = exclude;
    exclude.extend(seeds.iter().take(SEED_SONGS).map(|s| s.0.clone()));
    let mut want: Vec<String> = cands.iter().map(|c| c.uri.clone()).filter(|u| !exclude.contains(u)).collect();
    want.sort();
    want.dedup();
    let rows: HashMap<String, Row> = api::track_rows(session, want).await?.into_iter().map(|r| (r.uri.clone(), r)).collect();
    let out = rank(cands, &rows, &exclude, &recent_counts(&plays, now), SHOW);
    if out.is_empty() {
        return Err("Spotify had no suggestions this time; R tries again".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(uri: &str, artist: &str) -> Row {
        Row {
            uri: uri.into(),
            title: uri.into(),
            artist: artist.into(),
            album: String::new(),
            duration_ms: 1,
            kind: api::Kind::Track,
            art: None,
            added_at: 0,
            members: String::new(),
            plays: 0,
        }
    }

    #[test]
    fn plays_fade_with_age() {
        let now = 100 * 86_400;
        let p = |days: u64, uri: &str| Play { at: now - days * 86_400, uri: uri.into(), title: String::new() };
        let w = weights(&[p(0, "a"), p(14, "b"), p(28, "c"), p(28, "c")], now);
        assert!((w["a"] - 1.0).abs() < 1e-9 && (w["b"] - 0.5).abs() < 1e-9 && (w["c"] - 0.5).abs() < 1e-9);
        assert_eq!(recent_counts(&[p(1, "a"), p(29, "a"), p(31, "a")], now)["a"], 2);
        // a short log borrows the newest liked songs, below what you actually play
        let s = seeds(&[p(0, "a")], &[row("liked", "x")], now);
        assert_eq!(s[0].0, "a");
        assert_eq!(s[1].0, "liked");
    }

    #[test]
    fn ranks_diverse_explains_and_respects_no() {
        let mut rows = HashMap::new();
        for (u, a) in [("a1", "A"), ("a2", "A"), ("a3", "A"), ("a4", "A, Feat"), ("b1", "B"), ("liked", "C"), ("tired", "D"), ("no", "E")] {
            rows.insert(u.to_string(), row(u, a));
        }
        let c = |uri: &str, score: f64, why: &str| Candidate { uri: uri.into(), score, why: why.into() };
        let cands = vec![
            c("a1", 1.0, "radio · x"),
            c("a2", 0.95, "radio · x"),
            c("a3", 0.9, "radio · x"),
            c("a4", 0.89, "radio · x"),
            c("b1", 0.4, "like B"),
            c("b1", 0.3, "radio · y"), // a second source adds up; the stronger reason stays
            c("liked", 5.0, "radio · x"),
            c("tired", 3.0, "radio · x"),
            c("no", 4.0, "radio · x"),
            c("gone", 9.0, "radio · x"), // no metadata: dropped
        ];
        let exclude: HashSet<String> = ["liked", "no"].map(String::from).into();
        let recent: HashMap<String, u32> = [("tired".to_string(), 9)].into();
        let out = rank(cands, &rows, &exclude, &recent, 10);
        let got: Vec<&str> = out.iter().map(|r| r.uri.as_str()).collect();
        // B's merged 0.7 beats A's second song (0.95 / 2); "tired" (3.0 / 10) beats A's third
        // (0.9 / 4); A's fourth is past the cap of 3
        assert_eq!(got, ["a1", "b1", "a2", "tired", "a3"]);
        assert_eq!(out[1].members, "like B");
        assert_eq!(out[0].members, "radio · x");
    }
}
