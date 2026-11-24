#!/bin/bash
# Captures screens/*.ansi from the real trill in tmux at 140×42, for make.py.
#
#   TRILL=target/release/trill HOMES=/path/to/copies assets/readme/screens/capture.sh
#
# HOMES holds copies of config/, cache/ and data/ (each with a trill/ inside, logged in), used as
# XDG_CONFIG_HOME, XDG_CACHE_HOME and XDG_DATA_HOME. Use copies: this writes a play log there.
# trill runs with --preview: it shows a song as playing without playing it or starting Soloist.
# For You is worked out from a short play log of well-known songs written below. The sidebar's
# playlist names are then replaced, by position, with neutral ones.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${TRILL:?path to a trill binary}" "${HOMES:?folder with config/, cache/ and data/ copies}"
TRILL=$(realpath "$TRILL")
sock=trill-readme
# the default green theme, like the other README graphics
sed -i 's/^theme=.*/theme=spotify/' "$HOMES/data/trill/audio_settings"

run() { # run <args…>: start trill in a fresh tmux server
    tmux -L $sock kill-server 2>/dev/null || true
    tmux -L $sock new-session -d -x 140 -y 42 -e XDG_CONFIG_HOME="$HOMES/config" -e XDG_CACHE_HOME="$HOMES/cache" \
        -e XDG_DATA_HOME="$HOMES/data" -e TRILL_LOG= "$TRILL" --preview "$@"
}
wait_for() { # wait_for <text>: until the pane shows it (30 s at most)
    for _ in $(seq 300); do tmux -L $sock capture-pane -p | grep -q "$1" && return; sleep 0.1; done
    echo "timed out waiting for: $1" >&2; exit 1
}
grab() { sleep 1.5; tmux -L $sock capture-pane -ep > "$here/$1"; }

# 1. playing Dreams from Rumours
run spotify:track:5TDZyWDfbQFQJabbPwImVY spotify:album:1bt6q2SruMsBtcerNVtpZB "Rumours — Fleetwood Mac"
wait_for "Gold Dust Woman"
tmux -L $sock send-keys Tab j
grab screen-library.ansi

# 2. For You, from a play log of 16 well-known songs, 3 plays each an hour apart, last week
# (15 songs or more, so it doesn't borrow from the account's liked songs)
now=$(date +%s)
: > "$HOMES/data/trill/plays"
i=0
while IFS='|' read -r uri title artist; do
    for _ in 1 2 3; do printf '%s\tspotify:track:%s\t%s\t%s\n' $((now - 7 * 86400 + i * 3600)) "$uri" "$title" "$artist" >> "$HOMES/data/trill/plays"; i=$((i + 1)); done
done <<'SONGS'
0GjEhVFGZW8afUYGChu3Rr|Dancing Queen|ABBA
2grjqo0Frpf2okIBiifQKs|September|Earth, Wind & Fire
0DiWol3AO6WpXZgp0goxAV|One More Time|Daft Punk
6dGnYIeXmHdcikdzNNDMm2|Here Comes The Sun|The Beatles
1NHWG8zxSEypSRF3UufrnO|Don't Stop Me Now|Queen
2WfaOiMkCvy7F5fcp2zZ8L|Take on Me|a-ha
4N0TP4Rmj6QQezWV88ARNJ|Superstition|Stevie Wonder
5TDZyWDfbQFQJabbPwImVY|Dreams|Fleetwood Mac
2JiDi0qAXsPwhPqA2qaKGt|Bohemian Rhapsody|Queen
7J1uxwnxfQLu4APicE5Rnj|Billie Jean|Michael Jackson
2374M0fQpWi3dLnB54qaLX|Africa|TOTO
2RlgNHKcydI9sayD2Df2xp|Mr. Blue Sky|Electric Light Orchestra
4y1LsJpmMti1PfRQV9AWWe|Girls Just Want to Have Fun|Cyndi Lauper
4qL9VYPB4XwJKAOq4xn2Mh|Le Freak|CHIC
32OlwWuMpZ6b0aN2RZOeMS|Uptown Funk|Mark Ronson, Bruno Mars
6kopmMZiyLmw7h66uXcXR7|Valerie|Mark Ronson, Amy Winehouse
SONGS
# Spotify's radio changes over time: check the capture, and mark anything unsuitable for a README
# "not interested" here (what X does in the app).
printf '%s\n' spotify:track:5D5Iwi9xI64mqdQOU9lTui >> "$HOMES/data/trill/not_interested"
rm -f "$HOMES/cache/trill/lists/trill_foryou" # worked out afresh from that log
run spotify:track:0GjEhVFGZW8afUYGChu3Rr
wait_for "Liked Songs"
sleep 2
tmux -L $sock send-keys g j j j Enter
wait_for "For You: [0-9]"
grab screen-foryou.ansi
tmux -L $sock send-keys q
sleep 1
tmux -L $sock kill-server 2>/dev/null || true

# 3. neutral playlist names in the sidebar (the rows after Search, by position)
python3 - "$here"/screen-*.ansi <<'EOF'
import re, sys
NAMES = ["Morning coffee", "Road trip", "Focus", "Sunday", "Running", "Rainy day", "Throwbacks", "Dinner party",
         "Chill", "Old favourites", "Summer", "Late night", "Workout", "Acoustic", "Weekend", "Piano", "Jazz", "Indie"]
ESC = re.compile(r"\x1b\[[0-9;]*m")
def cells(line):  # [(escapes before it, char)]
    out, pre, i = [], "", 0
    while i < len(line):
        m = ESC.match(line, i)
        if m:
            pre += m.group(); i = m.end(); continue
        out.append((pre, line[i])); pre = ""; i += 1
    return out, pre
for path in sys.argv[1:]:
    lines = open(path, encoding="utf-8").read().split("\n")
    after = next(i for i, l in enumerate(lines) if "⌕ Search" in l) + 1
    n = 0
    for r in range(after, len(lines)):
        cs, tail = cells(lines[r])
        if len(cs) < 32 or cs[0][1] != "│" or cs[31][1] != "│":
            break  # the end of the Library pane
        text = "".join(c for _, c in cs[1:31]).strip()
        if not text:
            continue
        name = NAMES[n % len(NAMES)][:30].ljust(30); n += 1
        border = cs[0][0]  # the pane border's style, for the right edge too
        lines[r] = border + "│\x1b[0m" + name + border + "".join(p + c for p, c in cs[31:]) + tail
    open(path, "w", encoding="utf-8").write("\n".join(lines))
EOF
echo "captured: $(ls "$here"/*.ansi | xargs -n1 basename | tr '\n' ' ')"
