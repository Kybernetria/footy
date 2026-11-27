#!/usr/bin/env bash
# Publish the built application with a temporary signing key, serve the site on
# loopback, and check what a user's installation does: install, update in place,
# roll back, and refuse unsigned replacements. Also checks that publication
# rejects stale builds and changed permissions.
#
# Uses throwaway Flatpak, GnuPG and home directories. The GNOME runtime must be
# available in the system installation (FLATPAK_SYSTEM_DIR may point elsewhere).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 1 ]] || die "usage: $0 WORK_DIR"
work=$(cd -- "$1" && pwd)
scratch=$(mktemp -d "$work/signed-test.XXXXXXXX")
server=
cleanup() {
    [[ -z $server ]] || kill "$server" 2>/dev/null || true
    GNUPGHOME="$scratch/gnupg" gpgconf --kill all 2>/dev/null || true
    rm -rf -- "${scratch:?}"
}
trap cleanup EXIT

export HOME="$scratch/home" FLATPAK_USER_DIR="$scratch/flatpak"
export XDG_DATA_HOME="$HOME/.local/share" XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config" GNUPGHOME="$scratch/gnupg"
unset RUNNER_TEMP FOOTY_RESET_HISTORY
mkdir -p "$HOME" "$scratch/live"
mkdir -m 700 "$GNUPGHOME"

step() { printf '\n== %s\n' "$*"; }

step 'Validate the untrusted archive with the signing job extractor'
python3 -I - "$scratch" <<'PY'
import io
import pathlib
import sys
import tarfile

scratch = pathlib.Path(sys.argv[1])
entries = {
    "absolute": [tarfile.TarInfo(str(scratch / "outside"))],
    "parent": [tarfile.TarInfo("repo/../../outside")],
    "duplicate": [tarfile.TarInfo("repo/object"), tarfile.TarInfo("repo/object")],
    "pax": [tarfile.TarInfo("repo/looks-safe")],
    "symlink": [tarfile.TarInfo("repo/link")],
    "hardlink": [tarfile.TarInfo("repo/link")],
    "valid": [tarfile.TarInfo("repo/config"), tarfile.TarInfo("repo/object")],
}
entries["pax"][0].pax_headers["path"] = "repo/../../outside"
for name, kind in [("symlink", tarfile.SYMTYPE), ("hardlink", tarfile.LNKTYPE)]:
    entries[name][0].type = kind
    entries[name][0].linkname = "../outside"
for name, members in entries.items():
    with tarfile.open(scratch / f"{name}.tar.gz", "w:gz") as archive:
        for member in members:
            data = b"[core]\nmode=bare\n" if member.name == "repo/config" else b""
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
PY
mkdir "$scratch/extract"
for variant in absolute parent duplicate pax symlink hardlink; do
    if python3 -I "$SCRIPTS_DIR/check.py" unpack "$scratch/$variant.tar.gz" \
        "$scratch/extract" "$MAX_SITE_BYTES" 2>"$scratch/$variant.log"; then
        die "The extractor accepted the $variant archive."
    fi
    grep -Eq '^check failed: (invalid archive member path|duplicate archive member|unsupported archive member type)' \
        "$scratch/$variant.log"
done
[[ ! -e $scratch/outside && ! -e $scratch/extract/repo ]]
python3 -I "$SCRIPTS_DIR/check.py" unpack "$scratch/valid.tar.gz" \
    "$scratch/extract" "$MAX_SITE_BYTES"
[[ -f $scratch/extract/repo/object && ! -e $scratch/extract/repo/config ]]

gpg --batch --quiet --passphrase '' \
    --quick-generate-key 'Footy repository test <test@invalid>' rsa3072 sign 1d
FLATPAK_GPG_KEY_ID=$(gpg --batch --with-colons --list-secret-keys |
    awk -F: '$1 == "fpr" { print $10; exit }')
export FLATPAK_GPG_KEY_ID

python3 -I -u -m http.server 0 --bind 127.0.0.1 --directory "$scratch/live" \
    >"$scratch/http.log" 2>&1 &
server=$!
port=
for ((poll = 0; poll < 100; poll++)); do
    port=$(sed -n 's/^Serving HTTP on 127\.0\.0\.1 port \([0-9]*\).*/\1/p' "$scratch/http.log")
    [[ -z $port ]] || break
    sleep 0.1
