#!/usr/bin/env bash
# checksum_verify.test.sh — pins the integrity-check idiom provision.sh uses for the GOWA binary:
#   printf '%s  %s\n' "$EXPECTED_SHA256" "$file" | sha256sum -c -
# Proves a known-good checksum passes and a wrong one fails (so a bad/compromised/tag-drifted
# artifact aborts provisioning instead of installing). No network, no real GOWA binary.
set -euo pipefail

# Linux boxes (and CI) have sha256sum; fall back to `shasum -a 256` on macOS for local runs.
if command -v sha256sum >/dev/null 2>&1; then
  SUM() { sha256sum "$@"; }
  SUM_CHECK() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  SUM() { shasum -a 256 "$@"; }
  SUM_CHECK() { shasum -a 256 -c -; }
else
  echo "SKIP: no sha256sum/shasum available" >&2
  exit 0
fi

fail() { echo "FAIL: $*" >&2; exit 1; }

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
printf 'pretend this is the gowa binary\n' > "$tmp"

good="$(SUM "$tmp" | awk '{print $1}')"
bad="0000000000000000000000000000000000000000000000000000000000000000"

# 1. The genuine checksum must verify.
if ! printf '%s  %s\n' "$good" "$tmp" | SUM_CHECK >/dev/null 2>&1; then
  fail "a known-good checksum did not verify"
fi

# 2. A wrong checksum must be rejected (this is the abort path provision.sh relies on).
if printf '%s  %s\n' "$bad" "$tmp" | SUM_CHECK >/dev/null 2>&1; then
  fail "a bad checksum was accepted"
fi

echo "PASS: checksum verify accepts good, rejects bad"
