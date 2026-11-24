#!/usr/bin/env python3
"""Builds the README graphics in assets/readme/: hand-laid SVGs in trill's look.

    TRILL_FONTS=/path/to/JetBrainsMono-2.304/fonts/ttf python3 assets/readme/make.py

Fonts: JetBrains Mono (SIL OFL 1.1, https://github.com/JetBrains/JetBrainsMono) for text, DejaVu
Sans Mono (free license) for the few symbols it lacks. Each SVG embeds a renamed subset of just the
characters it uses, so it looks the same on every machine and stays small. Needs fonttools + brotli.

The screenshots are real: `screens/*.ansi` are `tmux capture-pane -e` captures of trill itself
(140×42), rendered cell by cell; `screens/capture.sh` makes them. The only edit: the sidebar's
playlist names are swapped for neutral ones, since they're someone's own library. Borders,
half-block cover art and the progress bar are drawn as shapes, so they join without gaps at any zoom.

Every number below is in NUMBERS, measured on a release build (see the README's Numbers section).
"""

import base64, html, io, os, re
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent
FONTS = Path(os.environ.get("TRILL_FONTS", ""))
JB = {400: FONTS / "JetBrainsMono-Regular.ttf", 500: FONTS / "JetBrainsMono-Medium.ttf", 600: FONTS / "JetBrainsMono-SemiBold.ttf"}
FALLBACK = Path(os.environ.get("TRILL_FALLBACK", "/usr/share/fonts/TTF/DejaVuSansMono.ttf"))

# palette: trill's greys and its default theme (src/theme.rs: "spotify", green → cyan)
BG, EDGE, PANE, TILE = "#0d0d0d", "#1f1f1f", "#2a2a2a", "#1b1b1b"
TXT, MID, DIM = "#ececec", "#8f8f8f", "#5b5b5b"
ACC, ACC2 = "#1ed760", "#00afeb"
W = 880

JB_CMAP = set(TTFont(JB[400]).getBestCmap()) if JB[400].exists() else set()

LOGO = [  # src/ui.rs
    "   __       _ ____",
    "  / /______(_) / /",
    " / __/ ___/ / / / ",
    "/ /_/ /  / / / /  ",
    "\\__/_/  /_/_/_/   ",
]


def woff2(path, text, family):
    opts = subset.Options()
    opts.flavor = "woff2"
    opts.layout_features = []
    opts.hinting = False
    opts.notdef_outline = True
    opts.name_IDs = [1, 2, 4, 6]
    f = subset.load_font(str(path), opts)
    s = subset.Subsetter(opts)
    s.populate(text=text)
    s.subset(f)
    for rec in f["name"].names:  # renamed, as the OFL asks of modified versions
        if rec.nameID in (1, 4):
            rec.string = family
        elif rec.nameID == 6:
            rec.string = family.replace(" ", "")
    buf = io.BytesIO()
    subset.save_font(f, buf, opts)
    return base64.b64encode(buf.getvalue()).decode()


