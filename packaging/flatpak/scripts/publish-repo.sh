#!/usr/bin/env bash
# Turn an unsigned build artifact into the signed GitHub Pages site.
#
# The artifact is untrusted: only its single application commit is imported,
# after its content, identity and permissions are checked. Nothing from the
# artifact is executed.
#
# Environment:
#   FLATPAK_GPG_KEY_ID    signing key, with no passphrase, in GNUPGHOME
#   GNUPGHOME             keyring holding that key
#   FLATPAK_REPO_URL      site root (default: derived from GITHUB_REPOSITORY)
#   FOOTY_RESET_HISTORY   "true" starts over, e.g. after a signing key change
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 2 ]] || die "usage: $0 ARTIFACT_DIR SITE_DIR"
artifact=$1
site=${2:?}
: "${FLATPAK_GPG_KEY_ID:?}" "${GNUPGHOME:?}"
url=$(pages_url)
homepage="https://github.com/${GITHUB_REPOSITORY:-Kybernetria/footy}"
repo="$site/repo"
[[ ! -e $site ]] || die "The site directory must not exist yet: $site"

archive="$artifact/footy-ostree.tar.gz"
for file in "$archive" "$artifact/footy-ref.txt" "$artifact/footy-commit.txt"; do
    [[ -f $file && ! -L $file && -s $file ]] || die "Missing artifact file: $file"
done
[[ $(<"$artifact/footy-ref.txt") == "$APP_REF" ]] || die 'Unexpected application ref.'
commit=$(<"$artifact/footy-commit.txt")
[[ $commit =~ ^[a-f0-9]{64}$ ]] || die 'Malformed commit checksum.'

work=$(mktemp -d "${RUNNER_TEMP:-$(dirname -- "$site")}/footy-publish.XXXXXXXX")
trap 'rm -rf -- "${work:?}"' EXIT

# An archive repository holds only files and directories.
tar -tvzf "$archive" >"$work/listing"
if grep -qv '^[-d]' "$work/listing"; then
    die 'The artifact contains links or special files.'
fi
mkdir "$work/untrusted"
tar -xzf "$archive" -C "$work/untrusted" --no-same-owner --no-same-permissions
source_repo="$work/untrusted/repo"
[[ -d $source_repo/objects ]] || die 'The artifact holds no repository.'
# Replace the artifact's configuration, which could otherwise redirect reads.
rm -f -- "$work/untrusted/repo/config"
printf '[core]\nrepo_version=1\nmode=archive-z2\n' >"$source_repo/config"

ostree fsck --repo="$source_repo" --all
[[ $(ostree rev-parse --repo="$source_repo" "$APP_REF") == "$commit" ]] ||
    die 'The artifact ref does not point at the validated commit.'

gpg --batch --export "$FLATPAK_GPG_KEY_ID" >"$work/key.gpg"
[[ -s $work/key.gpg ]] || die 'The signing key has no public key to export.'

mkdir -p "$site"
ostree init --repo="$repo" --mode=archive-z2 --collection-id="$COLLECTION_ID"
if [[ ${FOOTY_RESET_HISTORY:-false} != true ]]; then
    bash "$SCRIPTS_DIR/restore-history.sh" "$repo" "$work/key.gpg"
fi
previous=$(ostree rev-parse --repo="$repo" "$APP_REF" 2>/dev/null || true)

# Pull only this commit, without any history the artifact might carry.
ostree pull-local --repo="$repo" --untrusted "$source_repo" "$APP_REF"
[[ $(ostree rev-parse --repo="$repo" "$APP_REF") == "$commit" ]]
ostree fsck --repo="$repo"

# Other commit metadata can change how clients treat the app, such as extra
# downloads or moving users to another ref.
unexpected=$(ostree show --repo="$repo" --list-metadata-keys "$commit" |
    grep -vxE 'ostree\.(collection|ref|collection-refs)-binding|xa\.(download-size|installed-size|metadata|ref|from_commit)' || true)
[[ -z $unexpected ]] || die "Unexpected commit metadata: ${unexpected//$'\n'/, }"

