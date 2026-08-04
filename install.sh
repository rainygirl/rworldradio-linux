#!/bin/sh
# Builds rworldradio and installs it so it shows up in the XFCE applications
# menu (Multimedia). Run from anywhere:
#
#   ./install.sh            # per-user install, no root needed (default)
#   ./install.sh --system   # system-wide into /usr/local, needs root
#   ./install.sh --uninstall [--system]
#
# Per-user layout (all standard XDG paths, so XFCE, GNOME and KDE all pick it up):
#   ~/.local/bin/rworldradio
#   ~/.local/share/rworldradio/data/            <- StationCache's XDG search path
#   ~/.local/share/applications/rworldradio.desktop
#   ~/.local/share/icons/hicolor/<size>/apps/rworldradio.png
#
# The Haiku original symlinked into Deskbar's menu directory and needed a reboot
# for packagefs to merge it. Nothing like that here - the freedesktop menu is
# read from these directories directly, and `update-desktop-database` is enough
# to refresh it (XFCE usually notices without even that).
set -e

cd "$(dirname "$0")"
PROJECT_DIR=$PWD
DATA_SRC="$PROJECT_DIR/data"

MODE=install
SCOPE=user
for arg in "$@"; do
	case "$arg" in
		--system) SCOPE=system ;;
		--uninstall) MODE=uninstall ;;
		--help|-h)
			# Print the header comment block, minus the shebang.
			awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"
			exit 0 ;;
		*)
			echo "install.sh: unknown option: $arg" >&2
			exit 2 ;;
	esac
done

if [ "$SCOPE" = system ]; then
	PREFIX=/usr/local
	BIN_DIR="$PREFIX/bin"
	DATA_DIR="$PREFIX/share/rworldradio/data"
	APPS_DIR="$PREFIX/share/applications"
	ICONS_DIR="$PREFIX/share/icons/hicolor"
	if [ "$(id -u)" != 0 ]; then
		echo "install.sh: --system needs root; re-run with sudo" >&2
		exit 1
	fi
else
	# XDG_DATA_HOME points at share/, so bin/ comes from HOME directly.
	BIN_DIR="$HOME/.local/bin"
	SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
	DATA_DIR="$SHARE_DIR/rworldradio/data"
	APPS_DIR="$SHARE_DIR/applications"
	ICONS_DIR="$SHARE_DIR/icons/hicolor"
fi

ICON_SIZES="16 24 32 48 64 128 256"

if [ "$MODE" = uninstall ]; then
	rm -f "$BIN_DIR/rworldradio"
	rm -f "$APPS_DIR/rworldradio.desktop"
	for size in $ICON_SIZES; do
		rm -f "$ICONS_DIR/${size}x${size}/apps/rworldradio.png"
	done
	rm -rf "$(dirname "$DATA_DIR")"
	command -v update-desktop-database >/dev/null 2>&1 \
		&& update-desktop-database "$APPS_DIR" 2>/dev/null || true
	echo "Uninstalled from $BIN_DIR / $APPS_DIR"
	exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
	echo "install.sh: cargo not found. Install Rust from https://rustup.rs" >&2
	exit 1
fi

if [ ! -f "$DATA_SRC/countries.json" ]; then
	echo "install.sh: no dataset at $DATA_SRC/countries.json" >&2
	echo "  (run tools/update_stations_db.py first)" >&2
	exit 1
fi

echo "Building (release)..."
cargo build --release

BINARY="$PROJECT_DIR/target/release/rworldradio"
[ -x "$BINARY" ] || { echo "install.sh: $BINARY missing after build" >&2; exit 1; }

mkdir -p "$BIN_DIR" "$DATA_DIR" "$APPS_DIR"
install -m 755 "$BINARY" "$BIN_DIR/rworldradio"

# Copy rather than symlink the dataset: a symlink into the source tree breaks the
# moment the checkout moves, and the whole point of the XDG path is that the
# installed app doesn't depend on the checkout still being there.
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"
cp "$DATA_SRC/countries.json" "$DATA_DIR/"
cp -r "$DATA_SRC/countries" "$DATA_DIR/"

# Rewrite Exec to the absolute path: a per-user install lands in ~/.local/bin,
# which is not guaranteed to be on the desktop session's PATH.
sed "s|^Exec=rworldradio$|Exec=$BIN_DIR/rworldradio|" \
	packaging/rworldradio.desktop > "$APPS_DIR/rworldradio.desktop"
chmod 644 "$APPS_DIR/rworldradio.desktop"
for size in $ICON_SIZES; do
	mkdir -p "$ICONS_DIR/${size}x${size}/apps"
	install -m 644 "packaging/rworldradio-${size}.png" \
		"$ICONS_DIR/${size}x${size}/apps/rworldradio.png"
done

command -v update-desktop-database >/dev/null 2>&1 \
	&& update-desktop-database "$APPS_DIR" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
	&& gtk-update-icon-cache -f -t "$ICONS_DIR" 2>/dev/null || true

echo
echo "Installed:"
echo "  $BIN_DIR/rworldradio"
echo "  $DATA_DIR ($(ls "$DATA_DIR/countries" | wc -l | tr -d ' ') country files)"
echo "  $APPS_DIR/rworldradio.desktop"
echo
echo "It should now be under Multimedia in the applications menu."
case ":$PATH:" in
	*":$BIN_DIR:"*) ;;
	*) echo "NOTE: $BIN_DIR is not on your PATH, so the 'rworldradio' command"
	   echo "      won't work from a shell. The menu entry will, since it runs"
	   echo "      through the desktop file. Log out and back in, or add it:"
	   echo "        export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
