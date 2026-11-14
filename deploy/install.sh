#!/bin/sh
# Build lumbridge-libraryd and the two CLIs, install them for the current user, and (re)start the
# daemon as a systemd user service. Run from the repository root:
#
#   sh deploy/install.sh
#
# Environment (all optional):
#   CARGO       cargo to build with                       (default: cargo)
#   BIN         where the binaries go                      (default: ~/.local/bin)
#   UNIT        systemd user unit to (re)start             (default: lumbridge-libraryd)
#   NO_SERVICE  1 = install binaries and files only; do not touch systemd
#   HEALTH_URL  checked after the restart                  (default: http://127.0.0.1:8830/health)
#   DATA_ROOT   folder for the clipper venv                (default: ~/lumbridge-library)
#
# It writes: the binaries to $BIN, tools/extract.py to ~/.local/share/lumbridge-library/, a Python
# venv with trafilatura and pypdf to $DATA_ROOT/.venv (for clipping URLs), and the unit file to
# ~/.config/systemd/user/ when UNIT is lumbridge-libraryd and none is installed yet. It never touches
# the config, tokens, the vault, the mirrors or the database. Set those up first: see README.md.
set -eu
CARGO=${CARGO:-cargo}
BIN=${BIN:-$HOME/.local/bin}
UNIT=${UNIT:-lumbridge-libraryd}
UNITS="$HOME/.config/systemd/user"
SHARE="$HOME/.local/share/lumbridge-library"
DATA_ROOT=${DATA_ROOT:-$HOME/lumbridge-library}
HEALTH_URL=${HEALTH_URL:-http://127.0.0.1:8830/health}

[ -f deploy/lumbridge-libraryd.service ] || { echo "run from the repository root" >&2; exit 1; }

"$CARGO" build --release --locked -p lumbridge-libraryd -p lumbridge-library-cli
mkdir -p "$BIN"
for b in lumbridge-libraryd lumbridge-docs lumbridge-warehouse; do
  install -m 755 "target/release/$b" "$BIN/$b.new"
  mv "$BIN/$b.new" "$BIN/$b"
done

# The clipper: tools/extract.py and a venv for it ([clipper] in the config points at both). A
# folder from an earlier release (~/.local/share/library) is kept current while it exists.
mkdir -p "$SHARE"
install -m 644 tools/extract.py "$SHARE/extract.py"
[ -d "$HOME/.local/share/library" ] && install -m 644 tools/extract.py "$HOME/.local/share/library/extract.py"
if [ ! -x "$DATA_ROOT/.venv/bin/python" ]; then
  mkdir -p "$DATA_ROOT"
  python3 -m venv "$DATA_ROOT/.venv" && "$DATA_ROOT/.venv/bin/pip" install -q trafilatura pypdf \
    || echo "clipper venv not set up (python3-venv missing?); clipping a URL from the CLI will not work" >&2
fi

[ "${NO_SERVICE:-}" = 1 ] && { echo "installed to $BIN"; exit 0; }

if [ "$UNIT" = lumbridge-libraryd ] && [ ! -e "$UNITS/$UNIT.service" ]; then
  mkdir -p "$UNITS"
  cp deploy/lumbridge-libraryd.service "$UNITS/$UNIT.service"
fi
systemctl --user daemon-reload
systemctl --user enable "$UNIT"
systemctl --user restart "$UNIT"
sleep 2
curl -fsS "$HEALTH_URL" && echo
