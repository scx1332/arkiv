//! `arkiv-reth prefill`: write a chain's prefill into GolemDB before the
//! chain starts, then commit block 0 on top of it.
//!
//! ```text
//! arkiv-reth prefill --chain genesis.json --archives set.json              # the archives; resumable
//! arkiv-reth prefill --chain genesis.json --archives set.json --finalize   # block 0, and its root into genesis.json
//! arkiv-reth node    --chain genesis.json                                  # block 1 onwards
//! ```
//!
//! The input is an archive set (`--archives set.json`) or a Sourcify bundle
//! (`--seed seed.json`), whose parts are its archives: see
//! [`arkiv_prefill::bundle`]. A Sourcify bundle's entities are permanent, which
//! a prefill writes only with `--allow-permanent`.
//!
//! The store is GolemDB's directory, `--store` or `ARKIV_STORE_PATH` as for the
//! node, opened with the same settings from the genesis file, so the roots the
//! node later checks are the roots written here.
//!
//! reth's command line has a fixed set of subcommands, so this one is picked
//! out before reth parses the rest: `arkiv-reth prefill …` never reaches it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arkiv_prefill::{Archive, Prefill, finalize, open_bundle, open_set, run, set_prefilled_root};
use arkiv_reth_chainspec::{ArkivChainSpec, ArkivChainSpecParser};
use arkiv_reth_statemanager::{GenesisAccount, HostStore, StoreState, classify};
use clap::{ArgGroup, Parser};
use eyre::{Context, bail, ensure};
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum::chainspec::EthChainSpec;

use crate::genesis::NodeChainSpecParser;

/// Write a chain's prefill into GolemDB, then finalize block 0.
#[derive(Debug, Parser)]
#[command(name = "arkiv-reth prefill")]
#[command(group(ArgGroup::new("input").required(true).args(["archives", "seed"])))]
pub(crate) struct PrefillCommand {
    /// The chain's genesis file. `--finalize` writes block 0's state root into
    /// it, as `config.arkiv.prefilledBlockZeroStateRoot`.
    #[arg(long, value_name = "FILE")]
    chain: PathBuf,
    /// The archive set file: the archives' index files, in commit order.
    #[arg(long, value_name = "FILE")]
    archives: Option<PathBuf>,
    /// A Sourcify bundle's seed.json, instead of an archive set: each of its
    /// parts is one archive, committed in the order seed.json gives.
    #[arg(long, value_name = "FILE")]
    seed: Option<PathBuf>,
    /// Let entities be permanent (`$expiresAt` 2^64-1), as a Sourcify
    /// bundle's are. No transaction can create a permanent entity; without
    /// this flag, a prefill cannot either.
    #[arg(long)]
    allow_permanent: bool,
    /// Commit block 0 instead of archives. Only once every archive is in.
    #[arg(long)]
    finalize: bool,
    /// GolemDB's directory, as the node takes it.
    #[arg(long, env = "ARKIV_STORE_PATH", value_name = "DIR")]
    store: OsString,
}

/// Run `arkiv-reth prefill` with `args`, the words after `prefill`.
pub(crate) fn main(args: impl IntoIterator<Item = OsString>) -> eyre::Result<()> {
    let command = PrefillCommand::parse_from(
        std::iter::once(OsString::from("arkiv-reth prefill")).chain(args),
    );
    command.execute(&mut std::io::stdout())
}

/// What the prefill reads: its archives, and what to call them.
struct Input {
    archives: Vec<Archive>,
    /// What one archive is called in the output: an archive, or a bundle's part.
    noun: &'static str,
}

impl PrefillCommand {
    /// Run, reporting progress to `out`.
    fn execute(&self, out: &mut impl std::io::Write) -> eyre::Result<()> {
        let chain = self.chain.display();
        let text = std::fs::read_to_string(&self.chain)
            .wrap_err_with(|| format!("read the genesis file {chain}"))?;
        let spec = ArkivChainSpecParser::parse(&text)
            .wrap_err_with(|| format!("parse the genesis file {chain}"))?;
        let input = self.input(out, &spec)?;
        let store = crate::open_store(&spec, Some(self.store.clone()), None)?;
        if self.finalize {
            self.finalize(out, &text, &spec, &store, &input.archives)
        } else {
            self.run(out, &spec, &store, &input)
        }
    }