done
[[ -n $port ]] || die 'The test web server did not start.'
export FLATPAK_REPO_URL="http://127.0.0.1:$port"

publish() { # ARTIFACT SITE
    bash "$SCRIPTS_DIR/publish-repo.sh" "$1" "$2"
}
deploy() { # SITE
    rm -rf -- "${scratch:?}/live"
    cp -a -- "$1" "$scratch/live"
}
installed_commit() {
    flatpak info --user --show-commit "$APP_ID"
}
update() {
    flatpak update --user --noninteractive "$APP_ID"
}

# Prepare a further build on top of the published commit, as the build job does,
# without recompiling: same content, new commit.
next_build() { # NAME
    local dir="$scratch/$1"
    mkdir -p "$dir"
    bash "$SCRIPTS_DIR/restore-history.sh" "$dir/repo"
    flatpak build-commit-from --force --subject="Test $1" \
        --src-repo="$work/repo" --src-ref="$APP_REF" "$dir/repo" "$APP_REF"
    printf '%s\n' "$APP_REF" >"$dir/footy-ref.txt"
    ostree rev-parse --repo="$dir/repo" "$APP_REF" >"$dir/footy-commit.txt"
    bash "$SCRIPTS_DIR/pack-artifact.sh" "$dir" "$scratch/$1-artifact"
}

step 'First publication'
# The built commit may continue an older publication; publish a root commit.
mkdir -p "$scratch/build1"
ostree init --repo="$scratch/build1/repo" --mode=archive-z2 \
    --collection-id="$COLLECTION_ID"
flatpak build-commit-from --force --subject='Test build 1' \
    --src-repo="$work/repo" --src-ref="$APP_REF" "$scratch/build1/repo" "$APP_REF"
printf '%s\n' "$APP_REF" >"$scratch/build1/footy-ref.txt"
ostree rev-parse --repo="$scratch/build1/repo" "$APP_REF" >"$scratch/build1/footy-commit.txt"
bash "$SCRIPTS_DIR/pack-artifact.sh" "$scratch/build1" "$scratch/build1-artifact"
first=$(<"$scratch/build1/footy-commit.txt")
publish "$scratch/build1-artifact" "$scratch/site1"
deploy "$scratch/site1"

step 'Fresh install from the published .flatpakref'
flatpak install --user --noninteractive --from "$FLATPAK_REPO_URL/footy.flatpakref"
[[ $(installed_commit) == "$first" ]]
[[ $(flatpak info --user --show-ref "$APP_ID") == "$APP_REF" ]]
origin=$(flatpak info --user --show-origin "$APP_ID")
options=$(flatpak remotes --user --columns=name,options |
    awk -v name="$origin" '$1 == name { print $2 }')
[[ $options != *no-gpg-verify* ]] || die 'The installed remote does not verify signatures.'
flatpak remote-ls --user --app "$origin" --columns=application | grep -qx "$APP_ID"
flatpak run --user "$APP_ID" --help >/dev/null

step 'In-place update with the same key'
next_build build2
second=$(<"$scratch/build2/footy-commit.txt")
[[ $second != "$first" ]]
publish "$scratch/build2-artifact" "$scratch/site2"
deploy "$scratch/site2"
update
[[ $(installed_commit) == "$second" ]]
flatpak run --user "$APP_ID" --help >/dev/null

step "History is limited to the newest $((KEEP_PREVIOUS + 1)) commits"
next_build build3
publish "$scratch/build3-artifact" "$scratch/site3"
deploy "$scratch/site3"
third=$(<"$scratch/build3/footy-commit.txt")
next_build build4
publish "$scratch/build4-artifact" "$scratch/site4"
deploy "$scratch/site4"
fourth=$(<"$scratch/build4/footy-commit.txt")
for kept in "$fourth" "$third" "$second"; do
    ostree show --repo="$scratch/site4/repo" "$kept" >/dev/null
done
if ostree show --repo="$scratch/site4/repo" "$first" >/dev/null 2>&1; then
    die 'The oldest commit was not pruned.'
fi
update
[[ $(installed_commit) == "$fourth" ]]
flatpak update --user --noninteractive --commit="$third" "$APP_ID"
[[ $(installed_commit) == "$third" ]]
update
[[ $(installed_commit) == "$fourth" ]]

step 'Publication rejects a build that skips the published commit'
if publish "$scratch/build2-artifact" "$scratch/site-stale" 2>"$scratch/stale.log"; then
    die 'A stale build was published.'
