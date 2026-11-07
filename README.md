# cachify

A lean terminal Spotify player for CachyOS (or any Linux running PipeWire or PulseAudio).
Browsing and search go through [librespot](https://github.com/librespot-org/librespot).
Audio plays through Spotify's official headless player, **Soloist**, or optionally through
librespot itself. **Requires Spotify Premium.**

## Why Soloist?

Since late 2025, Spotify has been refusing audio keys to librespot on more and more
accounts ([librespot#1649](https://github.com/librespot-org/librespot/issues/1649)).
On those accounts, login and browsing work, but every track fails with
`error audio key 0 1` and gets skipped. Soloist is Spotify's own player, so it isn't
affected. cachify starts it in the background, controls it over its local WebSocket,
and keeps shuffle, repeat and the queue itself.

### One-time Soloist setup

1. Get an API key: https://developer.spotify.com/dashboard → **Spotify Soloist API Key**.
   Save it, readable only by you:
   ```bash
   install -Dm600 /dev/null ~/.config/cachify/soloist_api_key && $EDITOR ~/.config/cachify/soloist_api_key
   ```
2. Install Soloist (downloaded from Spotify's CDN). Re-run this at least every 90 days,
   because Spotify's builds expire:
   ```bash
   cachify --update-soloist
   ```
3. Pair once: run `cachify --pair`, then pick **cachify** in the device list of the Spotify
   app on your phone (same Wi-Fi).

cachify uses Soloist automatically once the key and binary are present. To force an
engine, set `backend = soloist` or `backend = librespot` in the config.

**Song transitions:** when a song ends, Soloist would normally play its own autoplay
"radio" pick. To stop that, cachify puts your next song into Soloist's queue about 8 s
before the end, so the switch is native and gapless. Soloist's queue can't be edited, so if
you pick a different song during those last 8 s, the queued song is skipped first and can
be heard for a split second (about 0.15 s).

**Measured on this machine** (during playback, 3,852-song Liked Songs open):

| process | CPU (one core) | memory (PSS) |
|---|---|---|
| cachify (TUI + library) | ~0.2% | ~16 MB |
| Soloist | ~2.2% | ~48 MB |
| Soloist crash reporter | 0% | ~11 MB |

Note: Soloist only accepts the API key as a command-line argument, so anything that can
list processes on this machine (`/proc/<pid>/cmdline`) can see it while Soloist runs. On a
single-user desktop that's fine; on a shared machine, keep that in mind.

## Install

```bash
cargo build --release
install -Dm755 target/release/cachify ~/.local/bin/cachify
```

Build deps (already on CachyOS): `rust`, `libpulse`, `openssl`, `pkgconf`.

## First run

```bash
cachify
```

Your browser opens for Spotify login, once. This login is for browsing your library.
librespot then stores reusable credentials in `~/.cache/cachify/credentials.json`, so
later starts skip the browser. `cachify --logout` deletes them.

## Sorting

Playlists and Liked Songs are shown **most recently added first**, with an **Added**
date column. Playing from the top therefore plays your newest additions first. Press `o`
to switch to the playlist's own order. Albums and artists always keep their track order.

## EQ, crossfade and quality

- **EQ:** `e` cycles presets (Flat, Bass Boost, Bass Reducer, Treble Boost, Vocal, Rock,
  Pop, Hip-Hop, Electronic, Acoustic, Loudness, Custom). `E` opens the 10-band panel
  (32 Hz to 16 kHz, ±12 dB). In the panel, `↑↓` picks a band, `←→` adjusts it and `0`
  resets it. A preamp is set automatically so boosts don't clip.
  - It's a small PipeWire filter that only cachify's audio passes through; other apps
    aren't affected.
  - **On Flat it doesn't run at all.** The first non-flat setting starts it and briefly
    restarts the player (~3 s, resumes where you were). After that, changes are live.
  - Cost while running: ~1.7% of one core and ~4 MB. Set `eq = off` in the config to
    disable it completely.
- **Crossfade:** `x` cycles off / 3 / 5 / 8 / 12 s. This is Spotify's own crossfade inside
  Soloist. Applying it restarts Soloist (~3 s, resumes where you were).
- **Quality:** cachify sets Soloist to 320 kbps. Out of the box it streamed at 160 kbps.

Settings are saved in `~/.local/share/cachify/audio_settings`.

## Desktop integration

cachify registers as an MPRIS player (`org.mpris.MediaPlayer2.cachify`). That gives you:
- the KDE media widget and lock screen, with title, artist and album art
- media keys (play/pause, next, previous)
- `playerctl`, KDE Connect, and anything else that speaks MPRIS

Install the launcher so KDE can show its name and icon (it also adds a menu entry):

```bash
install -Dm644 assets/cachify.desktop ~/.local/share/applications/cachify.desktop
```

For Spotify's icon, change `Icon=` to `spotify-launcher`, if that package is installed.

## Library, lyrics and album art

- **Liking:** `f` likes or unlikes a song. Liked songs show a ♥ everywhere, including the
  now-playing line. The ♥ flips straight away and is undone if Spotify refuses.
- **Playlists:** `A` opens a picker of the playlists you can edit (yours and collaborative
  ones; type to filter). It warns before adding a song that's already there. In one of your
  playlists, `D` twice removes the highlighted song. cachify re-reads the playlist from
  Spotify first, so an edit made on your phone in the meantime can't make it remove the
  wrong song.
- **Lyrics:** `L` swaps the track list for the current song's lyrics. When Spotify has
  timed lyrics, the current line is highlighted and kept in view; otherwise `j`/`k` scroll.
  Lyrics are only fetched while this view is open. The screen redraws exactly when the
  next line starts, not on a timer.
- **Album art:** the cover sits under the playlist list when the window is tall enough. In
  terminals with an image protocol it's a real image: kitty, Ghostty, WezTerm, foot and
  iTerm2 are detected automatically, and Konsole uses the iTerm2 protocol. Elsewhere,
  including Alacritty and tmux, it's drawn with coloured half-block characters. The cover
  is decoded and resized once per song, off the UI thread.

These go through the same private Spotify API as the rest of cachify (not the Web API), so
no rate limits apply. Each action takes about 0.4–0.5 s to reach Spotify.

## Keys

| key | action |
|---|---|
| `↑↓` / `jk`, `PgUp/PgDn`, `g/G` | move |
| `Tab`, `h` / `l` | switch pane (library ↔ tracks) |
| `Enter` | play track / open album, playlist or artist |
| `Esc` / `Backspace` | back |
| `Space` | play / pause |
| `n` / `p` | next / previous |
| `←→` or `,` `.` | seek ±10 s |
| `+` / `-` | volume ±5% |
| `a` | add to queue |
| `c` | show the queue |
| `f` | like / unlike (the highlighted song, or what's playing) |
| `A` | add to one of your playlists (type to filter) |
| `D` `D` | remove the highlighted song from the open playlist |
| `L` | lyrics (synced ones follow the song) |
| `i` | show / hide album art |
| `s` / `r` | shuffle / repeat (off → all → one) |
| `o` | sort: recently added ↔ playlist order |
| `e` / `E` | next EQ preset / EQ panel |
| `x` | crossfade off / 3 / 5 / 8 / 12 s |
| `/` | search |
| `R` | reload the current list (or playlists) |
| `?` | key help in the status line |
| `q` | quit |

## Config (optional)

`~/.config/cachify/config`:

```ini
bitrate = 320          # 96 | 160 | 320
audio_cache_mb = 0     # >0 keeps played tracks on disk (saves bandwidth on repeats)
normalise = false
client_id =            # your own Spotify app, used ONLY for search (see below)
backend = auto         # auto | soloist | librespot
device_name = cachify  # how Soloist shows up in Spotify Connect
soloist_cache_mb = 200
soloist_ws =           # host:port of a Soloist you already run, instead of spawning one
eq = on                # off = never run the EQ process
art = on               # off = no album art (and no terminal capability query)
```

### Better search (recommended)

Playlists, liked songs, albums and artists use librespot's own connection, so the public
Web API's limits don't apply to them. Search does use the Web API. Without your own app,
search borrows librespot's shared token, which every librespot app uses, so Spotify
rate-limits it most of the time. cachify always runs a tracks-only search alongside it
(about 0.3 s), and after one rate-limit error it stops asking the Web API for 2 minutes.
That keeps search fast either way.

To get reliable full search (tracks, albums, playlists and artists):

1. Go to https://developer.spotify.com/dashboard, click **Create app**, and choose Web API.
2. Add the redirect URI `http://127.0.0.1:8898/login`.
3. Put the Client ID in the config as `client_id = …`. The next start asks you to log in
   once more, for search only.

The status line shows which search path answered: `web api` (your app), `web api*`
(shared token) or `basic` (fallback).

## Why it's light

- One async thread runs the UI and networking. librespot's player runtime is capped to
  1 worker thread instead of one per CPU core.
- Terminal input sits on one blocking thread, using no CPU while idle.
- The screen redraws only on input, player events, or once a second while playing.
  Nothing polls Spotify.
- Audio is decoded in-process and sent to PipeWire over the PulseAudio protocol. No
  rodio/cpal layer.
- Stream buffers go to `~/.cache/cachify/tmp`, not `/tmp`. On CachyOS `/tmp` is tmpfs,
  which means RAM.
- Track info is fetched in batches of 300 per request, not one request per song, with
  up to 16 batches in parallel.
- Only the table rows that fit on screen are built each frame.
- The playlist list is cached in `~/.cache/cachify/playlists`, so the sidebar fills on the
  first frame. It's refreshed in the background.
- Spotify login and Soloist startup run at the same time.
- Freed heap is handed back to the OS after big lists load (`malloc_trim`), and glibc is
  limited to 2 malloc arenas.

Measured on an i9-14900HX laptop (network round trip to Spotify ~150 ms):

| | |
|---|---|
| Launch to library on screen | ~0.44 s |
| Open Liked Songs (3,852 tracks), first time | ~0.55 s; afterwards ~25 ms |
| Search | ~0.35 s |
| Key press to redraw | ~2 ms |
| Press play to playing | ~0.2 s |
| cachify RAM at start (♥ set + cover loaded) | ~14 MB PSS |
| cachify RAM, Liked Songs open | ~16.5 MB PSS |
| cachify CPU while idle | 0 |
| cachify CPU while playing | ~0.1–0.3% of one core (also with lyrics open) |
| Soloist while playing | ~2.5% of one core, ~48 MB |

## Debugging

```bash
CACHIFY_LOG=debug cachify            # writes ~/.cache/cachify/cachify.log
cachify --debug-play [spotify:track:…] # play one track headless, print engine events
```

Soloist's own output goes to `~/.cache/cachify/soloist.log`.