class Svg:
    def __init__(self, w, h, label):
        self.w, self.h, self.label = w, h, label
        self.els = []
        self.used = {400: set(), 500: set(), 600: set()}
        self.fallback = set()
        self.defs = []

    def add(self, s):
        self.els.append(s)

    def rect(self, x, y, w, h, fill, rx=0, stroke=None, extra=""):
        st = f' stroke="{stroke}"' if stroke else ""
        rr = f' rx="{rx}"' if rx else ""
        self.add(f'<rect x="{x:.2f}" y="{y:.2f}" width="{w:.2f}" height="{h:.2f}"{rr} fill="{fill}"{st}{extra}/>')

    def text(self, x, y, s, size=13, weight=400, fill=TXT, anchor="start", ls=None, fit=None):
        for c in s:
            (self.used[weight] if ord(c) in JB_CMAP or ord(c) < 128 else self.fallback).add(c)
        attrs = f' letter-spacing="{ls}"' if ls is not None else ""
        if fit:
            attrs += f' textLength="{fit:.2f}" lengthAdjust="spacingAndGlyphs"'
        self.add(f'<text x="{x:.2f}" y="{y:.2f}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}" xml:space="preserve"{attrs}>{html.escape(s, quote=False)}</text>')

    def gradient(self, gid, a=ACC, b=ACC2, x2="1", y2="0"):
        self.defs.append(f'<linearGradient id="{gid}" x1="0" y1="0" x2="{x2}" y2="{y2}"><stop offset="0" stop-color="{a}"/><stop offset="1" stop-color="{b}"/></linearGradient>')

    def card(self):
        self.rect(0, 0, self.w, self.h, BG, rx=6)
        self.rect(0.5, 0.5, self.w - 1, self.h - 1, "none", rx=6, stroke=EDGE)

    def pane(self, x, y, w, h, title, note=""):
        """A box with its title set into the top edge, like a pane in the app."""
        self.rect(x + 0.5, y + 0.5, w, h, "none", rx=3, stroke=PANE)
        tw = len(title) * 13 * 0.6 + 14
        self.rect(x + 12, y - 2, tw, 5, BG)
        self.text(x + 18, y + 4.5, title, 13, 500, MID)
        if note:
            nw = len(note) * 12 * 0.6 + 14
            self.rect(x + w - 10 - nw, y - 2, nw, 5, BG)
            self.text(x + w - 4, y + 4.5, note, 12, 400, DIM, "end")

    def save(self, name):
        faces = []
        for wgt, chars in self.used.items():
            if chars:
                faces.append(f"@font-face{{font-family:tm;font-weight:{wgt};src:url(data:font/woff2;base64,{woff2(JB[wgt], ''.join(sorted(chars)), 'trillmono')})}}")
        if self.fallback:
            faces.append(f"@font-face{{font-family:tf;src:url(data:font/woff2;base64,{woff2(FALLBACK, ''.join(sorted(self.fallback)), 'trillmono fallback')})}}")
        style = "\n".join(faces) + "\ntext{font-family:tm,tf,ui-monospace,SFMono-Regular,Menlo,Consolas,'DejaVu Sans Mono',monospace}"
        defs = f"<defs>{''.join(self.defs)}</defs>" if self.defs else ""
        out = (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.w} {self.h}" width="{self.w}" height="{self.h}" role="img" aria-label="{html.escape(self.label)}">\n'
            f"<style>\n{style}\n</style>\n{defs}\n" + "\n".join(self.els) + "\n</svg>\n"
        )
        (HERE / name).write_text(out, encoding="utf-8")
        print(f"{name}: {len(out) / 1024:.1f} KB")


def pill(s, x, y, label, lit=True, size=12):
    w = len(label) * size * 0.6 + 12
    if lit:
        s.rect(x, y - size + 1, w, size + 6, ACC, rx=2)
        s.text(x + w / 2, y + 1, label, size, 600, BG, "middle")
    else:
        s.text(x + w / 2, y + 1, label, size, 400, DIM, "middle")
    return w


def progress_bar(s, x, y, w, frac, h=5, gid="pg"):
    s.rect(x, y, w, h, TILE)
    s.rect(x, y, w * frac, h, f"url(#{gid})")