    /// Open the archive set or the bundle, and say what it holds.
    fn input(&self, out: &mut impl std::io::Write, spec: &ArkivChainSpec) -> eyre::Result<Input> {
        let chain_id = spec.chain().id();
        if let Some(set) = &self.archives {
            let archives = open_set(set)?;
            writeln!(
                out,
                "chain {chain_id}: {} in {}",
                plural(archives.len() as u64, "archive"),
                set.display()
            )?;
            return Ok(Input {
                archives,
                noun: "archive",
            });
        }
        let seed = self
            .seed
            .as_ref()
            .expect("clap requires --archives or --seed");
        let bundle = open_bundle(seed)?;
        let entities = match bundle.entities {
            Some(n) => format!("{} by report.json", plural(n, "entity")),
            None => "no report.json, so parts are checked line by line only".to_owned(),
        };
        writeln!(
            out,
            "chain {chain_id}: Sourcify bundle {}: {}, {entities}, created by {}",
            seed.display(),
            plural(bundle.archives.len() as u64, "part"),
            bundle.owner,
        )?;
        // The writer refuses every part too; this refuses before anything,
        // the alloc commit included, is written.
        ensure!(
            bundle.chain_id == chain_id,
            "{}: the bundle's keys are minted for chain {}, but the genesis file is chain \
             {chain_id}. Prefill a bundle exported (or re-keyed) for this chain",
            seed.display(),
            bundle.chain_id
        );
        Ok(Input {
            archives: bundle.archives,
            noun: "part",
        })
    }

    fn run(
        &self,
        out: &mut impl std::io::Write,
        spec: &ArkivChainSpec,
        store: &HostStore,
        input: &Input,
    ) -> eyre::Result<()> {
        let archives = &input.archives[..];
        let noun = input.noun;
        let total = archives.len();
        match classify(store, [0; 32])? {
            StoreState::GolemDbInitialized => {
                writeln!(
                    out,
                    "store: empty; writing the genesis accounts, then the {noun}s"
                )?;
            }
            StoreState::ArkivPrefilling { done, total } => writeln!(
                out,
                "store: {done} of {total} {noun}s in; checking nothing changed, then resuming"
            )?,
            StoreState::ArkivPrefillDone => {
                writeln!(out, "store: every {noun} is in; checking nothing changed")?;
            }
            // `run` refuses these, saying why.
            _ => {}
        }
        let alloc = alloc(spec);
        let prefill = self.prefill(spec, &alloc, archives);
        let mut progress = Progress::new(archives);
        let mut since = Instant::now();
        // Each archive is reported as it lands: a large prefill runs for hours.
        let mut failed_write = None;
        let ran = run(store, &prefill, |place, written| {
            let line = format!(
                "{}  committed as commit {} · {} · {:.1} s · {}\n  {}",
                archives[place].name().display(),
                written.commit.0,
                plural(written.entities, "entity"),
                since.elapsed().as_secs_f64(),
                written.hash,
                progress.after(place, written.entities, noun),
            );
            if let Err(error) = writeln!(out, "{line}") {
                failed_write.get_or_insert(error);
            }
            since = Instant::now();
        });
        ran?;
        if let Some(error) = failed_write {
            return Err(error.into());
        }
        writeln!(
            out,
            "{total} of {total} {noun}s in. Run again with --finalize to commit block 0."
        )?;
        Ok(())
    }

