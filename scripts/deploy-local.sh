#!/usr/bin/env bash
# Build the published revision only. Never modify or build an open checkout.
set -euo pipefail
umask 077

repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
state=${XDG_STATE_HOME:-$HOME/.local/state}/spofify-desktop-deploy
cache=${XDG_CACHE_HOME:-$HOME/.cache}/spofify-desktop-deploy
bin=$HOME/.local/bin/spofify-desktop
data=${XDG_DATA_HOME:-$HOME/.local/share}
launcher=$data/applications/spofify-desktop.desktop
icon=$data/icons/hicolor/scalable/apps/spofify-desktop.svg
license=$data/licenses/spofify-desktop/JetBrainsMono-OFL.txt

# Quote the absolute executable for the desktop entry, including home paths
# with spaces. Reject characters that would need a second level of escaping.
unsupported='[[:cntrl:]"\`$%]'
if [[ $bin =~ $unsupported ]]; then
    echo 'Unsupported character in the desktop installation path.' >&2
    exit 1
fi

mkdir -p -- "$state" "$cache"
exec 9>"$state/deploy.lock"
flock -n 9 || exit 0

ref=refs/spofify-deploy/origin-main
git -C "$repo" fetch --quiet --no-tags origin "+refs/heads/main:$ref"
revision=$(git -C "$repo" rev-parse --verify "$ref^{commit}")
if [[ -x $bin && -f $launcher && -f $icon && -f $license && -f $state/revision ]] &&
    [[ $(<"$state/revision") == "$revision" ]]; then
    exit 0
fi

if ! pkg-config --exists alsa fontconfig && command -v brew >/dev/null; then
    brew_prefix=$(brew --prefix)
    export PKG_CONFIG_PATH="$brew_prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$cache/target}
unset SLINT_EMIT_DEBUG_INFO SLINT_MCP_PORT

stage=$(mktemp -d "$state/stage.XXXXXXXX")
pending=
trap 'rm -rf -- "$stage"; if [[ -n $pending ]]; then rm -f -- "$pending"; fi' EXIT
mkdir -- "$stage/source"
git -C "$repo" archive "$revision" | tar -x -C "$stage/source"
make -C "$stage/source" build
make -C "$stage/source" install PREFIX=/usr DESTDIR="$stage/package"
package=$stage/package/usr
# The launcher works even when the GUI session omits ~/.local/bin from PATH.
awk -v executable="$bin" '/^Exec=/ { print "Exec=\"" executable "\""; next } { print }' \
    "$package/share/applications/spofify-desktop.desktop" >"$stage/launcher.desktop"
desktop-file-validate "$stage/launcher.desktop"

publish() {
    local mode=$1 src=$2 dest=$3
    local dir
    dir=$(dirname -- "$dest")
    mkdir -p -- "$dir"
    pending=$(mktemp "$dir/.spofify-deploy.XXXXXXXX")
    install -m "$mode" -- "$src" "$pending"
    mv -fT -- "$pending" "$dest"
    pending=
}

publish 644 "$package/share/icons/hicolor/scalable/apps/spofify-desktop.svg" "$icon"
publish 644 "$package/share/licenses/spofify-desktop/JetBrainsMono-OFL.txt" "$license"
publish 644 "$stage/launcher.desktop" "$launcher"
# Rename instead of overwriting; an already-running player keeps its image.
publish 755 "$package/bin/spofify-desktop" "$bin"
printf '%s\n' "$revision" >"$stage/revision"
publish 600 "$stage/revision" "$state/revision"
printf 'Installed Spofify Desktop at %s (%s).\n' "$bin" "$revision"
