#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
allowlist="$repo_root/scripts/file_folder_event_proposal_allowlist.tsv"
audit_tmp="$(mktemp -d)"
trap 'rm -rf "$audit_tmp"' EXIT

cd "$repo_root"
export LC_ALL=C

if rg -n --glob '*.rs' '(EventProposal\s+as\s+|type\s+[A-Za-z0-9_]+\s*=\s*EventProposal)' crates; then
  echo "EventProposal aliases bypass the direct-constructor inventory." >&2
  exit 1
fi

awk -F '\t' '$1 == "SITE" { print $3 "\t" $4 }' "$allowlist" \
  > "$audit_tmp/expected-sites"
rg -l --glob '*.rs' 'EventProposal\s*\{' crates | sort | while IFS= read -r path; do
  count="$(rg -c 'EventProposal\s*\{' "$path")"
  printf '%s\t%s\n' "$count" "$path"
done > "$audit_tmp/actual-sites"

if ! diff -u "$audit_tmp/expected-sites" "$audit_tmp/actual-sites"; then
  echo "FILE/FOLDER EventProposal constructor inventory changed; classify the exact site." >&2
  exit 1
fi

awk -F '\t' '$1 == "VERB" { print $3 "\t" $4 }' "$allowlist" \
  > "$audit_tmp/expected-verbs"
rg --no-filename -o --glob '*.rs' \
  'event_type:\s*"(FILE|FOLDER)::[A-Z0-9_:]+"' crates \
  | sed -E 's/.*"((FILE|FOLDER)::[A-Z0-9_:]+)"/\1/' \
  | sort \
  | uniq -c \
  | awk '{ print $1 "\t" $2 }' \
  > "$audit_tmp/actual-verbs"

if ! diff -u "$audit_tmp/expected-verbs" "$audit_tmp/actual-verbs"; then
  echo "FILE/FOLDER event vocabulary changed; amend the profile contract first." >&2
  exit 1
fi

echo "FILE/FOLDER write boundary audit passed"
