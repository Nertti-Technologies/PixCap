#!/bin/sh
# Generates the licence files PixCap ships (§4 of the owner rules):
#
#   THIRD-PARTY-NOTICES.txt                   both apps
#   platforms/windows/installer/License.txt   the Windows installer's licence page
#
#   scripts/notices.sh           regenerate both
#   scripts/notices.sh --check   fail if either committed file is out of date
#                                (run by scripts/gate.sh and by make-app.sh for release builds)
#
# Part 1 comes from cargo-about (about.toml + about.hbs) and covers every Rust crate
# compiled into the core for macOS and Windows. Part 2 appends third_party/notices/*:
# licence texts retrieved by hand. Part 3 appends third_party/windows/*: the Microsoft
# components installed with the Windows app. Each input folder has a SOURCE.txt with
# its URL and date; correct the inputs, never the output.
#
# The Microsoft terms require end users to agree to terms protecting those components,
# so the installer shows License.txt (PixCap's licence plus Part 3) on a page the user
# must accept (LicenseFile in PixCap.iss).
#
# The licence policy (which licences may be used at all) is checked separately by
# `cargo deny check licenses` with deny.toml.
set -eu
# Fixed collation, so input files are listed in the same order on every machine.
export LC_ALL=C

cd "$(dirname "$0")/.."
notices=THIRD-PARTY-NOTICES.txt
license_page=platforms/windows/installer/License.txt
temp_notices=$(mktemp)
temp_page=$(mktemp)
trap 'rm -f "$temp_notices" "$temp_page"' EXIT

rule='================================================================================'
thin='--------------------------------------------------------------------------------'

# Prints every input folder under $1: its SOURCE.txt, then each other file.
append_inputs() {
    for dir in "$1"/*/; do
        printf '\n%s\n' "$rule"
        cat "$dir/SOURCE.txt"
        for file in "$dir"*; do
            [ "$(basename "$file")" = SOURCE.txt ] && continue
            printf '%s\n%s\n\n' "$thin" "$(basename "$file")"
            # Microsoft's files use CRLF line endings; normalise so output is stable.
            tr -d '\r' < "$file"
            printf '\n'
        done
    done
}

# $1: optional prefix for the heading, such as "PART 3: ".
microsoft_heading() {
    printf '\n%sMICROSOFT COMPONENTS (WINDOWS ONLY)\n\n' "${1:-}"
    printf 'PixCap for Windows installs the Microsoft components below next to the app.\n'
    printf 'They are not covered by the Apache License 2.0. By installing PixCap you also\n'
    printf 'agree to the Microsoft licence terms that apply to them, reproduced here.\n'
    printf 'PixCap for macOS contains none of them.\n'
}

cargo about generate --workspace --fail about.hbs > "$temp_notices"
{
    printf '\nPART 2: LICENCE TEXTS NOT CARRIED BY THE CRATES\n'
    append_inputs third_party/notices
    microsoft_heading 'PART 3: '
    append_inputs third_party/windows
} >> "$temp_notices"

{
    # UTF-8 byte-order mark: Inno Setup reads a licence file without one as ANSI,
    # which would garble the curly quotes in Microsoft's terms.
    printf '\357\273\277'
    printf 'PixCap is licensed under the Apache License 2.0, reproduced first below.\n'
    printf 'Notices for the open-source components it includes are in\n'
    printf 'THIRD-PARTY-NOTICES.txt in the installation folder.\n\n%s\n\n' "$rule"
    tr -d '\r' < LICENSE
    printf '\n%s\n' "$rule"
    microsoft_heading
    append_inputs third_party/windows
} > "$temp_page"

if [ "${1:-}" = "--check" ]; then
    stale=0
    cmp -s "$temp_notices" "$notices" || { echo "$notices is out of date." >&2; stale=1; }
    cmp -s "$temp_page" "$license_page" || { echo "$license_page is out of date." >&2; stale=1; }
    if [ "$stale" -ne 0 ]; then
        echo "Run scripts/notices.sh and commit the result." >&2
        exit 1
    fi
    echo "Licence files are up to date."
else
    mv "$temp_notices" "$notices"
    mv "$temp_page" "$license_page"
    trap - EXIT
    chmod 644 "$notices" "$license_page"
    echo "Wrote $notices and $license_page"
fi
