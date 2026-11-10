<p align="center">
  <img src="assets/logo.svg" width="300" alt="trill">
</p>

<p align="center">
  <em>Spotify in your terminal. Lighter than the tab you'd open to look it up.</em>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-2024-111111?style=flat-square&logo=rust" alt="Rust 2024">
  <img src="https://img.shields.io/badge/linux-PipeWire%20%C2%B7%20PulseAudio-111111?style=flat-square&logo=linux&logoColor=white" alt="Linux, PipeWire or PulseAudio">
  <img src="https://img.shields.io/badge/macOS-Apple%20Silicon-111111?style=flat-square&logo=apple&logoColor=white" alt="macOS on Apple Silicon">
  <img src="https://img.shields.io/badge/windows-x86__64-111111?style=flat-square&logo=windows&logoColor=white" alt="Windows x86_64">
  <img src="https://img.shields.io/badge/built%20for-CachyOS-111111?style=flat-square&logo=archlinux&logoColor=white" alt="Built for CachyOS">
  <img src="https://img.shields.io/badge/needs-Spotify%20Premium-1ed760?style=flat-square&logo=spotify&logoColor=white" alt="Requires Spotify Premium">
</p>

<p align="center">
  <img src="assets/screenshots/hero-gif.gif" width="880" alt="trill playing a song: library with pinned playlists and album art on the left, the track list on the right, the progress bar along the bottom">
</p>

<p align="center">
  <strong>~17 MB RAM &middot; 0% CPU idle &middot; ~2 ms key to frame &middot; ~30 ms launch to library</strong><br>
  <sub>With a 3,852-song Liked Songs open, on an i9-14900HX. Full numbers in <a href="#numbers">Numbers</a>.</sub>
</p>

---

trill is a terminal Spotify client that does one thing and does it cheaply. It plays through **Soloist**, Spotify's own headless player, so audio keeps working on the accounts where librespot gets refused. It browses through librespot's private connection, so no Web API limits apply to your library. One async thread does the UI and the networking. It redraws only when something changes, and it never polls.

## Features

- **Your library.** Playlists, Liked Songs, saved albums and followed artists. Pinned playlists and folders sit at the top, like in the official app.
- **Artist pages.** Top tracks, then every album, single and EP, newest first. `P` plays the whole discography.
- **Synced lyrics.** The current line lights up and stays in view, and the screen redraws exactly when the next line starts.
- **Real album art.** It uses kitty, Ghostty, WezTerm, foot, iTerm2 or Konsole graphics when the terminal has them, and colour half-blocks everywhere else.
- **10-band EQ** (Linux) with 11 presets, as a PipeWire filter that only touches trill's audio. On Flat it doesn't run at all.
- **Spotify's own crossfade** and gapless transitions, streaming at 320 kbps.
- **Themes,** including `cover`, which takes its accent colour from the album art that's playing.
- **Smart shuffle.** Like Spotify's: a recommended song (✦) after every three of yours, picked from the list's own Spotify radio.
- **Edit as you listen.** Like and unlike, add to your playlists and remove from them; a ✓ shows which playlists already have the song. Every change is safe against edits made on your phone.
- **Desktop native.** On Linux, MPRIS gives you the KDE media widget, the lock screen, media keys, `playerctl` and KDE Connect. On macOS, the media keys, Control Center and the lock screen control trill.
- **Remembers you.** Volume, EQ, crossfade, shuffle, repeat, sort order and theme come back on the next start.

## Numbers

Measured on an i9-14900HX laptop, with a network round trip to Spotify of about 150 ms.

| | |
|---|---|
| Launch to library on screen | ~30 ms (usable after login, ~0.35–0.6 s) |
| Open Liked Songs (3,852 tracks), first time | ~0.55 s; afterwards ~25 ms |
| Search | ~0.35 s |
| Key press to redraw | ~2 ms |
| Press play to playing | ~0.2 s |
| trill RAM at start (♥ set + cover loaded) | ~14 MB PSS |
| trill RAM, Liked Songs open | ~17 MB PSS |
| trill CPU while idle | 0 |
| trill CPU while playing | ~0.1–0.3% of one core (also with lyrics open) |
| Soloist while playing | ~2.5% of one core, ~48 MB (+ ~11 MB crash reporter) |
| EQ, when not on Flat | ~1.7% of one core, ~4 MB |

