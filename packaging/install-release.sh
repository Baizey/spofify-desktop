#!/usr/bin/env bash
# Run from an extracted Spofify Desktop release. No root or Rust required.
set -euo pipefail
umask 077
src=$(cd -- "$(dirname -- "$0")" && pwd)
app=$HOME/.local/lib/spofify-desktop
data=${XDG_DATA_HOME:-$HOME/.local/share}
exe=$app/spofify-desktop
apps=$data/applications
icons=$data/icons/hicolor/scalable/apps
licenses=$data/licenses/spofify-desktop
unsupported='[[:cntrl:]"\`$%]'
if [[ ${HOME:-} != /* || $exe =~ $unsupported || $(uname -sm) != 'Linux x86_64' ]]; then
    echo 'This package requires Linux x86-64 and a supported home directory.' >&2
    exit 1
fi
mkdir -p -- "$app" "$apps" "$icons" "$licenses"
if [[ -L $app || ! -O $app || $(stat -c %a -- "$app") != 700 ]]; then
    echo 'The Spofify installation directory must be private (mode 700).' >&2
    exit 1
fi
exec 9<"$app"
flock 9
pending=
trap 'if [[ -n $pending ]]; then rm -f -- "$pending"; fi' EXIT
publish() {
    local mode=$1 input=$2 output=$3
    pending=$(mktemp "$(dirname -- "$output")/.spofify-install.XXXXXXXX")
    if [[ $input == - ]]; then
        cat >"$pending"
        chmod "$mode" "$pending"
    else
        install -m "$mode" -- "$input" "$pending"
    fi
    mv -fT -- "$pending" "$output"
    pending=
}
publish 644 "$src/spofify-desktop.svg" "$icons/spofify-desktop.svg"
publish 644 "$src/JetBrainsMono-OFL.txt" "$licenses/JetBrainsMono-OFL.txt"
launcher=$(awk -v executable="$exe" '/^Exec=/ { print "Exec=\"" executable "\""; next } { print }' \
    "$src/spofify-desktop.desktop")
publish 644 - "$apps/spofify-desktop.desktop" <<<"$launcher"
publish 755 "$src/spofify-desktop" "$exe"
printf 'Installed Spofify. Open it from the desktop menu.\n'