fi
cat "$scratch/stale.log"
grep -q 'does not continue the published commit' "$scratch/stale.log"

step 'Publication rejects changed permissions'
mkdir -p "$scratch/widened"
bash "$SCRIPTS_DIR/restore-history.sh" "$scratch/widened/repo"
ostree checkout --repo="$work/repo" --user-mode "$APP_REF" "$scratch/widened/tree"
# Replace the file: the checkout hard-links objects of the build repository.
sed 's/^devices=dri;$/devices=all;/' "$scratch/widened/tree/metadata" \
    >"$scratch/widened/metadata"
grep -q '^devices=all;$' "$scratch/widened/metadata"
rm -- "$scratch/widened/tree/metadata"
cp -- "$scratch/widened/metadata" "$scratch/widened/tree/metadata"
ostree commit --repo="$scratch/widened/repo" --branch="$APP_REF" \
    --owner-uid=0 --owner-gid=0 --no-xattrs --canonical-permissions \
    --add-metadata-string="xa.metadata=$(<"$scratch/widened/metadata")
" \
    --tree=dir="$scratch/widened/tree" >"$scratch/widened/footy-commit.txt"
printf '%s\n' "$APP_REF" >"$scratch/widened/footy-ref.txt"
bash "$SCRIPTS_DIR/pack-artifact.sh" "$scratch/widened" "$scratch/widened-artifact"
if publish "$scratch/widened-artifact" "$scratch/site-widened" 2>"$scratch/widened.log"; then
    die 'A build with extra permissions was published.'
fi
cat "$scratch/widened.log"
grep -q 'sandbox permissions differ' "$scratch/widened.log"

step 'Publication rejects unexpected commit metadata'
next_build redirect
flatpak build-commit-from --force --end-of-life-rebase="$APP_ID=computer.footy.other" \
    --src-repo="$work/repo" --src-ref="$APP_REF" "$scratch/redirect/repo" "$APP_REF"
ostree rev-parse --repo="$scratch/redirect/repo" "$APP_REF" >"$scratch/redirect/footy-commit.txt"
bash "$SCRIPTS_DIR/pack-artifact.sh" "$scratch/redirect" "$scratch/redirect-artifact2"
if publish "$scratch/redirect-artifact2" "$scratch/site-redirect" 2>"$scratch/redirect.log"; then
    die 'A build redirecting users was published.'
fi
cat "$scratch/redirect.log"
grep -q 'Unexpected commit metadata' "$scratch/redirect.log"

# A replacement repository: one more commit that nobody signed.
replacement() { # NAME
    cp -a -- "$scratch/site4" "$scratch/$1"
    flatpak build-commit-from --force --subject="Unsigned $1" \
        --src-repo="$work/repo" --src-ref="$APP_REF" "$scratch/$1/repo" "$APP_REF"
}
expect_signature_error() { # NAME COMMAND...
    local log="$scratch/$1.log"
    shift
    if "$@" >"$log" 2>&1; then
        cat "$log"
        die "Flatpak accepted a repository without signatures: $*"
    fi
    cat "$log"
    grep -Eq 'GPG verification enabled, but no( summary)? signatures found' "$log" ||
        die 'Flatpak failed for a reason other than missing signatures.'
}

step 'Signed summary with an unsigned application commit is refused'
replacement unsigned-commit
flatpak build-update-repo --no-update-appstream \
    --gpg-sign="$FLATPAK_GPG_KEY_ID" --gpg-homedir="$GNUPGHOME" \
    "$scratch/unsigned-commit/repo"
deploy "$scratch/unsigned-commit"
expect_signature_error unsigned-commit update
[[ $(installed_commit) == "$fourth" ]]

step 'Unsigned repository is refused'
replacement unsigned-repo
flatpak build-update-repo --no-update-appstream "$scratch/unsigned-repo/repo"
rm -f -- "${scratch:?}/unsigned-repo/repo/summary.sig" \
    "${scratch:?}/unsigned-repo/repo/summary.idx.sig"
deploy "$scratch/unsigned-repo"
expect_signature_error unsigned-repo flatpak remote-ls --user "$origin"
# An update of an installed app only warns here, and must change nothing.
update || true
[[ $(installed_commit) == "$fourth" ]]

step 'The signed repository still works afterwards'
deploy "$scratch/site4"
update
[[ $(installed_commit) == "$fourth" ]]

echo 'Signed repository tests passed.'
