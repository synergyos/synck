#!/bin/sh
# Install a published Synck CLI release into a user-owned directory.
set -eu

fail() { printf 'synck: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: sh install.sh [--version X.Y.Z|vX.Y.Z|latest] [--install-dir DIR]'
}

version=latest
install_dir="$HOME/.local/bin"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version|--install-dir)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"
            case "$1" in
                --version) version=$2 ;;
                --install-dir) install_dir=$2 ;;
            esac
            shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "Unknown option: $1" ;;
    esac
done

valid_version() {
    case "$1" in *[!v0-9.]*) return 1 ;; esac
    printf '%s\n' "$1" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

if [ "$version" != latest ]; then
    version="v${version#v}"
    valid_version "$version" || fail 'Version must be X.Y.Z, vX.Y.Z, or latest'
fi
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    *) fail 'Supported platforms: Linux x86_64, macOS Apple Silicon and Intel' ;;
esac
for tool in curl unzip awk; do
    command -v "$tool" >/dev/null 2>&1 || fail "Required command not found: $tool"
done
if command -v sha256sum >/dev/null 2>&1; then
    hash_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    hash_tool=shasum
else
    fail 'Required command not found: sha256sum or shasum'
fi

fetch() {
    curl --proto '=https' --proto-redir '=https' --fail --silent --show-error \
        --location --retry 3 "$@"
}
releases=https://github.com/synergyos/synck/releases
if [ "$version" = latest ]; then
    resolved=$(fetch --head --output /dev/null --write-out '%{url_effective}' "$releases/latest")
    version=${resolved##*/}
    valid_version "$version" || fail 'Latest release does not have a supported vX.Y.Z tag'
fi
asset="synck-${version#v}-$target.zip"
url="$releases/download/$version"
temporary=$(mktemp -d)
staged=
cleanup() {
    rm -rf "$temporary"
    [ -z "$staged" ] || rm -f "$staged"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
fetch --output "$temporary/SHA256SUMS.txt" "$url/SHA256SUMS.txt"
fetch --output "$temporary/$asset" "$url/$asset"
expected=$(awk -v file="$asset" '
    $2 == file {
        count++
        if (NF != 2 || length($1) != 64 || $1 ~ /[^[:xdigit:]]/) bad = 1
        hash = tolower($1)
    }
    END { if (count != 1 || bad) exit 1; print hash }
' "$temporary/SHA256SUMS.txt") || fail 'Missing, duplicate or malformed checksum'
if [ "$hash_tool" = sha256sum ]; then
    actual=$(sha256sum "$temporary/$asset")
else
    actual=$(shasum -a 256 "$temporary/$asset")
fi
[ "${actual%% *}" = "$expected" ] || fail 'Checksum verification failed; installation unchanged'

# Read only the executable entry; never extract paths supplied by the archive.
entries=$(unzip -Z1 "$temporary/$asset" | awk '$0 == "synck" { count++ } END { print count+0 }')
[ "$entries" = 1 ] || fail 'Archive must contain exactly one synck executable'
unzip -p "$temporary/$asset" synck > "$temporary/synck"
[ -s "$temporary/synck" ] || fail 'Downloaded executable is empty'
mkdir -p "$install_dir"
[ ! -d "$install_dir/synck" ] || fail 'Install destination is a directory'
staged=$(mktemp "$install_dir/.synck.XXXXXXXX")
cp "$temporary/synck" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$install_dir/synck"
staged=
printf 'Installed Synck %s to %s/synck\n' "$version" "$install_dir"
printf 'Ensure %s is on PATH, then run: synck --version\n' "$install_dir"
