#!/bin/sh
# Generates THIRD-PARTY-NOTICES.txt, which both apps ship (§4 of the owner rules).
#
#   scripts/notices.sh           regenerate the file
#   scripts/notices.sh --check   fail if the committed file is out of date (part of the gate)
#
# Part 1 comes from cargo-about (about.toml + about.hbs) and covers every Rust crate
# compiled into the core for macOS and Windows. Part 2 appends third_party/notices/*:
# licence texts retrieved by hand, each with a SOURCE.txt giving its URL and date.
# Those files are inputs; correct them there, never in the output.
#
# The licence policy (which licences may be used at all) is checked separately by
# `cargo deny check licenses` with deny.toml.
set -eu

cd "$(dirname "$0")/.."
output=THIRD-PARTY-NOTICES.txt
temp=$(mktemp)
trap 'rm -f "$temp"' EXIT

cargo about generate --workspace --fail about.hbs > "$temp"

{
    printf '\nPART 2: LICENCE TEXTS NOT CARRIED BY THE CRATES\n'
    for dir in third_party/notices/*/; do
        printf '\n================================================================================\n'
        cat "$dir/SOURCE.txt"
        for file in "$dir"*; do
            [ "$(basename "$file")" = SOURCE.txt ] && continue
            printf -- '--------------------------------------------------------------------------------\n'
            printf '%s\n\n' "$(basename "$file")"
            cat "$file"
            printf '\n'
        done
    done
} >> "$temp"

if [ "${1:-}" = "--check" ]; then
    if ! cmp -s "$temp" "$output"; then
        echo "THIRD-PARTY-NOTICES.txt is out of date: run scripts/notices.sh and commit it." >&2
        exit 1
    fi
    echo "THIRD-PARTY-NOTICES.txt is up to date."
else
    mv "$temp" "$output"
    trap - EXIT
    echo "Wrote $output"
fi
