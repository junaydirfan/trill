# cachify

A lean terminal Spotify player for CachyOS (or any Linux running PipeWire or PulseAudio).
It plays audio itself through [librespot](https://github.com/librespot-org/librespot),
so it doesn't need the official client or a browser running. **Requires Spotify Premium.**

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

Your browser opens for Spotify login, once. librespot then stores reusable credentials
in `~/.cache/cachify/credentials.json`, so later starts skip the browser.
`cachify --logout` deletes them.

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
| `s` / `r` | shuffle / repeat (off → all → one) |
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
```

### Better search (recommended)

Playlists, liked songs, albums and artists use librespot's own connection, so the public
Web API's limits don't apply to them. Search does use the Web API. Without your own app,
search borrows librespot's shared token, and Spotify sometimes rate-limits that token. If
it does, cachify falls back to a tracks-only search.

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
- Track info is fetched in batches of 300 per request, not one request per song.

## Debugging

```bash
CACHIFY_LOG=debug cachify     # writes ~/.cache/cachify/cachify.log
```
