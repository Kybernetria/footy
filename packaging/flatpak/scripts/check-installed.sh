#!/usr/bin/env bash
# Install the built bundle into a throwaway Flatpak installation and check what
# a user would get. The GNOME runtime must be available in the system
# installation (FLATPAK_SYSTEM_DIR may point somewhere else).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 1 ]] || die "usage: $0 WORK_DIR"
work=$(cd -- "$1" && pwd)
scratch=$(mktemp -d "$work/install-check.XXXXXXXX")
trap 'rm -rf -- "${scratch:?}"' EXIT
export HOME="$scratch/home" FLATPAK_USER_DIR="$scratch/flatpak"
export XDG_DATA_HOME="$HOME/.local/share" XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
mkdir -p "$HOME"

flatpak install --user --noninteractive --bundle "$work/footy.flatpak"
[[ $(flatpak info --user --show-ref "$APP_ID") == "$APP_REF" ]]
[[ $(flatpak info --user --show-commit "$APP_ID") == "$(<"$work/footy-commit.txt")" ]]

flatpak info --user --show-metadata "$APP_ID" >"$scratch/metadata"
python3 -I "$SCRIPTS_DIR/check.py" metadata "$scratch/metadata"

location=$(flatpak info --user --show-location "$APP_ID")
files="$location/files"
[[ -x $files/bin/footy ]] || die 'bin/footy is missing'
diff -r "$SOURCE_DIR/src-tauri/resources" "$files/lib/Footy/resources" ||
    die 'Installed resources differ from src-tauri/resources.'
for path in \
    lib/libayatana-appindicator3.so.1 \
    lib/libgtk-layer-shell.so.0 \
    "share/metainfo/$APP_ID.metainfo.xml" \
    "share/licenses/$APP_ID/LICENSE" \
    lib/Footy/resources/models/silero_vad_v4.onnx; do
    [[ -s $files/$path ]] || die "Missing installed file: $path"
done
for path in \
    "share/applications/$APP_ID.desktop" \
    "share/icons/hicolor/128x128/apps/$APP_ID.png"; do
    [[ -s $location/export/$path ]] || die "Missing exported file: $path"
done

# Every library must resolve inside the runtime. The tray library is loaded with
# dlopen, so check it separately from the binary's own dependencies.
flatpak run --user --command=sh "$APP_ID" -euc '
    test -f /.flatpak-info
    for object in /app/bin/footy /app/lib/libayatana-appindicator3.so.1 \
        /app/lib/libgtk-layer-shell.so.0; do
        libraries=$(ldd "$object")
        if printf "%s\n" "$libraries" | grep -F "not found"; then
            echo "Unresolved libraries in $object" >&2
            exit 1
        fi
    done'
flatpak run --user "$APP_ID" --help >"$scratch/help.txt"
for option in --toggle-transcription --toggle-post-process --cancel; do
    grep -qF -- "$option" "$scratch/help.txt" || die "--help does not list $option"
done
echo 'Installed bundle checks passed.'
