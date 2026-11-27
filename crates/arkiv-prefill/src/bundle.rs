//! Sourcify bundles: the Sourcify export's v4 (UDC) bundle as prefill input,
//! read as it is published, with no conversion step in between.
//!
//! # The bundle
//!
//! ```text
//! bundle/seed.json              the index: chain, owner, the parts in order
//! bundle/report.json            the export's report: counts, and each part's size, SHA-256 and entities
//! bundle/entities/000001.jsonl  one entity per line, in creation order
//! bundle/entities/000002.jsonl
//! ```
//!
//! ```json
//! {"version": 2, "format": "sourcify-arkiv.udc-bundle.v1", "chainId": 7706815,
//!  "owner": "0xa804…", "schemaPrefix": "sourcify.v4", "firstNonce": 0,
//!  "entities": [{"path": "entities/000001.jsonl"}, {"path": "entities/000002.jsonl"}]}
//! ```
//!
//! `version` is 2 and `format` is `sourcify-arkiv.udc-bundle.v1`; no other
//! field is allowed. Part paths are relative to `seed.json` and must stay
//! inside its directory, as an archive's parts must.
//!
//! **Each part is one archive**, so one GolemDB commit: what an archive's size
//! is to an archive set, a part's is to a bundle. The export writes 10,000
//! entities a part, about 50–130 MB. A part's archive hash is the
//! [archive hash](crate#archive-hash) with `seed.json` as the index and the
//! part as its only part, so it covers the bundle's chain, owner and part list
//! as well as the part's bytes.
//!
//! # Lines
//!
//! ```json
//! {"nonce": 20, "key": "0x19fb…", "expiresAt": "0xffffffffffffffff", "attributes": [
//!   {"name": "address", "type": "addr", "indexed": true, "value": "0x4f93…"},
//!   {"name": "arkiv.schema", "type": "str", "indexed": true, "value": "sourcify.v4.contract"},
//!   {"name": "data", "type": "bytes", "indexed": false, "value": "AaVQ3U…"}]}
//! ```
//!
//! Every field is required and no other is allowed:
//!
//! | field        | value                                                                  |
//! |--------------|------------------------------------------------------------------------|
//! | `nonce`      | the owner's minting nonce for this create: 0, 1, 2, … over the parts   |
//! | `key`        | the key a transaction would mint: the salt-0 key of (chain, owner, nonce) |
//! | `expiresAt`  | a hex quantity; `0xffffffffffffffff` is permanent (see below)          |
//! | `attributes` | `{name, type, indexed, value}`, the node's RPC encoding but for `bytes` |
//!
//! | `type`    | `value`                     | indexable |
//! |-----------|-----------------------------|-----------|
//! | `str`     | a string                    | yes       |
//! | `i32`     | a number                    | yes       |
//! | `u64`     | a hex quantity, `"0x1f"`    | yes       |
//! | `bool`    | `true` or `false`           | yes       |
//! | `addr`    | 20 bytes hex                | yes       |
//! | `key`     | an entity key, 32 bytes hex | yes       |
//! | `bytes32` | 32 bytes hex                | yes       |
//! | `bytes`   | base64, canonical           | no        |
//!
//! The bundle's `owner` creates every entity and keeps it: no creation flags,
//! no transfer. Its entities are read into the same executor ops as an
//! archive line's, and so meet the same rules (see [`crate::write_archive`]).
//!
//! # Checks
//!
//! On top of what every archive is checked for:
//!
//! - **Keys.** A line's key must be the salt-0 key of the bundle's chain, its
//!   owner and the line's nonce: the key a transaction would have minted. The
//!   bundle's `chainId` must be the genesis file's: keys of another chain are
//!   refused rather than imported under this one (the export can re-key a
//!   bundle for another chain).
//! - **Nonces.** Nonces run 0, 1, 2, … with no gap across the parts in order:
//!   within a part line by line, and a part's first line at the owner's
//!   minting nonce in the store when the part begins. So after block 0 the
//!   owner's minting nonce is the bundle's entity count, and its next
//!   transaction mints a key no prefilled entity has.
//! - **The report.** When `report.json` sits beside `seed.json`, it must list
//!   the same parts, and each part must have the size, SHA-256, entity count
//!   and first nonce it gives. A part that does not is refused before it is
//!   committed: a damaged copy of a bundle is not imported.
//!
//! # Permanent entities
//!
//! Sourcify entities are permanent: `expiresAt` is `u64::MAX`. No transaction
//! may create a permanent entity, and a prefill may not either unless it is
//! asked to (`arkiv-reth prefill --allow-permanent`), see
//! [`ExecEnv::allow_permanent`](arkiv_interfaces::execution::ExecEnv::allow_permanent).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use arkiv_interfaces::constants::OP_MAX_ATTRIBUTES;
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags};
use arkiv_interfaces::execution::Op;
use arkiv_interfaces::primitives::{EntityCreationNonce, UserAddress};
use arkiv_reth_executor::derive_entity_address;
use base64::{Engine, engine::general_purpose::STANDARD};
use eyre::{Result, WrapErr, ensure};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::reader::{check_attribute_name, check_length};
use crate::{Archive, Format, inside};

