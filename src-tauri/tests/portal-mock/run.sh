#!/bin/sh
# Run the ignored XDG portal protocol tests (portal::*::mock_portal) against
# mock_portal.py on a private session bus. Needs dbus-run-session, gdbus and
# python3 with PyGObject (all in org.gnome.Sdk). Extra arguments go to cargo.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
exec dbus-run-session -- sh -eu -c '
    here=$1; shift
    log=$(mktemp)
    python3 -I "$here/mock_portal.py" >"$log" 2>&1 &
    mock=$!
    trap "kill $mock 2>/dev/null; rm -f \"$log\"" EXIT
    gdbus wait --session --timeout 10 org.footy.Mock
    cd "$here/../.."
    cargo test --lib "$@" mock_portal -- --ignored --test-threads=1
' sh "$here" "$@"
