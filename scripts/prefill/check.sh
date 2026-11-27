#!/usr/bin/env bash
# Prefill the committed fixtures with a built arkiv-reth and check that block 0
# comes out as each fixture's expected.json says.
#
#   scripts/prefill/check.sh [path/to/arkiv-reth]           # check
#   scripts/prefill/check.sh [path/to/arkiv-reth] --update  # rewrite expected.json
#
# The fixtures are crates/arkiv-prefill/fixtures/small, an archive set, and
# crates/arkiv-prefill/fixtures/sourcify, a Sourcify bundle (prefilled with
# --seed and --allow-permanent).
#
# Block 0 depends only on a fixture's files, so a mismatch means the prefill
# now writes different state (GolemDB's encoding, the executor's writes, the
# archive format). If that is intended, rerun with --update and commit the file.
set -euo pipefail

bin=${1:-target/debug/arkiv-reth}
update=${2:-}

fail() {
  echo "error: $*" >&2
  exit 1
}

[ -x "$bin" ] || fail "no arkiv-reth at $bin; build it with 'just build'"

works=()
trap 'rm -rf "${works[@]}"' EXIT

# check FIXTURE: prefill a copy of it, finalize, and compare block 0.
check() {
  local fixture=$1 work input
  # Work on a copy: the finalize rewrites the genesis file.
  work=$(mktemp -d)
  works+=("$work")
  cp -R "$fixture/." "$work/"
  if [ -f "$work/seed.json" ]; then
    input=(--seed "$work/seed.json" --allow-permanent)
  else
    input=(--archives "$work/set.json")
  fi
  prefill() {
    "$bin" prefill --chain "$work/genesis.json" "${input[@]}" --store "$work/golemdb" "$@"
  }

  echo "== $fixture: prefill"
  prefill

  echo "== $fixture: prefill again: nothing left to write"
  prefill | tee "$work/again.log"
  ! grep -q "committed as commit" "$work/again.log" || fail "the second run wrote an archive again"

  echo "== $fixture: finalize"
  prefill --finalize | tee "$work/finalize.log"

  echo "== $fixture: finalize again: must be refused"
  ! prefill --finalize || fail "a second finalize was accepted"

  # What this run produced, in expected.json's shape.
  local root hash actual
  root=$(jq -r .config.arkiv.prefilledBlockZeroStateRoot "$work/genesis.json")
  hash=$(grep -o 'hash 0x[0-9a-f]*' "$work/finalize.log" | cut -d' ' -f2)
  actual=$(jq -n --arg root "$root" --arg hash "$hash" '{root: $root, blockZeroHash: $hash}')

  if [ "$update" = --update ]; then
    echo "$actual" >"$fixture/expected.json"
    echo "wrote $fixture/expected.json"
    return
  fi

  diff <(jq . "$fixture/expected.json") <(echo "$actual") ||
    fail "block 0 differs from $fixture/expected.json; if intended, rerun with --update and commit it"
  echo "ok: block 0 matches $fixture/expected.json"
}

for fixture in crates/arkiv-prefill/fixtures/small crates/arkiv-prefill/fixtures/sourcify; do
  check "$fixture"
done