/// The `seed.json` version of a v4 bundle. Version 1 was the payload bundle of
/// the v3 export, which this does not read.
pub const SEED_VERSION: u64 = 2;

/// The `seed.json` format of a v4 bundle.
pub const FORMAT: &str = "sourcify-arkiv.udc-bundle.v1";

/// The salt the export's keys are minted with.
const SALT: u128 = 0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Seed {
    version: u64,
    format: String,
    chain_id: u64,
    owner: Address,
    schema_prefix: String,
    first_nonce: u64,
    entities: Vec<SeedPart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedPart {
    path: PathBuf,
}

/// What `report.json` says that is checked here. The report has more; the
/// rest is the export's business.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    entities: u64,
    #[serde(default)]
    kinds: BTreeMap<String, u64>,
    parts: Vec<ReportPart>,
}

/// One part as `report.json` describes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportPart {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    entities: u64,
    first_nonce: u64,
}

/// A bundle part, as an archive's format: what its lines are read with.
#[derive(Debug, Clone)]
pub(crate) struct Part {
    /// The part's path, for messages.
    pub(crate) path: PathBuf,
    chain_id: u64,
    owner: Address,
    report: Option<ReportPart>,
}

impl Part {
    /// That the bundle's keys are minted for the chain `chain_id`, the
    /// genesis file's.
    pub(crate) fn check_chain(&self, chain_id: u64) -> Result<()> {
        ensure!(
            self.chain_id == chain_id,
            "{}: the bundle's keys are minted for chain {}, but the genesis file is chain \
             {chain_id}. Prefill a bundle exported (or re-keyed) for this chain",
            self.path.display(),
            self.chain_id
        );
        Ok(())
    }
}

/// An opened Sourcify bundle.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The chain its keys were minted for.
    pub chain_id: u64,
    /// The account that creates and owns every entity.
    pub owner: Address,
    /// The prefix of its `arkiv.schema` values, `sourcify.v4`.
    pub schema_prefix: String,
    /// How many entities `report.json` says it holds, if it has one.
    pub entities: Option<u64>,
    /// `report.json`'s entities per kind (`contract`, `code`, …), if any.
    pub kinds: BTreeMap<String, u64>,
    /// Its parts, in order, one archive each.
    pub archives: Vec<Archive>,
}

