# Shared names and helpers for the Flatpak scripts. Source this file.
# shellcheck shell=bash

APP_ID=computer.footy.app
ARCH=x86_64
BRANCH=master
APP_REF="app/$APP_ID/$ARCH/$BRANCH"
COLLECTION_ID=computer.footy.app.Repository
RUNTIME_REPO=https://flathub.org/repo/flathub.flatpakrepo
# Older application commits kept in the published repository for rollback.
KEEP_PREVIOUS=2
# GitHub Pages rejects sites above 1 GB.
MAX_SITE_BYTES=900000000

PACKAGING_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SOURCE_DIR=$(cd -- "$PACKAGING_DIR/../.." && pwd)
SCRIPTS_DIR="$PACKAGING_DIR/scripts"
MANIFEST="$PACKAGING_DIR/$APP_ID.yml"

die() {
    echo "$*" >&2
    exit 1
}

# Site root of the published repository, without a trailing slash. Plain HTTP is
# accepted only for the loopback server used by test-signed-repo.sh.
pages_url() {
    local url=${FLATPAK_REPO_URL:-} owner name
    if [[ -z $url ]]; then
        [[ ${GITHUB_REPOSITORY:-} == */* ]] || die 'Set FLATPAK_REPO_URL or GITHUB_REPOSITORY.'
        owner=${GITHUB_REPOSITORY%%/*}
        name=${GITHUB_REPOSITORY#*/}
        owner=${owner,,}
        if [[ ${name,,} == "$owner.github.io" ]]; then
            url="https://$owner.github.io"
        else
            url="https://$owner.github.io/$name"
        fi
    fi
    url=${url%/}
    [[ $url =~ ^(https://[A-Za-z0-9._~:/-]+|http://127\.0\.0\.1:[0-9]+(/[A-Za-z0-9._~/-]*)?)$ ]] ||
        die "The repository URL must be a plain HTTPS site root: $url"
    printf '%s\n' "$url"
}