def pie(s, cx, cy, r, frac, color=ACC):
    """◔ ◑ ◕ as shapes (fonts disagree on them)."""
    import math

    s.add(f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="{color}" stroke-width="1.4"/>')
    if frac >= 1:
        s.add(f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="{color}"/>')
        return
    a = frac * 2 * math.pi
    x2, y2 = cx + r * math.sin(a), cy - r * math.cos(a)
    big = 1 if frac > 0.5 else 0
    s.add(f'<path d="M{cx} {cy}L{cx} {cy - r}A{r} {r} 0 {big} 1 {x2:.2f} {y2:.2f}z" fill="{color}"/>')


# ---------------------------------------------------------------------------- screenshots

PAL16 = ["#1e1e1e", "#f2777a", "#99cc99", "#ffcc66", "#6699cc", "#cc99cc", "#66cccc", "#d0d0d0", "#6b6b6b", "#ff8c8c", "#b5e8b5", "#ffe08a", "#8fb8e8", "#e0b0e0", "#8ce0e0", "#ffffff"]
TERM_FG, TERM_BG = "#d4d4d4", "#101012"


def xterm256(n):
    if n < 16:
        return PAL16[n]
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (lv[n // 36], lv[n // 6 % 6], lv[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


def mix(a, b, t):
    pa = [int(a[i : i + 2], 16) for i in (1, 3, 5)]
    pb = [int(b[i : i + 2], 16) for i in (1, 3, 5)]
    return "#%02x%02x%02x" % tuple(round(x + (y - x) * t) for x, y in zip(pa, pb))


def parse_ansi(path, cols):
    """tmux capture-pane -e → rows of cells (char, fg, bg, bold)."""
    rows = []
    for line in path.read_text(encoding="utf-8").split("\n"):
        fg, bg, bold, dim, rev = None, None, False, False, False
        cells = []
        for tok in re.split(r"(\x1b\[[0-9;]*m)", line):
            if tok.startswith("\x1b["):
                codes = [int(c) if c else 0 for c in tok[2:-1].split(";")] or [0]
                i = 0
                while i < len(codes):
                    c = codes[i]
                    if c == 0:
                        fg, bg, bold, dim, rev = None, None, False, False, False
                    elif c == 1:
                        bold = True
                    elif c == 2:
                        dim = True
                    elif c == 22:
                        bold = dim = False
                    elif c == 7:
                        rev = True
                    elif c == 27:
                        rev = False
                    elif 30 <= c <= 37:
                        fg = PAL16[c - 30]
                    elif 90 <= c <= 97:
                        fg = PAL16[c - 90 + 8]
                    elif 40 <= c <= 47:
                        bg = PAL16[c - 40]
                    elif 100 <= c <= 107:
                        bg = PAL16[c - 100 + 8]
                    elif c == 39:
                        fg = None
                    elif c == 49:
                        bg = None
                    elif c in (38, 48) and i + 1 < len(codes):
                        if codes[i + 1] == 5:
                            col = xterm256(codes[i + 2])
                            i += 2
                        else:
                            col = "#%02x%02x%02x" % tuple(codes[i + 2 : i + 5])
                            i += 4
                        if c == 38:
                            fg = col
                        else:
                            bg = col
                    i += 1
                continue
            for ch in tok:
                f, b = fg or TERM_FG, bg or TERM_BG
                if rev:
                    f, b = b, f
                if dim:
                    f = mix(f, b, 0.45)
                cells.append((ch, f, b, bold))
        cells += [(" ", TERM_FG, TERM_BG, False)] * (cols - len(cells))
        rows.append(cells[:cols])
    return rows


BOX = set("─│╭╮╰╯")
BLOCKS = {"▀": "top", "▄": "bottom", "█": "full", "▏": 1, "▎": 2, "▍": 3, "▌": 4, "▋": 5, "▊": 6, "▉": 7}


def screenshot(name, ansi, label, cols=140, rows=42, crop=None):
    cells = parse_ansi(HERE / "screens" / ansi, cols)[:rows]
    if crop:
        cells = cells[crop[0] : crop[1]]
    fs, cw, lh = 12.5, 7.5, 16.5
    pad, bar = 16, 30
    tw, th = cols * cw, len(cells) * lh
    sw, sh = tw + pad * 2, th + pad * 2 + bar
    s = Svg(round(sw), round(sh), label)
    s.rect(0, 0, sw, sh, TERM_BG, rx=10)
    s.rect(0.5, 0.5, sw - 1, sh - 1, "none", rx=10, stroke="#2c2c30")
    s.rect(1, 1, sw - 2, bar - 1, "#18181b", rx=9)
    s.rect(1, bar - 8, sw - 2, 8, "#18181b")
    s.add(f'<line x1="1" y1="{bar}" x2="{sw - 1}" y2="{bar}" stroke="#2c2c30"/>')
    for i, c in enumerate(["#ff5f57", "#febc2e", "#28c840"]):
        s.add(f'<circle cx="{20 + i * 18}" cy="{bar / 2}" r="5.5" fill="{c}"/>')
    s.text(sw / 2, bar / 2 + 4.5, "trill", 12, 500, "#9a9aa0", "middle")
    ox, oy = pad, bar + pad
    for r, row in enumerate(cells):
        y = oy + r * lh
        # backgrounds, merged into runs
        c = 0
        while c < cols:
            b = row[c][2]
            e = c
            while e < cols and row[e][2] == b:
                e += 1
            if b != TERM_BG:
                s.rect(ox + c * cw, y, (e - c) * cw + 0.3, lh + 0.3, b)
            c = e
        # shapes for borders and blocks; text in runs of one style
        c = 0
        while c < cols:
            ch, f, b, bold = row[c]
            x = ox + c * cw
            if ch in BOX:
                mx, my = x + cw / 2, y + lh / 2
                d = {"─": f"M{x} {my}H{x + cw}", "│": f"M{mx} {y}V{y + lh}",
                     "╭": f"M{x + cw} {my}H{mx + 3}Q{mx} {my} {mx} {my + 3}V{y + lh}", "╮": f"M{x} {my}H{mx - 3}Q{mx} {my} {mx} {my + 3}V{y + lh}",
                     "╰": f"M{mx} {y}V{my - 3}Q{mx} {my} {mx + 3} {my}H{x + cw}", "╯": f"M{mx} {y}V{my - 3}Q{mx} {my} {mx - 3} {my}H{x}"}[ch]
                s.add(f'<path d="{d}" fill="none" stroke="{f}" stroke-width="1.1"/>')
                c += 1
                continue
            if ch in BLOCKS:
                k = BLOCKS[ch]
                if k == "top":
                    s.rect(x, y, cw + 0.3, lh / 2 + 0.2, f)
                elif k == "bottom":
                    s.rect(x, y + lh / 2, cw + 0.3, lh / 2 + 0.3, f)
                elif k == "full":
                    s.rect(x, y, cw + 0.3, lh + 0.3, f)
                else:
                    s.rect(x, y, cw * k / 8, lh + 0.3, f)
                c += 1
                continue
            if ch == "⏸":
                s.rect(x + 1.5, y + 4, 1.8, lh - 8, f)
                s.rect(x + 4.6, y + 4, 1.8, lh - 8, f)
                c += 1
                continue
            if ch == "⤓":  # no font here has it: an arrow down onto a bar
                mx = x + cw / 2
                s.add(f'<path d="M{mx} {y + 3.5}V{y + lh - 6.5}M{mx - 2.6} {y + lh - 9}L{mx} {y + lh - 6.2}L{mx + 2.6} {y + lh - 9}" fill="none" stroke="{f}" stroke-width="1.3"/>')
                s.rect(x + 1, y + lh - 4.6, cw - 2, 1.4, f)
                c += 1
                continue
            e = c
            while e < cols and row[e][0] not in BOX and row[e][0] not in BLOCKS and row[e][0] not in "⏸⤓" and row[e][1] == f and row[e][3] == bold:
                e += 1
            run = "".join(x[0] for x in row[c:e])
            if run.strip():
                s.text(x, y + lh - 4.5, run, fs, 600 if bold else 400, f, fit=(e - c) * cw)
            c = e
    s.save(name)



# ---------------------------------------------------------------------------- numbers
# Measured on v0.6.0, i9-14900HX, CachyOS, Soloist backend, a library of 3,953 liked songs and
# 110 playlists, in tmux at 140×42 (see the README for how). Memory is PSS.
NUMBERS = {
    "pss_start": "15.8 MB",
    "pss_playing": "19.2 MB",
    "pss_foryou": "19.6 MB",
    "soloist": "~50 MB",
    "cpu_playing": "0.2%",
    "first_frame": "~50 ms",
    "binary": "6.5 MB",
    "foryou_fresh": "~2 s",
    "foryou_reopen": "5 ms",
}
N = NUMBERS


# ---------------------------------------------------------------------------- hero


def hero():
    h = 352
    s = Svg(W, h, f"trill: Spotify in your terminal. It plays through Spotify's own player, Soloist. About {N['pss_start']} of memory at start, 0% CPU while idle, the library on screen in {N['first_frame']}, one {N['binary']} program.")
    s.gradient("lg", x2="1", y2="0")
    s.gradient("pg")
    s.card()
    for i, row in enumerate(LOGO):
        s.text(52, 74 + i * 25, row, 22, 600, "url(#lg)")
    x = 400
    s.text(x, 82, "Spotify in your terminal.", 27, 500, TXT, ls="-.6")
    s.text(x, 112, "Spotify's own player inside. Quiet when idle.", 14, 400, MID)
    s.add(f'<path d="M{x} 151L{x + 8.5} 155.5L{x} 160z" fill="{ACC}"/>')
    s.text(x + 18, 160, "Dreams", 13, 600, ACC)
    s.text(x + 18 + 8 * 7.8, 160, "Fleetwood Mac", 13, 400, MID)
    for i, (lit, icon) in enumerate([(True, "⇄"), (False, "↻"), (False, "⊘")]):
        pill(s, W - 44 - 3 * 26 + i * 26, 160, icon, lit)
    s.text(x, 186, "1:42", 11, 400, MID)
    progress_bar(s, x + 36, 180, W - 44 - x - 36 - 38, 0.4)
    s.text(W - 44, 186, "4:17", 11, 400, MID, "end")
    s.add(f'<line x1="44" y1="238" x2="{W - 44}" y2="238" stroke="{PANE}"/>')
    stats = [(N["pss_start"], "memory at start"), ("0%", "CPU while idle"), (N["first_frame"], "to your library"), (N["binary"], "one program")]
    cw = (W - 88) / 4
    for i, (big, small) in enumerate(stats):
        cx = 44 + i * cw
        if i:
            s.add(f'<line x1="{cx - 16:.1f}" y1="260" x2="{cx - 16:.1f}" y2="320" stroke="{PANE}"/>')
        s.text(cx, 290, big, 25, 500, TXT, ls="-.5")
        s.text(cx, 314, small, 12, 400, MID)
    s.save("hero.svg")


# ---------------------------------------------------------------------------- what's different


def different():
    cards = [
        ("Plays where librespot can't", ["Audio comes from Soloist, Spotify's own player,", "so it works on accounts that refuse librespot."]),
        ("Small and quiet", [f"About {N['pss_start']} of memory, and no CPU or timers", "while idle. It redraws only when something changes."]),
        ("For You, worked out here", ["Suggestions from what you play, ranked on your", "computer. Your play history stays on it."]),
        ("Your whole library", ["Playlists, folders, pins, albums, artists, lyrics.", "Through Spotify's own API: no Web API limits."]),
    ]
    h = 290
    s = Svg(W, h, "What's different about trill: " + "; ".join(f"{t}: " + " ".join(ls) for t, ls in cards))
    s.card()
    s.pane(24, 26, W - 49, h - 53, "what's different")
    cw, ch = (W - 88 - 14) / 2, 94
    for i, (title, lines) in enumerate(cards):
        x, top = 44 + (i % 2) * (cw + 14), 52 + (i // 2) * (ch + 14)
        s.rect(x + 0.5, top + 0.5, cw, ch, TILE, rx=4, stroke=PANE)
        s.rect(x + 16, top + 20, 3, 18, ACC, rx=1.5)
        s.text(x + 28, top + 34, title, 14, 600, TXT)
        for j, l in enumerate(lines):
            s.text(x + 16, top + 60 + j * 17, l, 11, 400, MID)
    s.save("different.svg")


# ---------------------------------------------------------------------------- features


def features():
    items = [
        ("/", "Search", "songs, albums, artists, playlists"),
        ("✦", "For You", "picks from what you play, X to skip"),
        ("s", "Smart shuffle", "a recommendation after every three"),
        ("L", "Synced lyrics", "the line being sung stays in view"),
        ("i", "Album art", "real images, colour blocks elsewhere"),
        ("e", "10-band EQ", "11 presets, Linux (PipeWire)"),
        ("x", "Crossfade", "Spotify's own, gapless at 320 kbps"),
        ("z", "Sleep timer", "15–90 min or the end of the song"),
        ("F", "Find in a list", "narrows as you type, o sorts it"),
        ("f", "Like, save, follow", "A adds to playlists, ✓ shows where"),
        ("I", "Private session", "hidden from friends, Discord too"),
        ("▶", "Media keys", "MPRIS on Linux, Now Playing on macOS"),
    ]
    h = 448
    s = Svg(W, h, "What trill does, by key: " + "; ".join(f"{k}: {a}, {b}" for k, a, b in items) + ". Plus pins and folders, artist pages with play counts, themes and the mouse.")
    s.card()
    s.pane(24, 26, W - 49, h - 53, "features", "press ? in trill for every key")
    for i, (k, a, b) in enumerate(items):
        col, row = i % 2, i // 2
        x, y = 52 + col * 400, 62 + row * 56
        s.rect(x + 0.5, y + 0.5, 29, 29, TILE, rx=3, stroke=PANE)
        if k == "▶":
            s.add(f'<path d="M{x + 11} {y + 9}L{x + 21} {y + 15}L{x + 11} {y + 21}z" fill="{ACC}"/>')
        else:
            s.text(x + 15, y + 20, k, 14, 600, ACC, "middle")
        s.text(x + 46, y + 12, a, 14, 500, TXT)
        s.text(x + 46, y + 30, b, 12, 400, MID)
    s.text(W / 2, h - 44, "plus pins and folders · artist pages with play counts · themes · the mouse", 12, 400, DIM, "middle")
    s.save("features.svg")


# ---------------------------------------------------------------------------- for you


def foryou():
    h = 280
    s = Svg(W, h, "How For You works. 1: what you play: every song played for 30 seconds, logged on your computer, older plays counting less. 2: your seeds: your top songs and top artists. 3: Spotify's radio of those songs and the artists related to yours, fetched at once and kept for 12 hours. 4: ranked here: liked songs left out, songs you play a lot pushed down, no one artist takes over, and X hides a song for good. Your play history stays on your computer.")
    s.card()
    s.pane(24, 26, W - 49, h - 53, "how For You works", "your play history stays on your computer")
    steps = [
        ("1", "What you play", ["each song after 30 s,", "older plays count less"]),
        ("2", "Your seeds", ["your top songs and", "your top artists"]),
        ("3", "Ask Spotify", ["their radio, related", "artists; kept 12 hours"]),
        ("4", "Ranked here", ["liked: left out", "played a lot: down"]),
    ]
    s.defs.append(f'<marker id="fa" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path d="M0 0L8 4L0 8z" fill="{DIM}"/></marker>')
    cw = (W - 88 - 3 * 28) / 4
    for i, (n, title, lines) in enumerate(steps):
        x = 44 + i * (cw + 28)
        top = 60
        hot = i == 3
        s.rect(x + 0.5, top + 0.5, cw, 128, TILE, rx=4, stroke=ACC if hot else PANE)
        s.add(f'<circle cx="{x + 26}" cy="{top + 30}" r="12" fill="{ACC if hot else PANE}"/>')
        s.text(x + 26, top + 34.5, n, 13, 600, BG if hot else TXT, "middle")
        s.text(x + 16, top + 70, title, 14, 600, ACC if hot else TXT)
        for j, l in enumerate(lines):
            s.text(x + 16, top + 94 + j * 17, l, 11.5, 400, MID)
        if i < 3:
            ax = x + cw + 4
            s.add(f'<path d="M{ax} {top + 64}L{ax + 19} {top + 64}" stroke="{DIM}" stroke-width="1.2" fill="none" marker-end="url(#fa)"/>')
    s.text(W / 2, 222, "No artist gets more than 3 songs. The Why column says where each came from; X hides one for good.", 12, 400, MID, "middle")
    s.save("foryou.svg")


# ---------------------------------------------------------------------------- get started


def start():
    steps = [
        ("1", "Install", ["one command,", "see below"]),
        ("2", "Soloist key", ["free, from Spotify's", "developer dashboard"]),
        ("3", "Get Soloist", ["trill --update-soloist", "downloads it"]),
        ("4", "Pair, play", ["trill --pair once,", "then just trill"]),
    ]
    h = 190
    s = Svg(W, h, "Get started in four steps, about ten minutes, once. 1: install trill with one command. 2: get a free Soloist API key from Spotify's developer dashboard and save it. 3: run trill --update-soloist to download Spotify's player. 4: run trill --pair once with your phone, then just trill.")
    s.card()
    s.pane(24, 26, W - 49, h - 53, "get started", "about ten minutes, once")
    cw = (W - 88 - 3 * 24) / 4
    s.defs.append(f'<marker id="sa" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path d="M0 0L8 4L0 8z" fill="{DIM}"/></marker>')
    for i, (n, title, lines) in enumerate(steps):
        x = 44 + i * (cw + 24)
        top = 54
        s.add(f'<circle cx="{x + 18}" cy="{top + 22}" r="13" fill="{ACC}"/>')
        s.text(x + 18, top + 27, n, 14, 600, BG, "middle")
        s.text(x + 40, top + 27, title, 15, 600, TXT)
        for j, l in enumerate(lines):
            s.text(x + 40, top + 52 + j * 18, l, 12, 400, MID)
        if i < 3:
            ax = x + cw - 6
            s.add(f'<path d="M{ax} {top + 22}L{ax + 22} {top + 22}" stroke="{DIM}" stroke-width="1.2" fill="none" marker-end="url(#sa)"/>')
    s.save("get-started.svg")


# ---------------------------------------------------------------------------- numbers


def numbers():
    h = 300
    s = Svg(W, h, f"Footprint, measured on an i9-14900HX, memory as PSS. trill at start: {N['pss_start']}; playing from Liked Songs, 3,953 songs: {N['pss_playing']}; with For You open: {N['pss_foryou']}; Soloist, Spotify's player, while playing: {N['soloist']}. CPU: 0% idle and paused, about {N['cpu_playing']} playing. The library on screen {N['first_frame']} after launch; For You worked out in {N['foryou_fresh']} and reopened in {N['foryou_reopen']}.")
    s.gradient("pg")
    s.card()
    s.pane(24, 26, W - 49, h - 53, "footprint", "measured · PSS · i9-14900HX")
    mb = lambda v: float(v.strip("~ MB"))
    bars = [("trill", "at start", N["pss_start"], True), ("trill", "playing, 3,953 liked songs", N["pss_playing"], True), ("trill", "with For You open", N["pss_foryou"], True), ("Soloist", "Spotify's player, playing", N["soloist"], False)]
    full = max(mb(b[2]) for b in bars)
    x0, bw = 330, W - 330 - 140
    for i, (who, what, label, ours) in enumerate(bars):
        y = 66 + i * 34
        s.text(52, y + 13, who, 13, 600 if ours else 400, ACC if ours else MID)
        s.text(132, y + 13, what, 12, 400, MID)
        w = bw * mb(label) / full
        s.rect(x0, y + 2, w, 14, "url(#pg)" if ours else "#3a3a3a", rx=2)
        s.text(x0 + w + 10, y + 14, label, 13, 500, TXT if ours else MID)
    s.add(f'<line x1="52" y1="206" x2="{W - 52}" y2="206" stroke="{PANE}"/>')
    stats = [("0%", "CPU idle and paused"), (N["cpu_playing"], "CPU playing"), (N["first_frame"], "launch to library"), (N["foryou_reopen"], "reopen For You")]
    cw = (W - 104) / 4
    for i, (big, small) in enumerate(stats):
        x = 52 + i * cw
        s.text(x, 240, big, 20, 500, TXT, ls="-.4")
        s.text(x, 260, small, 12, 400, MID)
    s.save("numbers.svg")


# ---------------------------------------------------------------------------- how it works


def how():
    h = 330
    s = Svg(W, h, "How it works. trill runs its screen and networking on one async thread, with keys read on a thread of their own. It browses your library through librespot's session, the same private API the official apps use, so no Web API limits apply, and keeps your lists on disk so they open at once. It drives Soloist, Spotify's own headless player, over a local WebSocket; Soloist streams from Spotify and plays through PipeWire, with the EQ when it's on. The desktop gets MPRIS on Linux and Now Playing on macOS.")
    s.card()
    s.pane(24, 26, W - 49, h - 53, "how it works", "Premium account · no Web API limits")

    def box(x, y, w, hgt, title, lines, hot=False):
        s.rect(x + 0.5, y + 0.5, w, hgt, TILE if hot else BG, rx=4, stroke=ACC if hot else PANE)
        s.text(x + 14, y + 22, title, 13, 600, ACC if hot else TXT)
        for j, l in enumerate(lines):
            s.text(x + 14, y + 42 + j * 17, l, 11.5, 400, MID)

    def arrow(x1, y1, x2, y2, label=""):
        s.add(f'<path d="M{x1} {y1}L{x2} {y2}" stroke="{DIM}" stroke-width="1.2" fill="none" marker-end="url(#ah)"/>')
        if label:
            s.text((x1 + x2) / 2, (y1 + y2) / 2 - 6, label, 10.5, 400, DIM, "middle")

    s.defs.append(f'<marker id="ah" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path d="M0 0L8 4L0 8z" fill="{DIM}"/></marker>')
    box(52, 58, 196, 92, "Spotify", ["your library, search,", "lyrics, radio: through", "librespot's session"])
    box(52, 176, 196, 92, "On disk", ["lists, settings, plays:", "open at once, refresh", "in the background"])
    box(318, 58, 244, 120, "trill", ["one async thread:", "screen + network", "redraws only on change", "keys on their own thread"], hot=True)
    box(318, 200, 244, 68, "Soloist", ["Spotify's own player,", "driven over a WebSocket"])
    box(632, 58, 196, 92, "Desktop", ["media keys, widgets:", "MPRIS · Now Playing", "your phone: Connect"])
    box(632, 176, 196, 92, "Sound", ["PipeWire, with the EQ", "when it's on (Linux);", "CoreAudio · Windows"])
    arrow(248, 104, 316, 104, "browse")
    arrow(248, 222, 316, 160)
    arrow(440, 178, 440, 198)
    arrow(562, 104, 630, 104)
    arrow(562, 234, 630, 222)
    s.save("how-it-works.svg")


# ---------------------------------------------------------------------------- keys


def keys():
    groups = [
        ("Move", [("↑↓ jk", "move"), ("g G", "top / bottom"), ("Tab h l", "switch pane"), ("Enter", "open · play"), ("Esc", "back"), ("w", "to what's playing"), ("?", "every key")]),
        ("Play", [("Space", "play / pause"), ("n p", "next / previous"), ("← →", "seek ±10 s"), ("+ -", "volume ±5%"), ("s r", "shuffle · repeat"), ("x z", "crossfade · sleep"), ("P", "play all")]),
        ("Library", [("f", "like · save · follow"), ("a", "add to queue"), ("A", "add to a playlist"), ("D D", "remove"), ("X", "not interested"), ("I", "private session"), ("y Y", "copy link")]),
        ("Find", [("/", "search"), ("F", "find in a list"), ("o O", "sort this list"), ("u U", "album · artist"), ("c", "the queue"), ("L i t", "lyrics · art · theme"), ("e E", "EQ")]),
    ]
    h = 346
    s = Svg(W, h, "Every key, grouped as in trill's help: " + "; ".join(f"{g}: " + ", ".join(f"{k} {d}" for k, d in ks) for g, ks in groups))
    s.card()
    s.pane(24, 26, W - 49, h - 53, "keys", "? in trill shows them all")
    cw = (W - 88) / 4
    for i, (g, ks) in enumerate(groups):
        x = 52 + i * cw
        s.text(x, 64, g, 13, 600, ACC)
        for j, (k, d) in enumerate(ks):
            y = 92 + j * 28
            kw = len(k) * 12 * 0.6 + 12
            s.rect(x + 0.5, y - 14.5, kw, 20, TILE, rx=3, stroke=PANE)
            s.text(x + kw / 2, y, k, 12, 600, TXT, "middle")
            s.text(x + kw + 8, y, d, 11, 400, MID)
    s.save("keys.svg")


if __name__ == "__main__":
    if not JB[400].exists():
        raise SystemExit("set TRILL_FONTS to JetBrains Mono's fonts/ttf directory (see the top of this file)")
    hero()
    different()
    features()
    foryou()
    start()
    numbers()
    how()
    keys()
    screenshot("screen-library.svg", "screen-library.ansi", "trill playing Dreams by Fleetwood Mac from the album Rumours: the Library sidebar with Liked Songs, Albums, Artists, For You, Queue, Search and playlists, the album cover drawn in colour blocks under it, the album's songs with their play counts, and the now-playing line with the progress bar.")
    screenshot("screen-foryou.svg", "screen-foryou.ansi", "trill's For You: fifty songs picked from what you play, each with a Why column saying where it came from, such as the radio of a song you play or an artist like one you play.")