# The permissions users are shown and the ones they get must both be the
# reviewed set.
ostree cat --repo="$repo" "$commit" /metadata >"$work/metadata"
python3 -I "$SCRIPTS_DIR/check.py" metadata "$work/metadata"
ostree show --repo="$repo" --print-metadata-key=xa.metadata "$commit" |
    python3 -I -c '
import ast, pathlib, sys
shown = ast.literal_eval(sys.stdin.read().strip())
installed = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
sys.exit(0 if shown == installed else "commit metadata differs from /metadata")
' "$work/metadata"
expect_binding() {
    local value
    value=$(ostree show --repo="$repo" --print-metadata-key="$1" "$commit")
    [[ $value == "$2" ]] || die "Unexpected $1: $value"
}
expect_binding ostree.ref-binding "['$APP_REF']"
expect_binding ostree.collection-binding "'$COLLECTION_ID'"

commit_time() {
    date -u +%s -d "$(ostree show --repo="$repo" "$1" | sed -n 's/^Date: *//p')"
}
if [[ -n $previous && $previous != "$commit" ]]; then
    parent=$(ostree rev-parse --repo="$repo" "$commit^" 2>/dev/null || true)
    [[ $parent == "$previous" ]] ||
        die "The build does not continue the published commit $previous. Run the whole workflow again."
    # Flatpak refuses to update to an older commit.
    (($(commit_time "$commit") >= $(commit_time "$previous"))) ||
        die 'The build is older than the published commit.'
fi

flatpak build-sign --arch="$ARCH" \
    --gpg-sign="$FLATPAK_GPG_KEY_ID" --gpg-homedir="$GNUPGHOME" \
    "$repo" "$APP_ID" "$BRANCH"

flatpak build-update-repo \
    --gpg-sign="$FLATPAK_GPG_KEY_ID" --gpg-homedir="$GNUPGHOME" \
    --collection-id="$COLLECTION_ID" \
    --deploy-sideload-collection-id \
    --default-branch="$BRANCH" \
    --title='Footy' \
    --comment='Local speech-to-text dictation' \
    --description='Flatpak repository for Footy' \
    --homepage="$homepage" \
    --generate-static-deltas \
    --prune --prune-depth="$KEEP_PREVIOUS" \
    "$repo"
ostree fsck --repo="$repo"

# Verify as a client does, with signatures required.
ostree init --repo="$work/client" --mode=archive-z2
ostree remote add --repo="$work/client" --gpg-import="$work/key.gpg" \
    --set=gpg-verify=true --set=gpg-verify-summary=true \
    signed "file://$(realpath -- "$repo")"
ostree pull --repo="$work/client" signed \
    "$APP_REF" "appstream/$ARCH" "appstream2/$ARCH"
[[ $(ostree rev-parse --repo="$work/client" "signed:$APP_REF") == "$commit" ]]

gpg_key=$(base64 --wrap=0 <"$work/key.gpg")
for template in footy.flatpakrepo footy.flatpakref index.html; do
    sed -e "s|@REPO_URL@|$url|g" \
        -e "s|@HOMEPAGE@|$homepage|g" \
        -e "s|@GPG_KEY@|$gpg_key|g" \
        "$PACKAGING_DIR/pages/$template.in" >"$site/$template"
    if grep -Eq '@[A-Z_]+@' "$site/$template"; then
        die "Unexpanded placeholder in $template"
    fi
done
cp -- "$SOURCE_DIR/src-tauri/icons/128x128.png" "$site/icon.png"

# Leave only what clients download: no working directories or lock files.
for leftover in .lock state extensions; do
    rm -rf -- "${site:?}/repo/$leftover"
done
find "${site:?}/repo/tmp" -mindepth 1 -delete
find "${site:?}/repo/refs/remotes" -mindepth 1 -type d -empty -delete

[[ -s $repo/summary && -s $repo/summary.sig ]] || die 'The repository summary is not signed.'
if find "$site" -type l -print -quit | grep -q .; then
    die 'The site must not contain symlinks.'
fi
size=$(du -sb "$site" | cut -f1)
((size <= MAX_SITE_BYTES)) || die "The site is $size bytes, above $MAX_SITE_BYTES."
echo "Signed $APP_REF at $commit; site is $size bytes."
