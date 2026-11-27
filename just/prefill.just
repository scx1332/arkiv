# The prefill fixtures' checks: crates/arkiv-prefill/fixtures/small, an archive
# set, and crates/arkiv-prefill/fixtures/sourcify, a Sourcify bundle: full
# prefills whose block 0 root and hash are committed in their expected.json.
#
# Prefills the fixtures with a built arkiv-reth, finalizes them, and checks block 0 against expected.json
[group('test')]
prefill_check binary=(bin_dir / "arkiv-reth"):
  scripts/prefill/check.sh "{{binary}}"

# Run it after a change meant to alter block 0, and commit the new file.
#
# Rewrites the prefill fixtures' expected.json from a built arkiv-reth
[group('test')]
prefill_fixture_update binary=(bin_dir / "arkiv-reth"):
  scripts/prefill/check.sh "{{binary}}" --update
