#!/bin/sh
# Record the arca TUI demo GIF against fictional seed data — no real money is
# ever shown. Builds the binaries, seeds a throwaway DB, starts a demo daemon on
# temp sockets, then runs VHS (demo/demo.tape) to produce demo/arca.gif.
#
# Usage:  sh demo/record.sh
# Needs:  vhs (charmbracelet/vhs), and a Rust toolchain.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEMO=/tmp/arca-demo
BIN="$ROOT/target/release"

echo ">> building release binaries"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin arca --bin arca-daemon

echo ">> seeding fictional demo DB at $DEMO"
rm -rf "$DEMO"
mkdir -p "$DEMO/reports"
"$BIN/arca-daemon" seed-demo --db "$DEMO/arca.db"

cat > "$DEMO/arca.conf" <<EOF
[daemon]
db_path           = "$DEMO/arca.db"
log_path          = "$DEMO/arca.log"
read_socket_path  = "$DEMO/read.sock"
write_socket_path = "$DEMO/write.sock"
pid_path          = "$DEMO/arca.pid"
operator_uid      = $(id -u)
tcp_bind          = "127.0.0.1:0"
tz_display        = "America/Puerto_Rico"

[reports]
reports_dir = "$DEMO/reports"

[calendar]
ics_dir = "$DEMO/reports"
EOF

echo ">> starting demo daemon"
"$BIN/arca-daemon" --conf "$DEMO/arca.conf" >"$DEMO/daemon.out" 2>&1 &
DAEMON=$!
cleanup() { kill "$DAEMON" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Wait for the read socket to appear.
i=0
while [ ! -S "$DEMO/read.sock" ]; do
  i=$((i + 1))
  if [ "$i" -gt 60 ]; then
    echo "!! daemon did not create $DEMO/read.sock; log:" >&2
    cat "$DEMO/daemon.out" >&2
    exit 1
  fi
  sleep 0.1
done

echo ">> recording (vhs)"
cd "$ROOT"
PATH="$BIN:$PATH" vhs demo/demo.tape

# Shrink for the README: a flat-color terminal GIF reduces to a small palette
# with no visible loss. gifsicle would be ideal but isn't always present, so use
# a two-pass ffmpeg palette (no dither — exact theme colors compress best).
GIF="$ROOT/demo/arca.gif"
if command -v ffmpeg >/dev/null 2>&1; then
  echo ">> optimizing GIF (ffmpeg palette)"
  PAL="$DEMO/palette.png"
  OPT="$DEMO/arca-opt.gif"
  FILT="fps=20,scale=1280:-1:flags=lanczos"
  if ffmpeg -y -loglevel error -i "$GIF" -vf "$FILT,palettegen=max_colors=48" "$PAL" \
    && ffmpeg -y -loglevel error -i "$GIF" -i "$PAL" \
         -lavfi "$FILT,paletteuse=dither=none:diff_mode=rectangle" "$OPT"; then
    mv "$OPT" "$GIF"
  else
    echo "!! ffmpeg optimization failed; keeping the raw recording" >&2
  fi
fi

echo ">> wrote $GIF ($(( $(stat -c %s "$GIF") / 1024 )) KB)"
