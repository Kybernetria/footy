#!/usr/bin/env bash
# Build the unsigned repository, a test bundle, and the files that the signing
# job uses to identify the build. Needs no secrets.
#
# Environment:
#   FLATPAK_BUILDER       flatpak-builder command (default: flatpak-builder)
#   FLATPAK_BUILDER_ARGS  extra arguments, e.g. --install-deps-from=flathub
#   FOOTY_MANIFEST        alternative manifest, e.g. one with local caches
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 1 ]] || die "usage: $0 WORK_DIR"
mkdir -p "$1"
work=$(cd -- "$1" && pwd)
case $work/ in
    "$SOURCE_DIR"/*) die 'The work directory must be outside the source tree.' ;;
esac
repo="$work/repo"
read -ra builder <<<"${FLATPAK_BUILDER:-flatpak-builder}"
read -ra builder_args <<<"${FLATPAK_BUILDER_ARGS:-}"

"${builder[@]}" \
    --force-clean \
    --arch="$ARCH" \
    --default-branch="$BRANCH" \
    --state-dir="$work/state" \
    --repo="$repo" \
    --collection-id="$COLLECTION_ID" \
    "${builder_args[@]}" \
    "$work/builddir" \
    "${FOOTY_MANIFEST:-$MANIFEST}"

commit=$(ostree rev-parse --repo="$repo" "$APP_REF")
printf '%s\n' "$APP_REF" >"$work/footy-ref.txt"
printf '%s\n' "$commit" >"$work/footy-commit.txt"
flatpak build-bundle --arch="$ARCH" "$repo" "$work/footy.flatpak" "$APP_ID" "$BRANCH"
echo "Built $APP_REF at $commit"