### Why it's light

- One async thread runs the UI and networking. librespot's player runtime is capped to one worker thread instead of one per CPU core.
- Terminal input sits on one blocking thread, so it uses no CPU while idle.
- The screen redraws only on input, player events, or once a second while playing. Nothing polls Spotify.
- Audio is decoded in-process and sent to PipeWire over the PulseAudio protocol. There's no rodio or cpal layer.
- Stream buffers go to `~/.cache/trill/tmp`, not `/tmp`. On CachyOS `/tmp` is tmpfs, which means RAM.
- Track info is fetched 300 tracks per request, with up to 16 requests in parallel, instead of one request per song.
- Each frame builds only the table rows that fit on screen.
- The playlist list is cached, so the sidebar fills on the first frame and refreshes in the background.
- Spotify login and Soloist startup run at the same time. With a saved login, the library is drawn from cache before the login finishes, and keys you type meanwhile wait their turn.
- Freed heap goes back to the OS after big lists load (`malloc_trim`), and glibc is limited to 2 malloc arenas.
- Album art is decoded and resized once per song, off the UI thread.

## Get started

You need **Spotify Premium** and about 10 minutes. Each step only happens once.

### 1. Install trill

**Windows:** paste this into PowerShell. It puts `trill.exe` in `~\.local\bin` and adds that folder to your `PATH`:

```powershell
$dir = "$HOME\.local\bin"; New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest https://github.com/junaydirfan/trill/releases/latest/download/trill-windows-x86_64.zip -OutFile "$env:TEMP\trill.zip"
Expand-Archive -Force "$env:TEMP\trill.zip" $dir
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$dir", "User")
```

Then **close PowerShell and open a new window**. Only new windows know where `trill` is; the old one says "trill is not recognized". Use Windows Terminal if you can: it shows real album art, and the old console draws it with coloured blocks.

**macOS (Apple Silicon):** paste this into Terminal, then open a new Terminal window:

```bash
mkdir -p ~/.local/bin && curl -fsSL https://github.com/junaydirfan/trill/releases/latest/download/trill-macos-arm64.tar.gz | tar -xz -C ~/.local/bin
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

**Linux (x86_64):** download it, or build it (below). It needs `libpulse`, which comes with PipeWire and PulseAudio:

```bash
mkdir -p ~/.local/bin && curl -fsSL https://github.com/junaydirfan/trill/releases/latest/download/trill-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
```

<details>
<summary><strong>Build from source instead</strong></summary>

You only need Rust for this; "`cargo` is not recognized" means it isn't installed, and the downloads above don't need it.

- **Linux:** needs `rust`, `libpulse`, `openssl` and `pkgconf` (all already on CachyOS).
  ```bash
  git clone https://github.com/junaydirfan/trill && cd trill
  cargo build --release
  install -Dm755 target/release/trill ~/.local/bin/trill
  ```
  Optional: install the launcher so KDE shows trill's name and icon in the media widget (it also adds a menu entry). With the `spotify-launcher` package installed, you can set its `Icon=` to `spotify-launcher` for Spotify's icon.
  ```bash
  install -Dm644 assets/trill.desktop ~/.local/share/applications/trill.desktop
  ```
- **macOS:** run `xcode-select --install`, install Rust from https://rustup.rs, then `cargo install --git https://github.com/junaydirfan/trill`.
- **Windows:** install Rust from https://rustup.rs (its installer offers to set up the Visual Studio C++ build tools it needs), open a new window, then `cargo install --git https://github.com/junaydirfan/trill`.

`cargo install` puts `trill` in `~/.cargo/bin`, which the Rust installer adds to your `PATH`.
</details>

### 2. Get your Soloist API key

