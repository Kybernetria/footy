#!/usr/bin/env bash
# Pack the unsigned build for the signing job: the application ref only.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

[[ $# == 2 ]] || die "usage: $0 WORK_DIR ARTIFACT_DIR"
work=$1
artifact=$2
mkdir -p "$artifact"
commit=$(<"$work/footy-commit.txt")

# A fresh repository leaves out debug symbols and older history.
staging=$(mktemp -d "$work/pack.XXXXXXXX")
trap 'rm -rf -- "${staging:?}"' EXIT
ostree init --repo="$staging/repo" --mode=archive-z2 --collection-id="$COLLECTION_ID"
ostree pull-local --repo="$staging/repo" "$work/repo" "$APP_REF"
[[ $(ostree rev-parse --repo="$staging/repo" "$APP_REF") == "$commit" ]]

tar --sort=name --owner=0 --group=0 --numeric-owner \
    -czf "$artifact/footy-ostree.tar.gz" -C "$staging" repo
cp -- "$work/footy-ref.txt" "$work/footy-commit.txt" "$artifact/"