/// The Sourcify bundle whose index is `seed`, opened: see the
/// [module docs](self). Nothing but `seed.json` and `report.json` is read
/// here; the parts are read as they are written.
pub fn open_bundle(seed: impl AsRef<Path>) -> Result<Bundle> {
    let seed = seed.as_ref();
    let at = seed.display();
    let bytes = std::fs::read(seed).wrap_err_with(|| format!("open {at}"))?;
    let parsed: Seed = serde_json::from_slice(&bytes).wrap_err_with(|| format!("parse {at}"))?;
    ensure!(
        parsed.version == SEED_VERSION && parsed.format == FORMAT,
        "{at}: version {} of {:?} is not a Sourcify v4 bundle: that is version {SEED_VERSION} of \
         {FORMAT:?}",
        parsed.version,
        parsed.format
    );
    ensure!(
        parsed.owner != Address::ZERO,
        "{at}: the owner is the zero address"
    );
    // The owner's minting nonce starts at 0 on a new chain.
    ensure!(
        parsed.first_nonce == 0,
        "{at}: firstNonce is {}: a bundle a chain starts from begins at nonce 0",
        parsed.first_nonce
    );
    ensure!(!parsed.entities.is_empty(), "{at}: names no part");
    let mut seen = std::collections::BTreeSet::new();
    for part in &parsed.entities {
        inside(seed, "part", &part.path)?;
        ensure!(
            seen.insert(&part.path),
            "{at}: part {} is named twice",
            part.path.display()
        );
    }

    let root = seed.parent().unwrap_or(Path::new("."));
    let report = read_report(&root.join("report.json"), &parsed)?;
    let archives = parsed
        .entities
        .iter()
        .enumerate()
        .map(|(n, part)| Archive {
            index: seed.to_path_buf(),
            index_bytes: bytes.clone(),
            parts: vec![root.join(&part.path)],
            format: Format::Sourcify(Box::new(Part {
                path: part.path.clone(),
                chain_id: parsed.chain_id,
                owner: parsed.owner,
                report: report.as_ref().map(|r| r.parts[n].clone()),
            })),
        })
        .collect();
    Ok(Bundle {
        chain_id: parsed.chain_id,
        owner: parsed.owner,
        schema_prefix: parsed.schema_prefix,
        entities: report.as_ref().map(|r| r.entities),
        kinds: report.map(|r| r.kinds).unwrap_or_default(),
        archives,
    })
}

/// `report.json` at `path`, if there is one: it must list `seed`'s parts, in
/// the same order.
fn read_report(path: &Path, seed: &Seed) -> Result<Option<Report>> {
    let at = path.display();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).wrap_err_with(|| format!("open {at}")),
    };
    let report: Report = serde_json::from_slice(&bytes).wrap_err_with(|| format!("parse {at}"))?;
    let listed: Vec<_> = report.parts.iter().map(|p| &p.path).collect();
    let named: Vec<_> = seed.entities.iter().map(|p| &p.path).collect();
    ensure!(
        listed == named,
        "{at} lists other parts than seed.json: {listed:?}, not {named:?}"
    );
    let total = report.parts.iter().map(|p| p.entities).sum::<u64>();
    ensure!(
        total == report.entities,
        "{at}: its parts hold {total} entities, but it counts {}",
        report.entities
    );
    Ok(Some(report))
}

/// A bundle line as it is written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Line {
    nonce: u64,
    key: B256,
    expires_at: String,
    attributes: Vec<LineAttribute>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LineAttribute {
    name: String,
    #[serde(rename = "type")]
    tag: String,
    indexed: bool,
    value: Value,
}

/// What reading a part checks as it goes: the nonces line by line, and at its
/// end what `report.json` says about it.
pub(crate) struct PartCheck {
    /// The nonce the next line must have, once a line was read.
    next_nonce: Option<u64>,
    /// The last line's nonce.
    nonce: Option<u64>,
    entities: u64,
    /// The part's SHA-256 so far, when there is a report to check it against.
    sha256: Option<Sha256>,
}

impl PartCheck {
    pub(crate) fn new(part: &Part) -> Self {
        Self {
            next_nonce: None,
            nonce: None,
            entities: 0,
            sha256: part.report.as_ref().map(|_| Sha256::new()),
        }
    }

