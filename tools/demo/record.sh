#!/bin/sh
# Record the demo at the top of the README.
#
#   tools/demo/record.sh ICDU DIR OUT.svg
#
# ICDU is the binary to record (target/release/icdu). DIR is the directory it scans: the
# `home` directory of a tree made by tools/demo/tree.py. Its path is on screen, so make it a
# short one. OUT.svg is where the recording goes (.github/assets/demo.svg).
#
# The sample tree is small, and a scan of it takes about a second. So that there is a scan
# to watch, icdu runs with one scanner thread and under strace, which makes every system
# call slow. Everything on screen is what icdu drew; only the pace is not real.
#
# Needs tmux, strace and python3. Uses a tmux server of its own, so it does not touch your
# sessions.
set -eu

[ $# -eq 3 ] || { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
icdu=$(realpath "$1") dir=$2 out=$3
here=$(dirname "$(realpath "$0")")
columns=120 rows=28
frames=$(mktemp -d)
tmux="tmux -L icdu-demo-$$"
trap '$tmux kill-server 2>/dev/null || true; rm -rf "$frames"' EXIT

$tmux new-session -d -x "$columns" -y "$rows" \
    "env -u NO_COLOR strace -f -o /dev/null '$icdu' --ignore-config -t 1 '$dir'"
$tmux resize-window -x "$columns" -y "$rows"

list=
# shot NAME MILLISECONDS CAPTION: keep the screen as it is now as the next frame.
shot() {
    $tmux capture-pane -e -p > "$frames/$1.ansi"
    list="$list
$frames/$1.ansi:$2:$3"
}
key() { $tmux send-keys "$@"; sleep 0.25; }
# Wait until the screen shows TEXT (at most SECONDS).
await() {
    n=0
    until $tmux capture-pane -p | grep -q "$1"; do
        n=$((n + 1))
        [ "$n" -lt $(($2 * 5)) ] || { echo "record.sh: never saw '$1'" >&2; exit 1; }
        sleep 0.2
    done
}

scanning='The list is there at once. Sizes fill in while the scan is running.'
await 'scanning' 20
sleep 1.2
shot 01 1500 "$scanning"
sleep 1.0
shot 02 1300 "$scanning"

# Into `projects`, the seventh of eight when the list is in name order, and back to sizes.
key n
key k
for _ in 1 2 3 4 5 6; do key j; done
key Enter
key s
sleep 0.6
shot 03 2600 'Enter a directory, and it is scanned next.'

searching='/ searches the whole tree by name, including what the scan has just reached.'
key /
key -l ca
shot 04 1100 "$searching"
key -l che
sleep 0.8
shot 05 2600 "$searching"
key Down
shot 06 1500 "$searching"
key Enter
sleep 0.5
shot 07 2400 'Enter goes to the match.'

await 'done ' 120
key h
key h
shot 08 3200 'Keys, options, config files and dump format are those of ncdu.'

# The search popup is the frame to show where animations are off.
echo "$list" | sed '/^$/d' | tr '\n' '\0' |
    STILL=4 xargs -0 python3 "$here/frames.py" "$columns" "$rows" > "$out"
echo "wrote $out ($(wc -c < "$out") bytes)"
