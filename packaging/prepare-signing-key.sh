#!/bin/sh
# Write the minisign secret key from $MINISIGN_SECRET_KEY to $1, tolerating
# what pasting into a web form does (CRLF, blank lines, stray spaces, the
# comment line left out), and explain anything else without echoing it.
set -eu
out=$1
umask 077
printf '%s\n' "$MINISIGN_SECRET_KEY" | tr -d '\r' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//; /^$/d' > "$out.raw"
case $(head -n1 "$out.raw") in
    "untrusted comment:"*) cp "$out.raw" "$out" ;;
    *) { echo "untrusted comment: minisign encrypted secret key"; cat "$out.raw"; } > "$out" ;;
esac
rm -f "$out.raw"
lines=$(wc -l < "$out")
bytes=$(sed -n 2p "$out" | base64 -d 2>/dev/null | wc -c || true)
if [ "$lines" -ne 2 ] || [ "$bytes" -ne 158 ]; then
    if [ "$bytes" -eq 42 ]; then
        why="it holds a public key (fex-release.pub); paste the whole of fex-release.key instead"
    else
        why="expected the two lines of fex-release.key (found $lines line(s), $bytes bytes of key data; a secret key has 158)"
    fi
    echo "::error::MINISIGN_SECRET_KEY isn't a minisign secret key: $why"
    rm -f "$out"
    exit 1
fi