    /// The last line's nonce.
    pub(crate) fn nonce(&self) -> Option<u64> {
        self.nonce
    }

    /// Take in `bytes`, the next bytes of the part.
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        if let Some(sha256) = &mut self.sha256 {
            sha256.update(bytes);
        }
    }

    /// One line, the part's next: its creator and its ops.
    pub(crate) fn line(
        &mut self,
        part: &Part,
        line: &str,
    ) -> Result<(UserAddress, Vec<Op>), String> {
        let (nonce, creator, ops) = parse_line(part, line)?;
        let expected = match self.next_nonce {
            Some(next) => next,
            None => part.report.as_ref().map_or(nonce, |r| r.first_nonce),
        };
        if nonce != expected {
            return Err(match self.next_nonce {
                Some(_) => format!(
                    "nonce {nonce}, but the line before has {}: a bundle's lines are in \
                     creation order",
                    expected - 1
                ),
                None => {
                    format!("nonce {nonce}, but report.json says the part starts at {expected}")
                }
            });
        }
        self.next_nonce = Some(
            nonce
                .checked_add(1)
                .ok_or_else(|| "nonce past u64::MAX".to_owned())?,
        );
        self.nonce = Some(nonce);
        self.entities += 1;
        Ok((creator, ops))
    }

    /// At the part's end, `len` bytes long: that it is the part `report.json`
    /// describes.
    pub(crate) fn finish(&mut self, part: &Part, len: u64) -> Result<(), String> {
        let (Some(report), Some(sha256)) = (&part.report, self.sha256.take()) else {
            return Ok(());
        };
        let digest = alloy_primitives::hex::encode(sha256.finalize());
        if len != report.bytes || !digest.eq_ignore_ascii_case(&report.sha256) {
            return Err(format!(
                "is {len} bytes with SHA-256 {digest}, but report.json says {} bytes with {}: \
                 the bundle is damaged or incomplete",
                report.bytes, report.sha256
            ));
        }
        if self.entities != report.entities {
            return Err(format!(
                "holds {} entities, but report.json says {}",
                self.entities, report.entities
            ));
        }
        Ok(())
    }
}

/// One bundle line's nonce, creator and ops, or what is wrong with it.
fn parse_line(part: &Part, line: &str) -> Result<(u64, UserAddress, Vec<Op>), String> {
    let line: Line = serde_json::from_str(line).map_err(|e| {
        if e.is_data() {
            e.to_string()
        } else {
            format!("not a JSON object: {e}")
        }
    })?;
    let derived = derive_entity_address(
        part.chain_id,
        &part.owner.0.0,
        EntityCreationNonce::new(line.nonce),
        SALT,
    );
    if line.key.0 != derived {
        return Err(format!(
            "key {} is not the key nonce {} mints for {} on chain {}, {}",
            line.key,
            line.nonce,
            part.owner,
            part.chain_id,
            B256::from(derived)
        ));
    }
    let expires_at = quantity(&line.expires_at)
        .ok_or_else(|| format!("\"expiresAt\" is not a hex quantity: {:?}", line.expires_at))?;
    if expires_at == 0 {
        return Err("\"expiresAt\" is 0: the entity would already be expired at block 0".into());
    }
    if line.attributes.len() > OP_MAX_ATTRIBUTES {
        return Err(format!(
            "{} attributes, at most {OP_MAX_ATTRIBUTES}",
            line.attributes.len()
        ));
    }
    let mut attributes = line
        .attributes
        .iter()
        .map(attribute)
        .collect::<Result<Vec<_>, _>>()?;
    // Ascending by name, as the decoder requires of a create.
    attributes.sort_by(|a, b| a.key.cmp(&b.key));
    if let Some(pair) = attributes.windows(2).find(|w| w[0].key == w[1].key) {
        let name = String::from_utf8_lossy(&pair[0].key);
        return Err(format!("{name:?} is given twice"));
    }
    let owner = part.owner.into_array();
    let create = Op::Create {
        key: derived,
        expires_at,
        creation_flags: CreationFlags::NONE,
        attributes,
    };
    Ok((line.nonce, owner, vec![create]))
}

