#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
export HOME="$root/test home" XDG_DATA_HOME="$root/desktop-data" XDG_STATE_HOME="$root/state" XDG_CACHE_HOME="$root/cache"
export CARGO_TARGET_DIR="$root/build" PATH="$root/tools:$PATH" SLINT_EMIT_DEBUG_INFO=1 SLINT_MCP_PORT=9000
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Test GIT_AUTHOR_EMAIL=test@example.invalid GIT_COMMITTER_NAME=Test GIT_COMMITTER_EMAIL=test@example.invalid
mkdir -p "$HOME" "$root/tools"
cat >"$root/tools/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "build --release --locked" && ! -v SLINT_EMIT_DEBUG_INFO && ! -v SLINT_MCP_PORT ]]
version=$(<revision.txt)
[[ $version != broken ]]
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$version" >"$CARGO_TARGET_DIR/release/spofify-desktop"
chmod +x "$CARGO_TARGET_DIR/release/spofify-desktop"
CARGO
chmod +x "$root/tools/cargo"
remote=$root/remote.git
publish=$root/publisher
checkout=$root/checkout
git init --quiet --bare --initial-branch main "$remote"
git init --quiet --initial-branch main "$publish"
cp -R "$repo/Makefile" "$repo/assets" "$repo/packaging" "$repo/scripts" "$publish/"
git -C "$publish" remote add origin "$remote"
release() {
    printf '%s\n' "$1" >"$publish/revision.txt"
    git -C "$publish" add .
    git -C "$publish" commit --quiet -m "$1"
    git -C "$publish" push --quiet origin main
}
release first
git clone --quiet --no-local "$remote" "$checkout"
printf 'uncommitted\n' >"$checkout/revision.txt"
installed=$HOME/.local/bin/spofify-desktop
launcher=$XDG_DATA_HOME/applications/spofify-desktop.desktop
stamp=$XDG_STATE_HOME/spofify-desktop-deploy/revision
bash "$checkout/scripts/deploy-local.sh"
[[ $("$installed") == first && $(<"$checkout/revision.txt") == uncommitted ]]
grep -Fx "Exec=\"$installed\"" "$launcher"
[[ -f $XDG_DATA_HOME/icons/hicolor/scalable/apps/spofify-desktop.svg ]]
first_stamp=$(<"$stamp")
first_inode=$(stat -c %i "$installed")
bash "$checkout/scripts/deploy-local.sh"
[[ $(stat -c %i "$installed") == "$first_inode" ]]
release broken
if bash "$checkout/scripts/deploy-local.sh"; then
    echo 'A broken build was reported as installed' >&2
    exit 1
fi
[[ $("$installed") == first && $(<"$stamp") == "$first_stamp" ]]
release second
bash "$checkout/scripts/deploy-local.sh"
[[ $("$installed") == second && $(<"$checkout/revision.txt") == uncommitted ]]
[[ $(<"$stamp") == "$(git -C "$publish" rev-parse HEAD)" ]]
[[ $(stat -c %i "$installed") != "$first_inode" ]]
[[ -z $(find "$XDG_STATE_HOME" -name 'stage.*' -print -quit) ]]
printf 'Local deployment regression passed.\n'
