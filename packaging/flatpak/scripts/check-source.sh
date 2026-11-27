#!/usr/bin/env bash
# Validate the packaging files before building.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

for script in "$SCRIPTS_DIR"/*.sh; do
    bash -n "$script"
done
desktop-file-validate "$PACKAGING_DIR/$APP_ID.desktop"
appstreamcli validate --no-net "$PACKAGING_DIR/$APP_ID.metainfo.xml"
python3 -I "$SCRIPTS_DIR/check.py" version "$SOURCE_DIR"
read -ra builder <<<"${FLATPAK_BUILDER:-flatpak-builder}"
"${builder[@]}" --show-manifest "$MANIFEST" |
    python3 -I "$SCRIPTS_DIR/check.py" manifest
echo 'Packaging sources are valid.'
