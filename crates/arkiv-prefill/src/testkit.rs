//! What the writer's and the run's tests share: GolemDB as the node opens it,
//! and archive lines.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_primitives::{Address, B256};
use arkiv_interfaces::primitives::EntityCreationNonce;
use arkiv_reth_statemanager::HostStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const CHAIN: u64 = 7_738_577;
pub(crate) const ALICE: &str = "0x1111111111111111111111111111111111111111";
pub(crate) const BOB: &str = "0x2222222222222222222222222222222222222222";

/// GolemDB's config as the node opens it on the example genesis: its prices,
/// and Arkiv's host weights.
fn config() -> arkiv_golemdb::GolemDbConfig {
    let spec = arkiv_reth_chainspec::ArkivChainSpec::from_genesis(
        serde_json::from_str(include_str!("../../../genesis/example.json")).unwrap(),
    )
    .unwrap();
    arkiv_golemdb::GolemDbConfig {
        genesis: spec.golemdb().unwrap().clone(),
        host_schemas: vec![arkiv_reth_chainspec::host_metering_schema()],
        segments: vec![],
    }
}

/// That GolemDB, in memory.
pub(crate) fn store() -> HostStore {
    Arc::new(arkiv_golemdb::GolemDbStore::open_memory(&config()).unwrap())
}

/// That GolemDB, on disk in `directory`: opening it again finds what was
/// committed before.
pub(crate) fn store_at(directory: &std::path::Path) -> HostStore {
    Arc::new(arkiv_golemdb::GolemDbStore::open(directory, &config(), None).unwrap())
}

/// One line: an entity `key` (a byte, repeated) created by `creator`, owned
/// by `owner`, expiring at `expires_at`, with `extra` fields.
pub(crate) fn line(key: u8, creator: &str, owner: &str, expires_at: u64, extra: &str) -> String {
    format!(
        r#"{{"$key":"0x{}","$creator":"{creator}","$owner":"{owner}","$expiresAt":{expires_at}{extra}}}"#,
        format!("{key:02x}").repeat(32)
    )
}

/// The key a Sourcify bundle's line `nonce` mints for Alice, the bundles'
/// owner here, on the test chain.
pub(crate) fn bundle_key(nonce: u64) -> B256 {
    let alice: Address = ALICE.parse().unwrap();
    B256::from(arkiv_reth_executor::derive_entity_address(
        CHAIN,
        &alice.0.0,
        EntityCreationNonce::new(nonce),
        0,
    ))
}

/// A Sourcify bundle line for `nonce`, permanent, with `attributes`.
pub(crate) fn bundle_line(nonce: u64, attributes: Value) -> Value {
    json!({
        "nonce": nonce,
        "key": bundle_key(nonce),
        "expiresAt": "0xffffffffffffffff",
        "attributes": attributes,
    })
}

/// A Sourcify bundle of Alice's on the test chain, in `dir`: `seed.json`
/// naming `parts` (path, lines), and a `report.json` that describes them when
/// `with_report`. Its `seed.json`.
pub(crate) fn write_bundle(dir: &Path, parts: &[(&str, Vec<Value>)], with_report: bool) -> PathBuf {
    let mut report_parts = Vec::new();
    let mut first_nonce = 0;
    for (name, lines) in parts {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, &text).unwrap();
        report_parts.push(json!({
            "path": name,
            "bytes": text.len(),
            "sha256": alloy_primitives::hex::encode(Sha256::digest(text.as_bytes())),
            "entities": lines.len(),
            "firstNonce": first_nonce,
        }));
        first_nonce += lines.len() as u64;
    }
    let seed = dir.join("seed.json");
    std::fs::write(
        &seed,
        json!({
            "version": 2, "format": crate::bundle::FORMAT, "chainId": CHAIN, "owner": ALICE,
            "schemaPrefix": "sourcify.v4", "firstNonce": 0,
            "entities": parts.iter().map(|(name, _)| json!({"path": name})).collect::<Vec<_>>(),
        })
        .to_string(),
    )
    .unwrap();
    if with_report {
        std::fs::write(
            dir.join("report.json"),
            json!({
                "schema": "sourcify-udc-import.v1",
                "entities": first_nonce,
                "kinds": {"chunk": first_nonce},
                "parts": report_parts,
            })
            .to_string(),
        )
        .unwrap();
    }
    seed
}