/// One `{name, type, indexed, value}`, checked as a transaction's attribute is.
fn attribute(attribute: &LineAttribute) -> Result<Attribute, String> {
    let name = attribute.name.as_str();
    check_attribute_name(name)?;
    let value = value(name, &attribute.tag, &attribute.value)?;
    check_length(name, &value)?;
    if attribute.indexed && !value.attr_type().is_indexable() {
        return Err(format!(
            "{name:?}: a {} attribute cannot be indexed",
            attribute.tag
        ));
    }
    Ok(Attribute {
        key: name.as_bytes().to_vec(),
        value,
        indexed: attribute.indexed,
    })
}

/// A value in the node's RPC encoding, but for `bytes`, which is base64.
fn value(name: &str, tag: &str, value: &Value) -> Result<AttributeValue, String> {
    let bad = |what: &str| format!("{name:?} is not {what}: {value}");
    let text = |what: &str| value.as_str().ok_or_else(|| bad(what));
    Ok(match tag {
        "str" => AttributeValue::Str(text("a string")?.to_owned()),
        "i32" => AttributeValue::Int(
            value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| bad("an i32"))?,
        ),
        "u64" => AttributeValue::U64(quantity(text("a u64")?).ok_or_else(|| bad("a u64"))?),
        "bool" => AttributeValue::Bool(value.as_bool().ok_or_else(|| bad("a bool"))?),
        "addr" => AttributeValue::EthereumAddress(
            hex::<20>(text("an address")?).ok_or_else(|| bad("an address"))?,
        ),
        "key" => AttributeValue::EntityKey(
            hex::<32>(text("an entity key")?).ok_or_else(|| bad("an entity key"))?,
        ),
        "bytes32" => {
            AttributeValue::Bytes32(hex::<32>(text("32 bytes")?).ok_or_else(|| bad("32 bytes"))?)
        }
        "bytes" => AttributeValue::Bytes(
            STANDARD
                .decode(text("base64")?)
                .map_err(|e| format!("{name:?} is not base64: {e}"))?,
        ),
        _ => return Err(format!("{name:?}: unknown type {tag:?}")),
    })
}

