# GolemDB genesis prefill: Sourcify bundles

A chain whose block 0 holds the Sourcify dataset: the export's v4 bundle is written straight into the node's GolemDB store, block 0's header carries the store's state root, and a node started on that genesis checks its store against the root and serves the entities over RPC.

This is the port of the `scx1332/golemdb-seed-genesis` branch (`arkiv-cli seed-genesis`, October 2026) onto current `main`. Upstream has since built its own prefill (#311: `arkiv-reth prefill`, archives, `prefilledBlockZeroStateRoot`, store-state checks), so the port is not a rebase: it adds what the Sourcify data needs on top of upstream's prefill.

- [What changed, and why](#what-changed-and-why)
- [Design](#design)
- [Running it: the mini bundle](#running-it-the-mini-bundle)
- [Running it: the full export](#running-it-the-full-export)
- [Measurements](#measurements)
- [Extrapolation to the full export](#extrapolation-to-the-full-export)
- [Tests](#tests)
- [Open questions and TODOs](#open-questions-and-todos)
- [Draft pull requests](#draft-pull-requests)

## What changed, and why

### The old branch against current main

| Old branch (`scx1332/golemdb-seed-genesis`) | Current main | Here |
|---|---|---|
| `arkiv-cli seed-genesis` writes the genesis commit | `arkiv-reth prefill` writes archives, then `--finalize` commits block 0 (#361–#368) | `arkiv-reth prefill --seed seed.json`: a bundle is one more input to upstream's prefill |
| genesis `stateHash` replaces the header's root (992c866) | `config.arkiv.prefilledBlockZeroStateRoot` (#339, #344) | upstream's field, unchanged |
| node refuses a store whose genesis commit has another root | store states and start-up refusals (#352–#354, #378) | upstream's checks, unchanged; one log line added when the check passes |
| `--commit-every N` chunks a single stream (2434b1f) | one GolemDB commit per archive | one commit per bundle part (10,000 entities in the export) |
| per-chunk seal/commit timing (4d1b14e) | one line per archive, printed only when the run ends | a line per part as it lands, plus progress, rate, ETA and memory |
| replay with `--expect-state-root` (2363bde) | the audit: `--finalize` against a finalized genesis file | upstream's audit, unchanged |
| reads `seed.json` **version 1** (`$key`-style lines) | reads archives (`$key`-style lines, indexed attributes only) | reads `seed.json` **version 2**, the v4 (UDC) export the mini and full bundles are in |
| GolemDB store from PRs #206/#207 (`golemdb://`, segments, `StoreExt`, query caching) | upstream's own GolemDB adapter (#202–#222, #312) | upstream's adapter; the old store work is obsolete |
| key attribute fix: `KEY` stored as `Int(W32)` (cbe1fb5) | `KEY` stored as `bytes32`, read back as `bytes32` (conformance item Q2, open) | GolemDB gets a `key` cell type; `key` attributes are stored as one |
| pruning map folds in expiring genesis entities (80e6093) | expiry of prefilled entities deferred to the new pruning design (#333) | not ported: Sourcify entities are permanent, and #333 owns it |
| golemdb pinned to `scx1332/arkiv-integration` (`query_keys`, `immutable_data_range_of`, batched cell checks, #69) | golemdb `v1.0.0`, which has #69 and its own queries | not needed; golemdb is `v1.0.0` plus one commit |
| synthetic `SeedSpec` generator, Valkey store, dev image (4ebc2b2) | Valkey removed (#256) | not ported |

Upstream's `crates/arkiv-seed` from September (#112, #118) no longer exists on main; upstream's `crates/arkiv-prefill` replaced it, and this work extends that crate rather than bringing `arkiv-seed` back.

### Three gaps between upstream's prefill and the Sourcify data

1. **The input format.** Upstream's archive lines carry indexed attributes only (plus `$payload` and `$contentType`). A v4 bundle line has typed, explicitly indexed or unindexed attributes, among them unindexed `bytes` and `key` values (`content`, `abi`, `compilationref`, …), a `nonce` and an owner given once per bundle. Converting would double 68 GB on disk and the archive format cannot say "unindexed `key`". So the prefill reads bundles natively: `crates/arkiv-prefill/src/bundle.rs`.
2. **Permanent expiry.** Every Sourcify entity expires at `2^64-1`, permanent. Upstream's executor refuses permanent expiry for transactions and, per #338, for prefilled lines too. So a prefill may now be *asked* to allow it: `--allow-permanent`, `ExecEnv::allow_permanent`.
3. **`key` attributes.** GolemDB stored `key` as 32 fixed bytes, the same as `bytes32`, so `compilationref` and friends read back over RPC as `bytes32` (and typed queries matched both types: conformance Q2). GolemDB now has a `key` cell type, and the adapter stores `key` attributes as one.

## Design

### Input: the bundle as published

```text
bundle/seed.json              version 2, "sourcify-arkiv.udc-bundle.v1": chainId, owner, parts in order
bundle/report.json            the export's report: counts per kind, each part's bytes, SHA-256, entities, first nonce
bundle/entities/000001.jsonl  {"nonce":0,"key":"0x…","expiresAt":"0xffffffffffffffff","attributes":[{name,type,indexed,value}…]}
```

- **One part is one archive, so one GolemDB commit.** A part's archive hash is upstream's archive hash with `seed.json` as the index and the part as its only part: it covers the bundle's chain, owner and part list as well as the part's bytes. Resume, the prefill record (`$prefillTotal` = number of parts), finalize, the audit and the node's start-up checks are upstream's, unchanged.
- **A line is the owner's create.** `owner` creates and keeps every entity (no flags, no transfer); attributes keep their `indexed` flag; values are in the node's RPC encoding, `bytes` in base64. Each becomes the same executor `Op::Create` an archive line becomes, so the same rules apply (attribute names and limits, GolemDB's cell limits, duplicate and reserved keys).
- **Checks on top of an archive's:**
  - every `key` must be the salt-0 key its `nonce` mints for `owner` on `chainId` (`derive_entity_address`), and `chainId` must be the genesis file's;
  - nonces run 0, 1, 2, … within a part, and a part's first nonce must equal the owner's minting nonce in the store when the part starts, so parts cannot be skipped or reordered, and after block 0 the owner's next transaction mints nonce N, a key no prefilled entity has;
  - with `report.json` beside `seed.json` (optional), it must list the same parts, and each part must have the size, SHA-256, entity count and first nonce it gives. A damaged or truncated part fails at its end, before it is committed.

### Streaming and bounded memory

The reader holds one line at a time (`MAX_LINE_BYTES`, 1 MiB); the writer holds one part's writes on one GolemDB branch until its commit, as upstream's does. Nothing is kept across parts, and no structure grows with the dataset: no key index, no list of parts' entities (the nonce check reads one counter from the store per part). The bundle is read once; the archive hash, the SHA-256 for the report check and the entities come from the same bytes.

Memory is therefore one part's worth of writes. The export's parts are 10,000 entities; the full bundle's largest is 132 MB. If a future export wrote much larger parts, the knob is the export's part size: there is no `--commit-every`, because the prefill record counts commits and so the split is part of block 0 (a different split, a different root), and the bundle's own split is the only one every auditor reproduces without extra parameters.

Resident memory has a second part that does grow: GolemDB is MDBX, memory-mapped, so the store's pages show up in RSS as file pages. They are page cache, which the kernel reclaims under pressure. The progress line reports the two separately (`memory … MiB, mapped files … MiB`, the latter mostly the store; the binary's own pages are in it too); `/usr/bin/time`'s maximum RSS adds them up.

### Permanent entities: `--allow-permanent`

`ExecEnv` has a new field, `allow_permanent`, false for every transaction, call and estimate (the node sets it false explicitly). When a prefill sets it, a create whose expiry is exactly `u64::MAX` skips the lifetime rule and buys no storage time (prefill is free, and permanence has no end to price). Only creates, and only exactly `u64::MAX`: an extend to permanent still reverts, and every other expiry goes through the rules as before (a prefill has no lifetime cap, but a far-future expiry on a large entity can still run its storage time past 64-bit gas, as upstream's own test shows).

Without the flag, a permanent line is refused with a message naming the flag. The flag must be given on every run of a prefill that has permanent entities (the first, any resume, the audit); it does not change the root of a run that succeeds.

After block 0, a permanent entity behaves as the executor already treats one: it can be transferred, deleted and patched without growing (patch growth would buy storage time until `u64::MAX`, which runs out of gas); it can never be extended (nothing to extend) and is never purged.

### `key` attributes as GolemDB keys

GolemDB (`golemdb-cells`) gets `CellType::RecordKey`, spelt `key`, type id 5 (a free singleton id): 32 bytes, stored and ordered as `bytes32` is, indexable, but its own type everywhere a type is asked for (typed operands, `ExistsType`, sort keys, immutable-data column spelling). arkiv's adapter maps a `KEY` attribute to it, so a `key` reads back as a `key` and `key`/`bytes32` predicates never match each other's attributes (conformance Q2, now done).

The entity's own `$key` cell keeps the 32 fixed bytes it has always had, so a record without key attributes keeps its root. A record with a key attribute does not: **this changes state roots** for any create or patch that writes a `key` attribute (see TODOs). The `small` fixture has one, so its `expected.json` changed (and the block 0 values quoted in `prefill.md` and `genesis-spec.md` with it).

### Progress reporting

Each part's line is printed as it lands, followed by one with the run's progress:

```text
entities/000001.jsonl  committed as commit 2 · 10000 entities · 2.4 s · 0xe15f909e…68c1
  1 of 2 parts · this run: 10000 entities in 2.4 s, 23.6 MB/s, 4136 entities/s · ETA 2.2 s · memory 95 MiB, mapped files 26 MiB
```

The ETA comes from the parts' sizes on disk and this run's rate. Memory is sampled just after the commit, when the part's writes are already released; the peak while a part is staged is higher (about 0.4–0.5 GB for the mini's 57 MB parts). Upstream printed the archives' lines only when the whole run ended, which for the full export would have been hours of silence.

### Node side

Unchanged from upstream: `ARKIV_STORE_PATH=… arkiv-reth node --chain genesis.json` reads `prefilledBlockZeroStateRoot`, refuses a store that is not this prefill's finalized block 0 (each case with its own message, `prefill-runbook.md`), and produces block 1 onwards. One `INFO` line now says when the check passed:

```text
INFO the Arkiv store holds block 0, with the state root its header carries block=0x8a90…1a24 root=0x30ce…2763 prefilled=true
```

## Running it: the mini bundle

The mini bundle is `~/scx1332/arkiv/sourcify-mini` (3,000 contracts, 18,412 entities, two parts, 108 MB). Its genesis is any Arkiv genesis file with `"chainId": 7706815`; the one used here is `crates/arkiv-prefill/fixtures/sourcify/genesis.json` (the example genesis, chain 7706815, funding the dev account). Work directory `~/scx1332/prefill-work/mini`.

```bash
cargo build --release -p arkiv-reth
BIN=$PWD/target/release/arkiv-reth
W=~/scx1332/prefill-work/mini/run-a && mkdir -p $W
cp crates/arkiv-prefill/fixtures/sourcify/genesis.json $W/genesis.json

$BIN prefill --chain $W/genesis.json --seed ~/scx1332/arkiv/sourcify-mini/seed.json --allow-permanent --store $W/golemdb
$BIN prefill --chain $W/genesis.json --seed ~/scx1332/arkiv/sourcify-mini/seed.json --allow-permanent --store $W/golemdb --finalize

ARKIV_STORE_PATH=$W/golemdb $BIN node --chain $W/genesis.json --dev --dev.block-time 2s \
  --http --http.addr 127.0.0.1 --http.port 9780 --authrpc.port 9781 --port 30387 \
  --datadir $W/reth --disable-discovery --ipcdisable
```

`~/scx1332/prefill-work/mini/start-node.sh RUN_DIR [PORT]` does the last step. `~/scx1332/prefill-work/sourcify-node-check.py` checks a running node against a bundle (not in the repo: it would need the repo's Python formatter, and the e2e test covers the same ground in CI):

```bash
python3 -I ~/scx1332/prefill-work/sourcify-node-check.py --rpc http://127.0.0.1:9780 \
  --bundle ~/scx1332/arkiv/sourcify-mini --genesis $W/genesis.json --all
```

It checks block 0's state root against the genesis file; every line (`--all`, or the first, the last and `--sample N` random lines of each part) against `arkiv_getEntity` (key, owner = creator = the bundle's owner, `createdAt` 0, `expiresAt`, every attribute's name, type and value) and its `attributeSchema` (indexed flags) via `arkiv_query '$key = key(…)'`; `arkiv_getEntityCount` in all and per kind against `report.json`; and pages `arkiv.schema = str('sourcify.v4.contract')` to the end, comparing the keys with the bundle's contracts. (The query language's string literal is `str('…')`; `"…"` quotes an attribute *name*.)

## Running it: the full export

The full bundle is `~/scx1332/sourcify-data/bundle-v4`: 16,767,511 entities (9,247,189 contracts), 1,677 parts, 68 GB, owner `0x73d44f15…fc72`, chain 7706815. **Not run here**; this is how one would.

```bash
D=/data/sourcify-genesis            # ≥ 150 GB free: ~100 GB store + headroom; the bundle stays where it is
mkdir -p $D && cp genesis.json $D/  # chainId 7706815; settle alloc, prices, golemdb settings first:
                                    # the prefill records the file's hash and refuses any later edit
BIN=/path/to/release/arkiv-reth
SEED=~/scx1332/sourcify-data/bundle-v4/seed.json

# 1. The parts. Resumable: after a crash or Ctrl-C, run the same command again;
#    it checks nothing changed and continues after the last committed part.
nohup $BIN prefill --chain $D/genesis.json --seed $SEED --allow-permanent --store $D/golemdb > $D/prefill.log 2>&1 &
tail -f $D/prefill.log              # one line per part, with progress, ETA and memory

# 2. Block 0: writes prefilledBlockZeroStateRoot into genesis.json, prints the root and block 0's hash.
$BIN prefill --chain $D/genesis.json --seed $SEED --allow-permanent --store $D/golemdb --finalize

# 3. Keep the block 0 store (what joining nodes copy; ~100 GB), then start the node.
cp -R $D/golemdb $D/golemdb-block0
ARKIV_STORE_PATH=$D/golemdb $BIN node --chain $D/genesis.json …

# 4. Check it.
python3 -I sourcify-node-check.py --rpc http://127.0.0.1:PORT --bundle ~/scx1332/sourcify-data/bundle-v4 \
  --genesis $D/genesis.json --sample 25
```

Notes:

- The bundle's keys are minted for chain 7706815. A chain with another id needs the bundle re-keyed first (the processor's `udc-rekey`); the prefill refuses a mismatch before writing anything.
- `report.json` is checked part by part (SHA-256, size, entities, first nonce), so a separate `sha256sum -c SHA256SUMS` is not needed for the parts.
- An auditor reproduces block 0 with the same two prefill commands into an empty store and the *finalized* `genesis.json`; `--finalize` then compares instead of writing (`it matches …`).
- `arkiv_getEntityCount` without a query counts every live entity: 1.6 s at 552k entities, so expect tens of seconds at 16.8M. Counting by an indexed attribute stays fast.

## Measurements

Machine: 12 cores, 62 GB RAM, `/dev/md2` RAID 0. Release build of this branch. Wall times from `/usr/bin/time -v`.

### The mini bundle (acceptance)

| | run a | run b |
|---|---|---|
| prefill (2 parts, 18,412 entities, 108.0 MB) | 5.19 s | 5.07 s |
| rate | 3,610 entities/s, 21.2 MB/s | 3,655 entities/s, 21.4 MB/s |
| part 1 / part 2 (10,000 / 8,412 entities) | 2.4 s / 2.7 s | 2.4 s / 2.6 s |
| max RSS (process memory + mapped store pages) | 405 MiB | 409 MiB |
| finalize | 0.14 s, 45 MiB | 0.14 s, 45 MiB |
| store on disk (`du`) | 161 MiB (167,772,610 B) | 161 MiB |
| block 0 state root | `0x30ce842833dd5a0e36de235ddb8ca7912c1765aecd274337ab18822454ad2763` | same |
| block 0 hash | `0x8a906c4441c673e749f6c1d3d40c7e73ac4b2930ddb4bada2729b13bd6db1a24` | same |

The two finalized genesis files are byte-identical, and so is a third run's (run c, with the final binary: 5.14 s). Counts per part and per kind match `report.json`: 10,000 + 8,412 entities; `source` 6,388, `code` 6,632, `compilation` 2,361, `contract` 3,000, `chunk` 31.

The node on run a's store: RPC answering 0.45 s after start; the start-up check logged above; then every one of the 18,412 entities equal to its bundle line over RPC (attributes, types, values, indexed flags; `--all`, 26.6 s), `arkiv_getEntityCount` 18,412 in all and per kind as above, and `arkiv.schema = str('sourcify.v4.contract')` paged to exactly the bundle's 3,000 contract keys (31 pages). The node on run b's store (the second, independent seeding) passed the same checks, and restarted on its store past block 0. Node RSS ≈ 260 MiB.

### A 30× probe for the extrapolation

The full bundle is not run here. To see how the cost grows with the store, a synthetic bundle was made from the mini one: 30 copies, each line re-keyed to its new nonce (attribute values unchanged, so references dangle, which the prefill does not check): 552,360 entities, 56 parts, 3.24 GB. (Generator: `~/scx1332/prefill-work/scale_probe.rs`, an ignored test, not committed.)

| | |
|---|---|
| prefill | 3 m 57.6 s, 2,325 entities/s, 13.6 MB/s |
| time per 10,000-entity part | 2.5 s (empty store) → 3.5 s (part 4) → 4.3 s (part 13) → 4.7 s (part 35) → 4.8 s (part 55) |
| process memory (`RssAnon`) | ~0.5 GB, flat |
| mapped store pages (`RssFile`) | grows with the store, 2.8 GB at part 44; max RSS 4.1 GiB |
| store on disk | 4.70 GB, 1.45× the bundle |
| finalize | 0.12 s |
| node start to RPC | 0.45 s, as on the mini store: the start-up checks read a few records, not the entities |
| `arkiv_getEntityCount`, all / one kind | 1.62 s / 0.01 s |

## Extrapolation to the full export

16,767,511 entities in 1,677 parts, 68 GB (parts average 40 MB and 10,000 entities; the largest is 132 MB).

- **Disk.** The store is 1.45–1.55× the bundle's bytes: **~100–105 GB**, plus MDBX headroom. With the bundle on the same disk (68 GB), ~170 GB in all; 334 GB are free. The block 0 copy for joining nodes is another ~100 GB (or a reflink/snapshot).
- **Memory.** Process memory is one part's writes: ~0.5 GB for the mini's 57 MB parts, so **≲ 1.5 GB** for the largest 132 MB part. The mapped store pages grow toward whatever page cache the machine has (62 GB here) and are reclaimable; `/usr/bin/time` will report them as RSS.
- **Time.** Per-part time grows roughly with the logarithm of the store's size: about `2.8 s + 0.5 s × ln(part number)` per 10,000 entities on the probe. Carried to 1,677 parts that is ~6 s a part on average, **~2.8 h**, as long as the store's working set stays in page cache. The full store (~100 GB) outgrows the 62 GB of RAM partway through; past that, B-tree pages are read back from disk and parts get slower. **Expect 3–6 h on this machine**, the upper end if the disk is the limit. The progress line's ETA tracks it as it goes.
- **Interruptions** cost at most the part being written (seconds); a rerun resumes after the last committed part.
- **The node** starts on the full store in about the same time as on the mini one; queries by indexed attribute stay fast; a count of everything grows with the store (above).

## Tests

- `crates/arkiv-prefill/src/bundle.rs`: a line becomes the owner's create (every attribute type, indexed and not); every malformed line names what is wrong (key not minted by its nonce, bad quantity, unknown field, bad base64, indexed `bytes`, …); nonces in order and from where the report says; a part must be the one the report describes (size, SHA-256, count); a bundle opens as one archive per part with the archive hash of `seed.json` + part; bad `seed.json`/`report.json` refused; a damaged part refused at its end with no hash; a bundle without a report opens.
- `crates/arkiv-prefill/src/writer.rs`: a bundle's parts become the owner's permanent entities with unindexed `bytes`/`key` attributes read back as written and the minting nonce at the bundle's count; permanent refused without `allow_permanent`; a part out of order refused; another chain's bundle refused.
- `crates/arkiv-reth-executor/src/arkiv.rs`: a prefill that allows it creates a permanent entity with no storage gas; one block short of permanent, and an extend to permanent, still revert.
- `crates/arkiv-golemdb`: `key` and `bytes32` predicates (presence, equality, range) each find only their own type; `key` cells read back as `KEY`, `$key` stays 32 fixed bytes; query lowering pins `key` operands and sort keys to GolemDB's `key`.
- `bin/arkiv-reth/src/prefill.rs`: on the committed fixture, the CLI refuses permanent entities without the flag, resumes with it, prints the parts and progress, finalizes to `expected.json`, a second store gives the same block 0 (audit), the node's store checks accept it; a bundle for another chain writes nothing.
- `bin/arkiv-reth/tests/prefill.rs` (e2e, real binary and node): the Sourcify fixture prefilled twice to the same block 0; a node on it serves every line exactly (attributes and indexed flags), counts per kind as `report.json`, answers an attribute query, and the owner's next create mints nonce 9's key.
- `scripts/prefill/check.sh` (`just prefill_check`, run by CI after the build): both fixtures, `small` and `sourcify`, against their `expected.json`.
- golemdb: cell vectors and round trips for `key`, the type-id space, markers, YAML column spelling, and the query property tests (`key` and `bytes32` of the same word told apart, ranges per type).

Results on this branch: `cargo fmt --check` clean; `cargo clippy --all-targets --locked --profile dev-fast --no-deps -- -D warnings` clean; `cargo nextest run --locked --cargo-profile dev-fast`: all pass; doc tests pass; golemdb `fmt`/`clippy -D warnings`/`nextest` pass on `v1.0.0` + the commit (715 tests) and on golemdb `main` + the same commit, cherry-picked cleanly (720 tests).

## Open questions and TODOs

1. **Permanent prefilled entities vs #338.** #338 decided that "the executor still refuses the permanent value" for prefill. The Sourcify data is permanent by design, so this adds an explicit, off-by-default exemption (`--allow-permanent`). Needs upstream sign-off; the alternative is a finite far-future expiry chosen by the export, which would need a pricing story for storage time that overflows 64-bit gas.
2. **The `key` cell type changes state roots** for every create or patch that writes a `key` attribute. A chain already running v1.0.x on GolemDB with key attributes would fork on upgrade: it needs a coordinated upgrade (or the new type gated by block height), or only new chains get it. Old `key` cells keep reading back as `bytes32`. Worth deciding before upstream merges either PR.
3. **golemdb is pinned to a fork commit** (`scx1332/golemdb@4fb94375985b979672f4eb511e58ca3f3a025061`, v1.0.0 + one commit) until the change lands upstream and is tagged; then switch back to `Arkiv-Network/golemdb` at that tag. The same commit applies cleanly to golemdb `main` (tested), which is where its PR should go; arkiv main itself still pins `v1.0.0`, and golemdb `main` has breaking changes (flat index, #108) that arkiv has not taken yet.

   **That commit is not on GitHub yet.** The token used for this work may push to `scx1332/arkiv` but not to `scx1332/golemdb` (HTTP 403), so the golemdb branch exists only in the local clone `~/scx1332/golemdb` (branch `scx1332/golemdb-genesis-prefill`, plus `scx1332/golemdb-key-cell-type-main`, the same commit on golemdb `main`, in the worktree `~/scx1332/prefill-work/golemdb-main`; a patch is at `~/scx1332/prefill-work/0001-feat-cells-a-key-cell-type-so-a-record-reference-is-.patch`). Until it is pushed, a clean build of this branch cannot fetch golemdb. To publish it:

   ```bash
   cd ~/scx1332/golemdb && git push origin scx1332/golemdb-genesis-prefill
   ```

   Before that, this branch builds against the local clone with git's URL rewrite, which leaves `Cargo.lock` naming the GitHub URL and the same commit:

   ```bash
   GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.file://$HOME/scx1332/golemdb.insteadOf \
   GIT_CONFIG_VALUE_0=https://github.com/scx1332/golemdb CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build
   ```
4. **Commit granularity is the export's part size** (10,000 entities). Fine for the current export; there is no `--commit-every`, since a different split gives a different block 0. If an export ever writes parts too large for memory, the fix belongs in the export.
5. **`report.json` is optional.** Without it, parts are checked line by line only (keys, nonces). `SHA256SUMS` is not read (the report already covers the parts, and `seed.json` is covered by every part's archive hash).
6. **Counting everything** (`arkiv_getEntityCount` with no query) is linear in the store: tens of seconds at 16.8M entities. Not a prefill issue, but the Sourcify chain will see it.
7. **Pruning of prefilled entities** stays with #333; permanent ones never expire.
8. **Running the full export** on a machine like this one: ~3–6 h, ~100 GB, ≲ 1.5 GB of process memory (above). Not done in this change.

## Draft pull requests

### arkiv (from `scx1332/arkiv:scx1332/golemdb-genesis-prefill` to `Arkiv-Network/arkiv:main`)

**Title:** feat(prefill): prefill a Sourcify bundle as it is published (`--seed`, `--allow-permanent`), and keep `key` attributes keys

**Body:**

> Lets `arkiv-reth prefill` start a chain whose block 0 holds the Sourcify dataset, read straight from the export's v4 bundle (`seed.json`, `report.json`, JSON Lines parts) with no conversion step. Design, measurements and how to run the full 16.8M-entity export: `docs/golemdb-genesis-prefill.md`.
>
> **Prefill**
> - `arkiv-reth prefill --seed seed.json` in place of `--archives set.json`: each bundle part is one archive, so one GolemDB commit; resume, finalize, the audit and the node's start-up checks are the existing ones. New module `arkiv_prefill::bundle`.
> - A line is the bundle owner's create with its typed, indexed or unindexed attributes. On top of an archive's checks: each key must be the salt-0 key its nonce mints for the owner on the genesis file's chain; nonces run on without a gap across the parts, each part starting at the owner's minting nonce in the store (so the owner's next transaction mints a fresh key); with `report.json`, each part must have the size, SHA-256, count and first nonce it gives, checked before the part is committed.
> - `--allow-permanent` (`ExecEnv::allow_permanent`, off everywhere else): a prefill asked to may create entities that never expire, buying no storage time. Sourcify entities are all permanent. This is an explicit exception to #338's "the executor still refuses the permanent value", for prefill only.
> - Each part (and archive) is reported as it lands, with progress, rate, ETA and memory; the lines used to be printed only when the whole run ended.
> - A Sourcify fixture (`crates/arkiv-prefill/fixtures/sourcify`, 9 entities of every kind and type), checked by `just prefill_check` and by a new e2e test that serves every line from a node.
>
> **`key` attributes (conformance Q2)**
> - GolemDB gets a `key` cell type (Arkiv-Network/golemdb PR: "feat(cells): a `key` cell type …"); the adapter stores `key` attributes as one, so they read back as `key` (they read back as `bytes32`), and `key`/`bytes32` predicates no longer match each other. The entity's own `$key` is unchanged.
> - **Changes state roots** of blocks that write `key` attributes, hence the `small` fixture's new `expected.json`. Existing chains need a coordinated upgrade.
> - Pins golemdb to the fork commit with the change until it is tagged upstream.
>
> **Node:** logs when the store's block 0 matched the genesis header's root.
>
> Tested: fmt, clippy (`--all-targets -D warnings`), nextest and doc tests; the mini Sourcify bundle (18,412 entities) prefilled twice to the same block 0 in ~5 s each, every entity checked over RPC on a node started on it.

### golemdb (from `scx1332/golemdb:scx1332/golemdb-genesis-prefill`, rebased on `main`, to `Arkiv-Network/golemdb:main`)

**Title:** feat(cells): a `key` cell type, so a record reference is not bytes32

**Body:**

> A reference to another record could only be stored as `bytes32`, so a reader could not tell it from 32 opaque bytes. Arkiv stores entity references that way and reads every `key` attribute back as `bytes32` (arkiv's query-language conformance item Q2), and an equality, range or presence filter on either type matched both.
>
> - `CellType::RecordKey`, spelt `key`, takes the free singleton id 5: 32 bytes, stored and ordered as `bytes32`, indexable.
> - It is its own type everywhere a type is asked for: `CellValue::from_record_key` / `as_record_key`, the `marker::RecordKey` marker for typed query operands, `ExistsType`, sort keys, and the immutable-data column spelling `key`.
> - Existing cells, and so existing roots, are unchanged; only a cell written as `key` is new.
> - Docs: the type tables in `golemdb-design.md` and `golemdb-api.md`.
>
> Tests: cell vectors and round trips, the type-id space, the markers' build-time check, YAML column names, and the query property tests now include `key` (and tell it apart from a `bytes32` of the same word). fmt, clippy `-D warnings` and nextest pass on `main` and on `v1.0.0`, which arkiv pins today.