    fn finalize(
        &self,
        out: &mut impl std::io::Write,
        text: &str,
        spec: &ArkivChainSpec,
        store: &HostStore,
        archives: &[Archive],
    ) -> eyre::Result<()> {
        // Block 0 is what the store holds, so the archives given must be the
        // ones it was prefilled from: the same checks as a resume (genesis
        // file, archive count, last archive's hash). On a finished prefill
        // `run` only checks; any other state `finalize` refuses below.
        if matches!(classify(store, [0; 32])?, StoreState::ArkivPrefillDone) {
            let alloc = alloc(spec);
            run(store, &self.prefill(spec, &alloc, archives), |_, _| {})?;
        }
        let published = spec.prefilled_block_zero_state_root();
        let mut with_root = None;
        let finalized = finalize(store, spec.prefill_genesis_hash().0, published, |root| {
            let text = set_prefilled_root(text, root)?;
            // Block 0's hash exactly as the node computes it from the file.
            let hash = NodeChainSpecParser::parse(&text)?.genesis_hash();
            with_root = Some(text);
            Ok(hash)
        })?;
        writeln!(
            out,
            "block 0 committed as commit {}: state root {}, hash {}",
            finalized.commit.0, finalized.root, finalized.block_zero
        )?;
        let chain = self.chain.display();
        if published.is_some() {
            writeln!(out, "it matches {chain}'s prefilledBlockZeroStateRoot")?;
        } else {
            let text = with_root.expect("finalize asks for block 0's hash before committing");
            if let Err(error) = replace(&self.chain, &text) {
                bail!(
                    "block 0 is committed, but writing {chain} failed: {error:#}. Add \
                     \"prefilledBlockZeroStateRoot\": \"{}\" under config.arkiv by hand",
                    finalized.root
                );
            }
            writeln!(
                out,
                "wrote config.arkiv.prefilledBlockZeroStateRoot = {} into {chain}",
                finalized.root
            )?;
        }
        writeln!(out, "Start the chain with: arkiv-reth node --chain {chain}")?;
        Ok(())
    }

    fn prefill<'a>(
        &self,
        spec: &ArkivChainSpec,
        alloc: &'a [GenesisAccount],
        archives: &'a [Archive],
    ) -> Prefill<'a> {
        Prefill {
            chain_id: spec.chain().id(),
            genesis_hash: spec.prefill_genesis_hash().0,
            alloc,
            archives,
            allow_permanent: self.allow_permanent,
        }
    }
}

fn alloc(spec: &ArkivChainSpec) -> Vec<GenesisAccount> {
    GenesisAccount::from_alloc(&spec.genesis().alloc).collect()
}

/// `n` and `noun`, plural unless `n` is 1: "1 entity", "3 entities".
fn plural(n: u64, noun: &str) -> String {
    match (n, noun.strip_suffix('y')) {
        (1, _) => format!("1 {noun}"),
        (_, Some(stem)) => format!("{n} {stem}ies"),
        (_, None) => format!("{n} {noun}s"),
    }
}

/// How far a run is, for the line after each archive: what it has written,
/// how fast, and how long the rest should take at that pace.
struct Progress {
    /// Each archive's size: the bytes of its parts.
    sizes: Vec<u64>,
    started: Instant,
    /// Written by this run, which may have resumed after other archives.
    bytes: u64,
    entities: u64,
}

impl Progress {
    fn new(archives: &[Archive]) -> Self {
        let size = |archive: &Archive| {
            archive
                .parts()
                .iter()
                .filter_map(|part| std::fs::metadata(part).ok())
                .map(|meta| meta.len())
                .sum()
        };
        Self {
            sizes: archives.iter().map(size).collect(),
            started: Instant::now(),
            bytes: 0,
            entities: 0,
        }
    }

    /// The line after archive `place` landed with `entities` entities.
    fn after(&mut self, place: usize, entities: u64, noun: &str) -> String {
        self.bytes += self.sizes[place];
        self.entities += entities;
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = self.bytes as f64 / elapsed.max(1e-3);
        let left: u64 = self.sizes[place + 1..].iter().sum();
        let mut line = format!(
            "{} of {} {noun}s · this run: {} in {}, {:.1} MB/s, {:.0} entities/s · ETA {}",
            place + 1,
            self.sizes.len(),
            plural(self.entities, "entity"),
            duration(elapsed),
            rate / 1e6,
            self.entities as f64 / elapsed.max(1e-3),
            duration(left as f64 / rate.max(1.0)),
        );
        if let Some(memory) = Memory::now() {
            line.push_str(&format!(
                " · memory {} MiB, mapped files {} MiB",
                memory.anonymous >> 20,
                memory.mapped >> 20
            ));
        }
        line
    }
}

/// `seconds` as `1h 02m`, `3m 04s` or `5.0 s`.
fn duration(seconds: f64) -> String {
    let whole = seconds as u64;
    match whole {
        0..60 => format!("{seconds:.1} s"),
        60..3600 => format!("{}m {:02}s", whole / 60, whole % 60),
        _ => format!("{}h {:02}m", whole / 3600, whole % 3600 / 60),
    }
}