/// A `0x` hex quantity, as the node's RPC writes one.
fn quantity(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("0x")?;
    if digits.is_empty() || digits.starts_with('+') {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// `0x` and exactly `N` bytes of hex.
fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let digits = text.strip_prefix("0x")?;
    let mut out = [0u8; N];
    alloy_primitives::hex::decode_to_slice(digits, &mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ALICE, CHAIN, bundle_key as key, bundle_line as line, write_bundle};
    use serde_json::json;

    fn owner() -> Address {
        ALICE.parse().unwrap()
    }

    fn part(report: Option<ReportPart>) -> Part {
        Part {
            path: "p.jsonl".into(),
            chain_id: CHAIN,
            owner: owner(),
            report,
        }
    }

    fn parse(value: &Value) -> Result<(u64, UserAddress, Vec<Op>), String> {
        parse_line(&part(None), &value.to_string())
    }

    #[test]
    fn a_line_becomes_a_create_by_the_owner() {
        let (nonce, creator, ops) = parse(&line(
            3,
            json!([
                {"name": "arkiv.schema", "type": "str", "indexed": true, "value": "sourcify.v4.contract"},
                {"name": "address", "type": "addr", "indexed": true, "value": "0x4f932770648a1f53c19342d38bc02dba06527246"},
                {"name": "blocknumber", "type": "u64", "indexed": true, "value": "0xdc1fbf"},
                {"name": "optimizerruns", "type": "i32", "indexed": true, "value": -200},
                {"name": "viair", "type": "bool", "indexed": true, "value": false},
                {"name": "hash", "type": "bytes32", "indexed": true, "value": format!("0x{}", "ab".repeat(32))},
                {"name": "ref", "type": "key", "indexed": false, "value": key(1)},
                {"name": "data", "type": "bytes", "indexed": false, "value": "aGk="},
            ]),
        ))
        .unwrap();
        assert_eq!(nonce, 3);
        assert_eq!(creator, owner().into_array());
        let expected = Op::Create {
            key: key(3).0,
            expires_at: u64::MAX,
            creation_flags: CreationFlags::NONE,
            attributes: vec![
                Attribute::new(
                    "address",
                    AttributeValue::EthereumAddress(
                        "0x4f932770648a1f53c19342d38bc02dba06527246"
                            .parse::<Address>()
                            .unwrap()
                            .into_array(),
                    ),
                ),
                Attribute::new(
                    "arkiv.schema",
                    AttributeValue::Str("sourcify.v4.contract".into()),
                ),
                Attribute::new("blocknumber", AttributeValue::U64(0xdc1fbf)),
                Attribute::unindexed("data", AttributeValue::Bytes(b"hi".to_vec())),
                Attribute::new("hash", AttributeValue::Bytes32([0xab; 32])),
                Attribute::new("optimizerruns", AttributeValue::Int(-200)),
                Attribute::unindexed("ref", AttributeValue::EntityKey(key(1).0)),
                Attribute::new("viair", AttributeValue::Bool(false)),
            ],
        };
        assert_eq!(ops, [expected]);
    }

    #[test]
    fn a_bad_line_says_what_is_wrong() {
        let attr = |name: &str, tag: &str, indexed: bool, value: Value| {
            line(
                0,
                json!([{"name": name, "type": tag, "indexed": indexed, "value": value}]),
            )
        };
        let with = |field: &str, value: Value| {
            let mut l = line(0, json!([]));
            l[field] = value;
            l
        };
        let mut extra = line(0, json!([]));
        extra["owner"] = json!(ALICE);
        let mut missing = line(0, json!([]));
        missing.as_object_mut().unwrap().remove("expiresAt");
        for (bad, expected) in [
            (with("key", json!(key(1))), "is not the key nonce 0 mints"),
            (with("nonce", json!(1)), "is not the key nonce 1 mints"),
            (
                with("expiresAt", json!("0")),
                "\"expiresAt\" is not a hex quantity",
            ),
            (with("expiresAt", json!(5)), "invalid type"),
            (with("expiresAt", json!("0x0")), "\"expiresAt\" is 0"),
            (extra, "unknown field `owner`"),
            (missing, "missing field `expiresAt`"),
            (
                attr("a", "f64", true, json!(1)),
                "\"a\": unknown type \"f64\"",
            ),
            (
                attr("a", "bytes", true, json!("aGk=")),
                "a bytes attribute cannot be indexed",
            ),
            (
                attr("a", "bytes", false, json!("!!")),
                "\"a\" is not base64",
            ),
            (attr("a", "u64", true, json!(16)), "\"a\" is not a u64"),
            (attr("a", "u64", true, json!("16")), "\"a\" is not a u64"),
            (
                attr("a", "i32", true, json!(1u64 << 33)),
                "\"a\" is not an i32",
            ),
            (
                attr("a", "addr", true, json!("0x01")),
                "\"a\" is not an address",
            ),
            (
                attr("a", "key", true, json!(ALICE)),
                "\"a\" is not an entity key",
            ),
            (
                attr("a", "str", true, json!("x".repeat(129))),
                "\"a\" is 129 bytes long",
            ),
            (
                attr("$owner", "str", true, json!("x")),
                "the engine owns names starting with $",
            ),
            (
                attr("9x", "str", true, json!("x")),
                "invalid attribute name",
            ),
            (
                line(
                    0,
                    json!([
                        {"name": "a", "type": "str", "indexed": true, "value": "x"},
                        {"name": "a", "type": "str", "indexed": true, "value": "y"},
                    ]),
                ),
                "\"a\" is given twice",
            ),
        ] {
            let error = parse(&bad).unwrap_err();
            assert!(error.contains(expected), "{bad}: {error}");
        }
    }

    #[test]
    fn nonces_run_in_order_from_where_the_report_says() {
        let lines = |nonces: &[u64]| -> Vec<String> {
            nonces
                .iter()
                .map(|&n| line(n, json!([])).to_string())
                .collect()
        };
        let read = |part: &Part, nonces: &[u64]| -> Result<Option<u64>, String> {
            let mut check = PartCheck::new(part);
            for text in lines(nonces) {
                check.line(part, &text)?;
            }
            Ok(check.nonce())
        };
        let report = |first_nonce| ReportPart {
            path: "p.jsonl".into(),
            bytes: 0,
            sha256: String::new(),
            entities: 0,
            first_nonce,
        };
        assert_eq!(read(&part(None), &[5, 6, 7]), Ok(Some(7)));
        assert_eq!(read(&part(Some(report(5))), &[5, 6]), Ok(Some(6)));
        let gap = read(&part(None), &[5, 7]).unwrap_err();
        assert!(gap.contains("nonce 7, but the line before has 5"), "{gap}");
        let late = read(&part(Some(report(4))), &[5]).unwrap_err();
        assert!(
            late.contains("report.json says the part starts at 4"),
            "{late}"
        );
    }

    #[test]
    fn a_part_must_be_the_one_the_report_describes() {
        let text = format!("{}\n", line(0, json!([])));
        let digest = alloy_primitives::hex::encode(Sha256::digest(text.as_bytes()));
        let report = |bytes, sha256: &str, entities| ReportPart {
            path: "p.jsonl".into(),
            bytes,
            sha256: sha256.to_owned(),
            entities,
            first_nonce: 0,
        };
        let finish = |report: ReportPart| {
            let part = part(Some(report));
            let mut check = PartCheck::new(&part);
            check.update(text.as_bytes());
            check.line(&part, text.trim_end())?;
            check.finish(&part, text.len() as u64)
        };
        let len = text.len() as u64;
        assert_eq!(finish(report(len, &digest, 1)), Ok(()));
        assert!(
            finish(report(len, &"0".repeat(64), 1))
                .unwrap_err()
                .contains("is damaged")
        );
        assert!(
            finish(report(len + 1, &digest, 1))
                .unwrap_err()
                .contains("is damaged")
        );
        assert!(
            finish(report(len, &digest, 2))
                .unwrap_err()
                .contains("holds 1 entities, but report.json says 2")
        );
    }

    #[test]
    fn a_bundle_opens_as_one_archive_per_part() {
        let dir = tempfile::tempdir().unwrap();
        let seed = write_bundle(
            dir.path(),
            &[
                (
                    "entities/000001.jsonl",
                    vec![line(0, json!([])), line(1, json!([]))],
                ),
                ("entities/000002.jsonl", vec![line(2, json!([]))]),
            ],
            true,
        );
        let bundle = open_bundle(&seed).unwrap();
        assert_eq!((bundle.chain_id, bundle.owner), (CHAIN, owner()));
        assert_eq!(bundle.entities, Some(3));
        let names: Vec<_> = bundle
            .archives
            .iter()
            .map(|a| a.name().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                PathBuf::from("entities/000001.jsonl"),
                PathBuf::from("entities/000002.jsonl")
            ]
        );
        for archive in &bundle.archives {
            assert_eq!(archive.index(), seed);
            assert_eq!(archive.parts().len(), 1);
        }
        // Each part's hash is seed.json's bytes, then the part's.
        let mut hasher = alloy_primitives::Keccak256::new();
        let seed_bytes = std::fs::read(&seed).unwrap();
        let part = std::fs::read(dir.path().join("entities/000002.jsonl")).unwrap();
        hasher.update((seed_bytes.len() as u64).to_be_bytes());
        hasher.update(&seed_bytes);
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(&part);
        assert_eq!(bundle.archives[1].hash().unwrap(), hasher.finalize());

        // Reading a part checks it and gives the owner's creates.
        let read: Vec<_> = bundle.archives[0]
            .entities()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(read.len(), 2);
        assert!(
            read.iter()
                .all(|(creator, _)| *creator == owner().into_array())
        );
    }

    #[test]
    fn a_bad_seed_or_report_is_refused() {
        let good = || {
            let dir = tempfile::tempdir().unwrap();
            let seed = write_bundle(
                dir.path(),
                &[("entities/000001.jsonl", vec![line(0, json!([]))])],
                true,
            );
            (dir, seed)
        };
        // `file` with the field at `pointer` set to `value`, or added.
        let edit = |file: &Path, pointer: &str, value: Value| {
            let mut json: Value =
                serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            match json.pointer_mut(pointer) {
                Some(field) => *field = value,
                None => json[&pointer[1..]] = value,
            }
            std::fs::write(file, json.to_string()).unwrap();
        };
        let zero = "0x0000000000000000000000000000000000000000";
        for (pointer, value, expected) in [
            ("/version", json!(1), "is not a Sourcify v4 bundle"),
            ("/format", json!("other"), "is not a Sourcify v4 bundle"),
            ("/firstNonce", json!(5), "firstNonce is 5"),
            ("/owner", json!(zero), "the owner is the zero address"),
            ("/entities", json!([]), "names no part"),
            (
                "/entities",
                json!([{"path": "../x.jsonl"}]),
                "inside the file's directory",
            ),
            ("/extra", json!(1), "unknown field"),
        ] {
            let (_dir, seed) = good();
            edit(&seed, pointer, value);
            let error = format!("{:#}", open_bundle(&seed).unwrap_err());
            assert!(error.contains(expected), "{expected}: {error}");
        }
        for (pointer, value, expected) in [
            (
                "/parts/0/path",
                json!("entities/x.jsonl"),
                "lists other parts than seed.json",
            ),
            (
                "/entities",
                json!(2),
                "its parts hold 1 entities, but it counts 2",
            ),
        ] {
            let (dir, seed) = good();
            edit(&dir.path().join("report.json"), pointer, value);
            let error = format!("{:#}", open_bundle(&seed).unwrap_err());
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    /// A part whose bytes are not the ones `report.json` describes is refused
    /// once read, with nothing handed on as read: the reader gives no hash.
    #[test]
    fn a_damaged_part_is_refused_at_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let seed = write_bundle(
            dir.path(),
            &[(
                "entities/000001.jsonl",
                vec![line(0, json!([])), line(1, json!([]))],
            )],
            true,
        );
        // The same lines, re-serialized with a space: same entities, other bytes.
        let part = dir.path().join("entities/000001.jsonl");
        let text = std::fs::read_to_string(&part)
            .unwrap()
            .replace("\"nonce\":", "\"nonce\": ");
        std::fs::write(&part, text).unwrap();
        let bundle = open_bundle(&seed).unwrap();
        let mut entities = bundle.archives[0].entities();
        assert!(entities.next().unwrap().is_ok());
        assert!(entities.next().unwrap().is_ok());
        let error = entities.next().unwrap().unwrap_err().to_string();
        assert!(
            error.contains("entities/000001.jsonl") && error.contains("is damaged"),
            "{error}"
        );
        assert!(entities.next().is_none());
        assert_eq!(entities.hash(), None);
    }

    /// Without `report.json` a bundle still opens; its parts are checked line
    /// by line only.
    #[test]
    fn a_bundle_without_a_report_opens() {
        let dir = tempfile::tempdir().unwrap();
        let seed = write_bundle(
            dir.path(),
            &[("entities/000001.jsonl", vec![line(0, json!([]))])],
            false,
        );
        let bundle = open_bundle(&seed).unwrap();
        assert_eq!(bundle.entities, None);
        assert_eq!(bundle.archives[0].entities().count(), 1);
    }
}
