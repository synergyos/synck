#!/usr/bin/env bash
set -euo pipefail
umask 077

input_dir="$1"
output_dir="$2"
version="$3"
target="$4"
: "${MACOS_DEVELOPER_ID_P12_BASE64:?missing Developer ID certificate}"
: "${MACOS_DEVELOPER_ID_P12_PASSWORD:?missing certificate password}"
: "${ASC_KEY_P8_BASE64:?missing notarization key}"
: "${ASC_KEY_ID:?missing notarization key ID}"
: "${ASC_ISSUER_ID:?missing notarization issuer}"

signing_dir="$(mktemp -d)"
keychain="${signing_dir}/release.keychain-db"
original_keychains=()
while IFS= read -r entry; do
  entry="${entry#*\"}"
  entry="${entry%\"*}"
  original_keychains+=("${entry}")
done < <(security list-keychains -d user)
cleanup() {
  local cleanup_status=0
  security list-keychains -d user -s "${original_keychains[@]}" || cleanup_status=1
  security delete-keychain "${keychain}" >/dev/null 2>&1 || true
  rm -rf -- "${signing_dir}"
  return "${cleanup_status}"
}
trap cleanup EXIT
export signing_dir
python3 - <<'PY'
import base64
import os
from pathlib import Path

directory = Path(os.environ["signing_dir"])
for variable, name in (
    ("MACOS_DEVELOPER_ID_P12_BASE64", "certificate.p12"),
    ("ASC_KEY_P8_BASE64", "notary.p8"),
):
    (directory / name).write_bytes(base64.b64decode(os.environ[variable]))
PY

keychain_password="$(openssl rand -hex 24)"
security create-keychain -p "${keychain_password}" "${keychain}"
security unlock-keychain -p "${keychain_password}" "${keychain}"
security list-keychains -d user -s "${keychain}" "${original_keychains[@]}"
intermediate="$(dirname "$0")/DeveloperIDG2CA.pem"
digest="$(openssl x509 -in "${intermediate}" -outform der | shasum -a 256 | awk '{print $1}')"
test "${digest}" = f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a
security add-certificates -k "${keychain}" "${intermediate}"
security import "${signing_dir}/certificate.p12" -P "${MACOS_DEVELOPER_ID_P12_PASSWORD}" \
  -k "${keychain}" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k "${keychain_password}" "${keychain}" >/dev/null
identity="$(security find-identity -v -p codesigning "${keychain}" | python3 -c '
import os, re, sys
requested = os.environ.get("MACOS_SIGNING_IDENTITY", "").strip()
identities = re.findall(r"([0-9A-Fa-f]{40}) \"(Developer ID Application:[^\"]+)\"", sys.stdin.read())
if requested:
    identities = [(sha, name) for sha, name in identities if requested == name or requested.lower() == sha.lower()]
if len(identities) != 1:
    raise SystemExit("Expected exactly one matching Developer ID Application identity")
print(identities[0][0])
')"
chmod 755 "${input_dir}/synck"
codesign --force --timestamp --options runtime --keychain "${keychain}" \
  --sign "${identity}" --identifier com.synckhub.synck.cli "${input_dir}/synck"
codesign --verify --strict --verbose=2 "${input_dir}/synck"

python3 scripts/cli_release.py package "${input_dir}" "${output_dir}" "${version}" "${target}"
archive="${output_dir}/synck-${version}-${target}.zip"
xcrun notarytool submit "${archive}" --key "${signing_dir}/notary.p8" \
  --key-id "${ASC_KEY_ID}" --issuer "${ASC_ISSUER_ID}" \
  --wait --timeout 30m --output-format json > "${signing_dir}/notarization.json"
python3 - "${signing_dir}/notarization.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    result = json.load(source)
if result["status"] != "Accepted":
    raise SystemExit(f"Notarization failed: {result['id']} ({result['status']})")
print(f"Notarization accepted: {result['id']}")
PY
# Apple cannot staple tickets to standalone executables or ZIP archives.
# Gatekeeper retrieves the notarization ticket online for this signed binary.