/// The process's resident memory, where the OS says (Linux), in two parts
/// that grow differently: its own memory, which holds one archive's writes at
/// a time, and the pages of its memory-mapped files, mostly GolemDB's store,
/// which grow with the store and which the kernel takes back when it needs
/// them, as it does any file cache.
struct Memory {
    anonymous: u64,
    mapped: u64,
}

impl Memory {
    fn now() -> Option<Self> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let bytes = |field: &str| -> Option<u64> {
            let kib = status
                .lines()
                .find_map(|line| line.strip_prefix(field))?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()?;
            Some(kib << 10)
        };
        Some(Self {
            anonymous: bytes("RssAnon:")?,
            mapped: bytes("RssFile:")? + bytes("RssShmem:").unwrap_or(0),
        })
    }
}

/// Replace `path`'s contents with `text` in one step: written beside it, then
/// renamed over it, so a crash leaves the old file or the new one, never half
/// of one.
fn replace(path: &Path, text: &str) -> eyre::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".prefill-tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text).wrap_err_with(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .wrap_err_with(|| format!("rename onto {}", path.display()))?;
    ensure!(
        std::fs::read_to_string(path)? == text,
        "{} did not take",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_reth_statemanager::{check_genesis_root, prepare_genesis};

    const ALICE: &str = "0x1111111111111111111111111111111111111111";

    /// A prefill directory: the example genesis file, two archives of two
    /// entities each and their set file, and a store directory.
    struct Setup {
        dir: tempfile::TempDir,
    }

    impl Setup {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("genesis.json"),
                include_str!("../../../genesis/example.json"),
            )
            .unwrap();
            for n in 0..2u8 {
                let at = dir.path().join(format!("A{n}"));
                std::fs::create_dir(&at).unwrap();
                let lines: Vec<String> = (1..=2u8)
                    .map(|k| {
                        format!(
                            r#"{{"$key":"0x{}","$creator":"{ALICE}","$owner":"{ALICE}","$expiresAt":5000,"kind":"str:k{k}"}}"#,
                            format!("{:02x}", 10 * n + k).repeat(32)
                        )
                    })
                    .collect();
                std::fs::write(at.join("p.jsonl"), lines.join("\n") + "\n").unwrap();
                std::fs::write(
                    at.join("index.json"),
                    r#"{"version":1,"parts":["p.jsonl"]}"#,
                )
                .unwrap();
            }
            std::fs::write(
                dir.path().join("set.json"),
                r#"{"version":1,"archives":["A0/index.json","A1/index.json"]}"#,
            )
            .unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn command(&self, store: &str, finalize: bool) -> PrefillCommand {
            PrefillCommand {
                chain: self.path("genesis.json"),
                archives: Some(self.path("set.json")),
                seed: None,
                allow_permanent: false,
                finalize,
                store: self.path(store).into_os_string(),
            }
        }

        /// Run the command; what it printed, or its error.
        fn run(&self, store: &str, finalize: bool) -> Result<String, String> {
            let mut out = Vec::new();
            let result = self.command(store, finalize).execute(&mut out);
            let printed = String::from_utf8(out).unwrap();
            result.map(|()| printed).map_err(|e| format!("{e:#}"))
        }

        fn genesis(&self) -> String {
            std::fs::read_to_string(self.path("genesis.json")).unwrap()
        }
    }

    /// Prefill, finalize, and the node accepts the store under the genesis
    /// file the finalize wrote: block 0's hash, as the node computes it from
    /// that file, tags the store's block 0, whose root the file carries.
    #[test]
    fn prefill_then_finalize_gives_a_store_the_node_starts_on() {
        let setup = Setup::new();
        let printed = setup.run("golemdb", false).unwrap();
        assert!(printed.contains("store: empty"), "{printed}");
        assert!(
            printed.contains("A1/index.json  committed as commit 3 · 2 entities"),
            "{printed}"
        );
        assert!(
            printed.ends_with("2 of 2 archives in. Run again with --finalize to commit block 0.\n")
        );

        let printed = setup.run("golemdb", true).unwrap();
        assert!(
            printed.contains("block 0 committed as commit 4"),
            "{printed}"
        );
        assert!(
            printed.contains("wrote config.arkiv.prefilledBlockZeroStateRoot"),
            "{printed}"
        );

        let genesis = setup.genesis();
        let node = NodeChainSpecParser::parse(&genesis).unwrap();
        let root = node
            .prefilled_block_zero_state_root()
            .expect("the field was written");
        assert_eq!(node.genesis_header().state_root, root);
        let store =
            crate::open_store(&node, Some(setup.path("golemdb").into_os_string()), None).unwrap();
        let block_zero = node.genesis_hash().0;
        assert_eq!(
            prepare_genesis(&store, block_zero, Some(node.prefill_genesis_hash().0), []).unwrap(),
            None
        );
        check_genesis_root(&store, block_zero, root.0).unwrap();
    }

    /// Run again after it finished, the prefill writes nothing more; finalized,
    /// it is refused.
    #[test]
    fn a_second_run_writes_nothing_and_a_second_finalize_is_refused() {
        let setup = Setup::new();
        setup.run("golemdb", false).unwrap();
        let printed = setup.run("golemdb", false).unwrap();
        assert!(printed.contains("store: every archive is in"), "{printed}");
        assert!(!printed.contains("committed as commit"), "{printed}");
        setup.run("golemdb", true).unwrap();
        let error = setup.run("golemdb", true).unwrap_err();
        assert!(error.contains("already finalized"), "{error}");
    }

    #[test]
    fn finalize_before_the_archives_is_refused() {
        let setup = Setup::new();
        let error = setup.run("golemdb", true).unwrap_err();
        assert_eq!(
            error,
            "the store is empty: run the prefill before finalizing"
        );
        assert!(!setup.genesis().contains("prefilledBlockZeroStateRoot"));
    }

    /// `--finalize` with archives other than the ones the store was prefilled
    /// from is refused, and nothing is committed or written.
    #[test]
    fn finalize_with_another_archive_set_is_refused() {
        let setup = Setup::new();
        setup.run("golemdb", false).unwrap();
        // The last archive edited: one entity's attribute changed.
        let part = setup.path("A1/p.jsonl");
        let edited = std::fs::read_to_string(&part)
            .unwrap()
            .replace("str:k1", "str:other");
        std::fs::write(&part, edited).unwrap();
        let error = setup.run("golemdb", true).unwrap_err();
        assert!(
            error.contains("the list changed since this prefill started"),
            "{error}"
        );
        // Fewer archives.
        std::fs::write(
            setup.path("set.json"),
            r#"{"version":1,"archives":["A0/index.json"]}"#,
        )
        .unwrap();
        let error = setup.run("golemdb", true).unwrap_err();
        assert!(
            error.contains("started with 2 archives; 1 were given"),
            "{error}"
        );
        assert!(!setup.genesis().contains("prefilledBlockZeroStateRoot"));
    }

    /// Anyone can re-run the published archives against the published genesis
    /// file: same root, and the file is left as it is.
    #[test]
    fn an_audit_run_matches_the_published_root() {
        let setup = Setup::new();
        setup.run("golemdb", false).unwrap();
        setup.run("golemdb", true).unwrap();
        let published = setup.genesis();

        setup.run("audit", false).unwrap();
        let printed = setup.run("audit", true).unwrap();
        assert!(printed.contains("it matches"), "{printed}");
        assert_eq!(setup.genesis(), published, "the file is not rewritten");
    }

    /// The committed Sourcify fixture, copied into a new directory: the prefill
    /// writes its store there, and `--finalize` its genesis file.
    fn sourcify() -> tempfile::TempDir {
        fn copy(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy(&entry.path(), &target);
                } else {
                    std::fs::copy(entry.path(), target).unwrap();
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/arkiv-prefill/fixtures/sourcify"),
            dir.path(),
        );
        dir
    }

    /// `arkiv-reth prefill --seed` on the bundle in `dir`; what it printed, or
    /// its error.
    fn seed(
        dir: &Path,
        store: &str,
        allow_permanent: bool,
        finalize: bool,
    ) -> Result<String, String> {
        let command = PrefillCommand {
            chain: dir.join("genesis.json"),
            archives: None,
            seed: Some(dir.join("seed.json")),
            allow_permanent,
            finalize,
            store: dir.join(store).into_os_string(),
        };
        let mut out = Vec::new();
        let result = command.execute(&mut out);
        let printed = String::from_utf8(out).unwrap();
        result.map(|()| printed).map_err(|e| format!("{e:#}"))
    }

    /// The fixture's block 0 root and hash, from its `expected.json`.
    fn sourcify_expected() -> (alloy_primitives::B256, alloy_primitives::B256) {
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/arkiv-prefill/fixtures/sourcify/expected.json"
        ))
        .unwrap();
        let field = |name: &str| expected[name].as_str().unwrap().parse().unwrap();
        (field("root"), field("blockZeroHash"))
    }

    /// A Sourcify bundle prefills part by part, its permanent entities only
    /// with `--allow-permanent`, and resumes after a run without it. Finalized,
    /// it is the fixture's block 0, and the node accepts the store under the
    /// genesis file the finalize wrote.
    #[test]
    fn a_sourcify_bundle_prefills_and_the_node_starts_on_it() {
        let work = sourcify();
        let dir = work.path();

        // Without --allow-permanent: refused at the first line, after the
        // alloc commit; the store waits to resume.
        let error = seed(dir, "golemdb", false, false).unwrap_err();
        assert!(
            error.contains("entities/000001.jsonl:1: refused: a permanent expiry, which a prefill writes only when it is allowed to (--allow-permanent)"),
            "{error}"
        );

        let printed = seed(dir, "golemdb", true, false).unwrap();
        assert!(
            printed.starts_with("chain 7706815: Sourcify bundle ")
                && printed.contains("2 parts, 9 entities by report.json, created by 0xf39F"),
            "{printed}"
        );
        assert!(
            printed.contains("store: 0 of 2 parts in; checking nothing changed, then resuming"),
            "{printed}"
        );
        assert!(
            printed.contains("entities/000001.jsonl  committed as commit 2 · 6 entities")
                && printed.contains("entities/000002.jsonl  committed as commit 3 · 3 entities")
                && printed.contains("2 of 2 parts · this run: 9 entities in"),
            "{printed}"
        );
        assert!(
            printed.ends_with("2 of 2 parts in. Run again with --finalize to commit block 0.\n"),
            "{printed}"
        );

        let printed = seed(dir, "golemdb", false, true).unwrap();
        let (root, hash) = sourcify_expected();
        assert!(
            printed.contains(&format!(
                "block 0 committed as commit 4: state root {root}, hash {hash}"
            )),
            "{printed}"
        );

        let genesis = std::fs::read_to_string(dir.join("genesis.json")).unwrap();
        let node = NodeChainSpecParser::parse(&genesis).unwrap();
        assert_eq!(node.prefilled_block_zero_state_root(), Some(root));
        assert_eq!(node.genesis_hash(), hash);
        let store =
            crate::open_store(&node, Some(dir.join("golemdb").into_os_string()), None).unwrap();
        assert_eq!(
            prepare_genesis(&store, hash.0, Some(node.prefill_genesis_hash().0), []).unwrap(),
            None
        );
        check_genesis_root(&store, hash.0, root.0).unwrap();

        // Seeding the same bundle again, into another store, gives the same
        // block 0: an audit of the published root.
        seed(dir, "audit", true, false).unwrap();
        let printed = seed(dir, "audit", true, true).unwrap();
        assert!(
            printed.contains(&format!("state root {root}, hash {hash}"))
                && printed.contains("it matches"),
            "{printed}"
        );
    }

    /// A bundle minted for another chain is refused before anything is
    /// written, the alloc commit included.
    #[test]
    fn a_bundle_for_another_chain_writes_nothing() {
        let work = sourcify();
        let dir = work.path();
        let genesis = std::fs::read_to_string(dir.join("genesis.json"))
            .unwrap()
            .replace("\"chainId\": 7706815", "\"chainId\": 7706816");
        std::fs::write(dir.join("genesis.json"), genesis).unwrap();
        let error = seed(dir, "golemdb", true, false).unwrap_err();
        assert!(
            error.contains(
                "the bundle's keys are minted for chain 7706815, but the genesis file is chain 7706816"
            ),
            "{error}"
        );
        assert!(!dir.join("golemdb").exists(), "no store was opened");
    }

    /// The genesis file is replaced whole: nothing is left beside it.
    #[test]
    fn the_genesis_file_is_replaced_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("genesis.json");
        std::fs::write(&file, "old").unwrap();
        replace(&file, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
