#!/bin/sh
# Sign every file in DIR for release VERSION with a minisign secret key:
#   sign-release.sh DIR VERSION KEYFILE   (the key's password on stdin)
# Each signature's trusted comment, covered by the signature, is
# "fex VERSION FILENAME": the updater checks both, so no file can stand in
# for another or for another version.
set -eu
dir=$1 version=$2 key=$3
password=$(cat)
for f in "$dir"/*; do
    case $f in *.minisig) continue ;; esac
    name=$(basename "$f")
    printf '%s\n' "$password" | minisign -S -s "$key" -m "$f" -x "$f.minisig" -t "fex $version $name" >/dev/null
    echo "signed $name"
done
