# Prefill fixture: sourcify

A Sourcify bundle in the export's v4 (UDC) format, small enough to read: `seed.json`, `report.json`, `SHA256SUMS` and two parts, nine entities, as `arkiv-reth prefill --seed` reads a real one. The values are made up but shaped like the export's: every `bytes` value carries the export's codec byte (`00`, identity), hashes are the real SHA-256 or keccak of what they describe, and references name earlier entities.

The bundle's owner is the dev account `DEV_KEY_0` (`0xf39f…2266`) on chain 7706815, Sourcify's Arkiv chain, so a test can send that account's next transaction. `genesis.json` is the example genesis with that chain id, funding only the owner.

| Part | Nonce | Kind | What it holds |
|---|---|---|---|
| `entities/000001.jsonl` | 0, 1 | `source` | two Solidity files (`content`, `hash`) |
| | 2 | `code` | creation bytecode |
| | 3 | `compilation` | `abi`, `manifest`, … and `key` references to codes 2 and 4 |
| | 4 | `code` | runtime bytecode |
| | 5 | `contract` | `Greeter`: every attribute type the format has, indexed and not |
| `entities/000002.jsonl` | 6 | `chunk` | a chunk of a long value |
| | 7 | `code` | another runtime bytecode |
| | 8 | `contract` | `GreeterV2`, sharing compilation 3 |

Every entity is permanent (`expiresAt` `0xffffffffffffffff`), so the prefill needs `--allow-permanent`. Every key is the one its nonce mints for the owner, so after block 0 the owner's next create mints the key of nonce 9.

`expected.json` holds block 0's state root and hash after prefilling and finalizing this fixture. `just prefill_check` checks it, as for the `small` fixture, and the `prefill` e2e test starts a node on the store and reads every line back over RPC.
