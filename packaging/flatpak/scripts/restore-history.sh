#!/usr/bin/env bash
# Fetch the currently published application commit into a local repository, so
# the next build continues its history. With a public key file, signatures are
# required, as needed by the signing job.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 1 || $# == 2 ]] || die "usage: $0 REPOSITORY [PUBLIC_KEY_FILE]"
repo=$1
public_key=${2:-}
url=$(pages_url)

if [[ ! -f $repo/config ]]; then
    ostree init --repo="$repo" --mode=archive-z2 --collection-id="$COLLECTION_ID"
fi

# Only a definite 404 means nothing has been published. Any other failure stops
# here, so a network error cannot silently discard the history.
status=$(curl --silent --show-error --location --retry 3 --output /dev/null \
    --write-out '%{http_code}' "$url/repo/summary")
case $status in
    200) ;;
    404)
        echo "No repository is published at $url yet; starting a new history."
        exit 0
        ;;
    *) die "Could not check $url/repo/summary (HTTP $status)." ;;
esac

if [[ -n $public_key ]]; then
    verify=(--gpg-import="$public_key" --set=gpg-verify=true --set=gpg-verify-summary=true)
else
    verify=(--no-gpg-verify)
fi
ostree remote delete --repo="$repo" --if-exists published
ostree remote add --repo="$repo" "${verify[@]}" published "$url/repo/"
ostree pull --repo="$repo" --depth="$((KEEP_PREVIOUS - 1))" published "$APP_REF"
commit=$(ostree rev-parse --repo="$repo" "published:$APP_REF")
# rev-parse would also match the remote ref, so list the local refs instead.
if ostree refs --repo="$repo" | grep -qxF "$APP_REF"; then
    ostree reset --repo="$repo" "$APP_REF" "$commit"
else
    ostree refs --repo="$repo" --create="$APP_REF" "$commit"
fi
# The remote ref would otherwise keep old commits alive when pruning.
ostree refs --repo="$repo" --delete "published:$APP_REF"
ostree remote delete --repo="$repo" published
echo "Restored published commit $commit."