trill plays music through **Soloist**, Spotify's own player ([why](#why-soloist)). Soloist needs a free API key that belongs to your Spotify account:

1. Sign in at https://developer.spotify.com/dashboard with your Premium account. The first time, accept the developer terms.
2. Open the **Soloist API Key** page and create a key. It starts with `spak_`.

Everyone needs their own key, made with their own Premium account: a friend can't use yours. Keep it private, like a password. Spotify's guide: [Soloist authentication](https://developer.spotify.com/documentation/soloist/concepts/authentication).

### 3. Save the key where trill looks for it

**Windows (PowerShell):** this opens an empty file in Notepad. Paste the key, press Ctrl+S, and close Notepad.

```powershell
New-Item -Force -ItemType File $HOME\.config\trill\soloist_api_key | Out-Null
notepad $HOME\.config\trill\soloist_api_key
```

**macOS and Linux:** this makes a file only you can read and opens it. Paste the key and save (in nano: Ctrl+O, Enter, Ctrl+X).

```bash
mkdir -p ~/.config/trill && touch ~/.config/trill/soloist_api_key && chmod 600 ~/.config/trill/soloist_api_key
${EDITOR:-nano} ~/.config/trill/soloist_api_key
```

### 4. Download Soloist

```bash
trill --update-soloist
```

This fetches Spotify's Soloist build for your system. Spotify's builds expire after 90 days; when trill says yours has, run this again.

### 5. Pair with your phone

```bash
trill --pair
```

Open Spotify on your phone (on the same Wi-Fi), tap the devices icon and pick **trill**. On Windows or a Mac, allow it when you're asked about the firewall, incoming connections or local network access, or your phone won't see it.

### 6. Play

```bash
trill
```

Your browser opens once to log in to Spotify, which trill uses to browse your library. Later starts skip it (`trill --logout` forgets the login). Press `?` for every key.

### Updating

Run the download again; it replaces the old trill. Quit trill first (on Windows the file is locked while it runs). Your login, key, pairing and settings stay. `trill --version` shows what you have, and the [releases page](https://github.com/junaydirfan/trill/releases) what's new.

**Windows** (PowerShell):

```powershell
Invoke-WebRequest https://github.com/junaydirfan/trill/releases/latest/download/trill-windows-x86_64.zip -OutFile "$env:TEMP\trill.zip"
Expand-Archive -Force "$env:TEMP\trill.zip" "$HOME\.local\bin"
```

**macOS:**

```bash
curl -fsSL https://github.com/junaydirfan/trill/releases/latest/download/trill-macos-arm64.tar.gz | tar -xz -C ~/.local/bin
```

**Linux:**

```bash
curl -fsSL https://github.com/junaydirfan/trill/releases/latest/download/trill-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
```

Built it yourself? Run `cargo install --git https://github.com/junaydirfan/trill --force`, or `git pull` and build again in your clone.

Soloist updates separately: when trill says your Soloist build has expired (Spotify's builds last 90 days), run `trill --update-soloist`.

### If something's wrong

- **"trill is not recognized" or "command not found":** open a new terminal window after installing.
- **"cargo is not recognized":** you only need Rust to build trill yourself. Use the download in step 1.
- **Every song is skipped with "Unavailable":** trill isn't playing through Soloist, and the message says what's missing. Usually the key isn't saved in `~/.config/trill/soloist_api_key` (on Windows, `C:\Users\<you>\.config\trill\soloist_api_key`), or step 4 hasn't been run.
- **Your phone doesn't list trill:** run `trill --pair` again with the phone on the same Wi-Fi, and allow the firewall prompt.
- **Anything else:** run `TRILL_LOG=debug trill` (on Windows: `$env:TRILL_LOG="debug"; trill`). trill's log is then in `~/.cache/trill/trill.log`, and Soloist's in `~/.cache/trill/soloist.log`.

> [!NOTE]
> macOS and Windows support is new. If something doesn't work there, an issue with the log helps.

> [!TIP]
> Setting trill up on a second computer? Copy only `~/.config/trill/soloist_api_key` and pair again there. Don't copy `~/.local/share/trill`: it holds this computer's Soloist identity, and two players sharing one identity keep taking playback from each other.

### Why Soloist

Since late 2025, Spotify has been refusing audio keys to librespot on more and more accounts ([librespot#1649](https://github.com/librespot-org/librespot/issues/1649)). On those accounts, login and browsing work, but every track fails with `error audio key 0 1` and is skipped. Soloist is Spotify's own player, so it isn't affected. trill starts it in the background, controls it over its local WebSocket, and manages shuffle, repeat and the queue itself.

trill uses Soloist automatically once the key and Soloist are both in place, and librespot otherwise. To force one, set `backend = soloist` or `backend = librespot` in the [config](#details).

> [!NOTE]
> Soloist only accepts the API key as a command-line argument. While it runs, anything that can list processes on this machine (`/proc/<pid>/cmdline`) can see the key. That's fine on a single-user desktop, but keep it in mind on a shared machine.

<details>
<summary><strong>How song transitions work</strong></summary>

When a song ends, Soloist would normally play its own autoplay "radio" pick. To prevent that, trill puts your next song into Soloist's queue about 8 s before the end, so the switch is native and gapless. Soloist's queue can't be edited. If you pick a different song during those last 8 s, the queued song is skipped first, and you may hear a split second of it (about 0.15 s).
</details>

## Keys

| key | action | | key | action |
|---|---|---|---|---|
| `↑↓` `jk` `PgUp/PgDn` `g/G` | move | | `f` | like a song · save an album · follow an artist |
| `Tab` `h` `l` | switch pane | | `A` | add to or remove from a playlist (✓ = already in it) |
| `Enter` | play / open | | `D` `D` | remove from the open playlist |
| `Esc` `Backspace` | back | | `L` | lyrics |
| `Space` | play / pause | | `i` | album art on / off |
| `n` / `p` | next / previous | | `t` | next colour theme |
| `w` | jump to the playing song | | `s` | shuffle: off → on → smart |
| `←→` or `,` `.` | seek ±10 s | | `r` | repeat: off → all → one |
| `+` / `-` | volume ±5% | | `o` / `O` | next / previous sort order |
| `a` | add to queue | | `e` / `E` | next EQ preset / EQ panel |
| `c` | show the queue | | `x` | crossfade off / 3 / 5 / 8 / 12 s |
| `/` | search | | `R` | reload the current list |
| `P` | play everything in the list | | `?` | key help |
| `q` `Ctrl-C` | quit (pauses first, so Spotify and Discord stop showing the song) | | | |

## Details

<details>
<summary><strong>Library, sorting and pins</strong></summary>

- **Sorting:** playlists and Liked Songs show **most recently added first**, with an **Added** column, so playing from the top plays your newest additions first. `o` steps through the other orders: the playlist's own order, title, artist, album and length (`O` steps back). The list's header shows the current order, and it's remembered. Artist pages and albums always keep Spotify's order.
- **Pins:** playlists and folders you pinned in Spotify sit at the top of the library, newest pin first. Opening a pinned folder lists its playlists. trill can't pin or unpin, because Spotify ignores pin changes from third-party clients, so pin them from the official app.
- **Albums and Artists:** the two entries under Liked Songs list your saved albums and the artists you follow, most recent first, and sort like playlists. Opening an artist shows their top tracks, then their albums, singles, EPs and compilations, newest first.
- **Liking, saving, following:** `f` likes or unlikes the highlighted song, saves or unsaves the highlighted album, and follows or unfollows the highlighted artist. With nothing highlighted, it likes the song playing. Everything in your library shows a ♥, including the now-playing line. The ♥ changes straight away and changes back if Spotify refuses.
- **Smart shuffle:** `s` steps through off, shuffle and smart shuffle. Smart shuffle plays your list shuffled with a song Spotify recommends after every three, marked ✦ in the now-playing line and the queue (`c`). They come from the list's Spotify radio (playlists, albums, artists), or the playing song's radio for Liked Songs and search results. Like in Spotify, they're only played, not added; `f` or `A` keeps one you like.
- **Jump to the playing song:** `w` opens the list it's playing from and highlights it.
- **Play all:** `P` plays every track in the open list, opening the albums and playlists in it. On an artist page that's the whole discography, top tracks first, without repeats (about 1 s for ~100 songs).
- **Playlists:** `A` opens a picker of the playlists you can edit, which are yours and collaborative ones. A ✓ marks the ones that already have the song. `Enter` adds it, or removes it from a ✓ playlist, and the picker stays open for more until `Esc`. In one of your playlists, pressing `D` twice removes the highlighted song. trill re-reads the playlist from Spotify first, so an edit made on your phone in the meantime can't make it remove the wrong song.

These actions go through the same private Spotify API as the rest of trill (not the Web API), so no rate limits apply. Each one takes about 0.4–0.5 s to reach Spotify.
</details>

<details>
<summary><strong>Lyrics and album art</strong></summary>

- **Lyrics:** `L` swaps the track list for the current song's lyrics. When Spotify has timed lyrics, the current line is highlighted and kept in view. Otherwise, `j`/`k` scroll. Lyrics are only fetched while this view is open.
- **Album art:** the cover sits under the playlist list when the window is tall enough. Terminals with an image protocol show a real image. kitty, Ghostty, WezTerm, foot and iTerm2 are detected automatically, and Konsole uses the iTerm2 protocol. Everywhere else, including Alacritty, tmux and VS Code, the cover is drawn with coloured half-block characters. For a sharp image in VS Code, turn on `terminal.integrated.enableImages`.
</details>

<details>
<summary><strong>EQ, crossfade, quality and themes</strong></summary>

- **EQ:** `e` cycles through the presets: Flat, Bass Boost, Bass Reducer, Treble Boost, Vocal, Rock, Pop, Hip-Hop, Electronic, Acoustic, Loudness and Custom. `E` opens the 10-band panel (32 Hz to 16 kHz, ±12 dB). In the panel, `↑↓` picks a band, `←→` adjusts it and `0` resets it. A preamp is set automatically so boosts don't clip.
  - The EQ is a small PipeWire filter that only trill's audio passes through, so other apps aren't affected.
  - **On Flat it doesn't run at all.** The first non-flat setting starts it and briefly restarts the player (~3 s, then it resumes where you were). After that, changes apply live. Set `eq = off` in the config to disable it completely.
- **Crossfade:** `x` cycles through off, 3, 5, 8 and 12 s. This is Spotify's own crossfade inside Soloist. Applying it restarts Soloist (~3 s, then it resumes where you were).
- **Quality:** trill sets Soloist to 320 kbps. Out of the box, Soloist streams at 160 kbps.
- **Themes:** `t` cycles through spotify, ocean, violet, sunset, rose, mono and cover. `cover` takes the accent colour from the album art of the song that's playing:
  - Every pixel votes for its colour, weighted by how colourful it is, so a vivid title or detail wins over a large muted area.
  - Pastel and off-white covers keep their tint, and black-and-white covers get a light grey.
  - The colour is brightened until it reads on a dark terminal. The last colour is remembered, so the next start opens in it.
  - The colour comes from the cover trill already decodes for the art (~0.5–1 ms per song). With the art hidden, trill fetches the cover just for its colour (~50–100 KB per song).
  - `trill --debug-tint <search>` prints the colour picked for each result's cover.

All of these, plus shuffle, repeat, sort order and album art on or off, are saved in `~/.local/share/trill/audio_settings`. Volume is saved too, including changes made from KDE's media controls or your phone.
</details>

<details>
<summary><strong>Config</strong></summary>

The config is optional. It lives in `~/.config/trill/config`:

```ini
bitrate = 320          # 96 | 160 | 320
audio_cache_mb = 0     # >0 keeps played tracks on disk (saves bandwidth on repeats)
normalise = false
client_id =            # your own Spotify app, used ONLY for search (see below)
backend = auto         # auto | soloist | librespot
device_name = trill  # how Soloist shows up in Spotify Connect
soloist_cache_mb = 200
soloist_ws =           # host:port of a Soloist you already run, instead of spawning one
eq = on                # off = never run the EQ process
art = on               # off = no album art (and no terminal capability query)
```
</details>

<details>
<summary><strong>Better search (recommended)</strong></summary>

Playlists, liked songs, albums and artists all use librespot's own connection, so the public Web API's limits don't apply to them. Search, however, uses the Web API.

Without your own app, search borrows librespot's shared token, which every librespot app uses, so Spotify rate-limits it most of the time. To keep search fast either way, trill always runs a tracks-only search alongside the Web API one (about 0.3 s), and after one rate-limit error it stops asking the Web API for 2 minutes.

For reliable full search (tracks, albums, playlists and artists):

1. Go to https://developer.spotify.com/dashboard, click **Create app**, and choose Web API.
2. Add the redirect URI `http://127.0.0.1:8898/login`.
3. Put the Client ID in the config as `client_id = …`. On the next start, trill asks you to log in once more, for search only.

The status line shows which search path answered: `web api` (your app), `web api*` (the shared token), or "tracks only" (the fallback).
</details>

<details>
<summary><strong>macOS and Windows notes</strong></summary>

- **Which machines:** Apple Silicon Macs and x86_64 Windows, because those are the Soloist builds Spotify publishes. An Intel Mac can still build trill and try `backend = librespot`, if your account still works with librespot. ARM Windows runs the x86_64 build through emulation.
- **Same paths as Linux:** `~/.config/trill`, `~/.cache/trill` and `~/.local/share/trill` (on Windows, under `C:\Users\<you>`), not `~/Library` or `%APPDATA%`.
- **No EQ.** It's a PipeWire filter, and PipeWire is Linux-only. Picking an EQ setting says so instead of starting it.
- **Media keys:** on a Mac, play/pause, next and previous (keyboard, AirPods, Control Center, the lock screen) control trill, and Now Playing shows the song (no cover yet). Windows has no media-key support yet. Every key inside trill works everywhere, and your phone can control it through Spotify Connect.
- **Album art:** Ghostty, kitty, WezTerm and iTerm2 show the real cover, and so does Windows Terminal. Apple's Terminal can't draw images, so there the cover is coloured half-blocks and looks blurry and blocky; use one of the others on a Mac for a sharp cover. The old Windows console also uses half-blocks.
- **Quitting:** `q` stops Soloist with trill. If trill crashes, Soloist can keep running in the background (Linux stops it automatically). Run `pkill soloist` on a Mac or `taskkill /IM soloist.exe /F` on Windows if that happens.
- The [Numbers](#numbers) were measured on Linux. The glibc memory tricks in [Why it's light](#why-its-light) only apply on Linux.
</details>

<details>
<summary><strong>Debugging</strong></summary>

```bash
TRILL_LOG=debug trill              # writes ~/.cache/trill/trill.log
trill --debug-play [spotify:track:…] # play one track headless, print engine events
```

Soloist's own output goes to `~/.cache/trill/soloist.log`.
</details>

## FAQ

**Why not just librespot?**
On many accounts, Spotify no longer hands librespot the keys to decrypt audio. trill still uses librespot for everything except playback, and you can switch to it with `backend = librespot` if your account still works with it.

**Does it work without Premium?**
No. Both Soloist and librespot need a Premium account.

**Only CachyOS?**
No. It's tuned and tested there, but any Linux with PipeWire or PulseAudio works, and so do Apple Silicon Macs and Windows PCs (see [Get started](#get-started)).

**Can I use it in WSL?**
The Linux build should work under WSL2 with WSLg, which provides the audio. Pairing your phone with Soloist needs WSL's mirrored networking, though, so on Windows the native build is the easier choice.

**Why does it ask me to log in twice?**
It only does if you set your own `client_id`. That second login is for search alone.
