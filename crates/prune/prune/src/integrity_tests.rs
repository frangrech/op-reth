//! Data-integrity tests for the static-file pruning segments (`Headers`, `Transactions`,
//! `Receipts`, `Sidecars`).
//!
//! In engine mode every persisted block is written to static files *and* to MDBX
//! (`UnifiedStorageWriter::save_blocks` + `DatabaseProvider::insert_block`). The static-file
//! pruning segments exist to delete the redundant MDBX copy once the data is safely in static
//! files. These tests pin down the safety properties of that deletion:
//!
//! 1. Nothing is ever deleted from MDBX above the highest static-file block of its own segment.
//! 2. After pruning, every block / transaction / receipt / header / sidecar is still readable
//!    through the high level providers and is identical to the data that was originally written
//!    (no gaps, no corruption), no matter where it is read from.
//! 3. Pruning in many small limited runs (what the periodic pruner does once `delete_limit` is
//!    non-zero) ends in exactly the same state as one unbounded run (what a startup catch-up
//!    does).
//! 4. Tables that are not part of the static-file segments (hash->number index, body indices,
//!    tx->block index, ...) are never touched.
//! 5. A `delete_limit` of 0 deletes nothing at all (this is why the opBNB chainspec must not use 0).

use crate::{PruneProgress, PrunerBuilder};
use alloy_primitives::{B256, U256};
use reth_db::tables;
use reth_db_api::table::Table;
use reth_provider::{
    BlockHashReader, BlockReader, DatabaseProviderFactory, HeaderProvider, ReceiptProvider,
    SidecarsProvider, StaticFileProviderFactory, TransactionsProvider,
};
use reth_stages::test_utils::{StorageKind, TestStageDB};
use reth_static_file_types::StaticFileSegment;
use reth_testing_utils::generators::{self, random_block_range, random_receipt, BlockRangeParams};

const MAX_RUNS: usize = 100_000;

/// Debug snapshot of every table the pruner could possibly touch, plus the ones it must not touch.
fn raw_tables(db: &TestStageDB) -> Vec<(&'static str, String)> {
    macro_rules! t {
        ($name:literal, $table:ty) => {
            ($name, format!("{:?}", db.table::<$table>().unwrap()))
        };
    }
    vec![
        t!("Headers", tables::Headers),
        t!("CanonicalHeaders", tables::CanonicalHeaders),
        t!("HeaderTerminalDifficulties", tables::HeaderTerminalDifficulties),
        t!("Transactions", tables::Transactions),
        t!("Receipts", tables::Receipts),
        t!("Sidecars", tables::Sidecars),
        // never pruned by the static file segments:
        t!("HeaderNumbers", tables::HeaderNumbers),
        t!("BlockBodyIndices", tables::BlockBodyIndices),
        t!("TransactionBlocks", tables::TransactionBlocks),
    ]
}

fn keys<T: Table<Key = u64>>(db: &TestStageDB) -> Vec<u64>
where
    T::Value: std::fmt::Debug,
{
    db.table::<T>().unwrap().into_iter().map(|(k, _)| k).collect()
}

/// Runs the pruner (built exactly like the node builds it: `PrunerBuilder`, `block_interval` 5)
/// repeatedly at `tip` until a run deletes nothing. Returns the number of runs that deleted
/// something.
fn prune_to_fixed_point(db: &TestStageDB, tip: u64, delete_limit: usize, timeout: bool) -> usize {
    let mut builder = PrunerBuilder::default().block_interval(5).delete_limit(delete_limit);
    if timeout {
        builder = builder.timeout(PrunerBuilder::DEFAULT_TIMEOUT);
    }
    let mut pruner = builder.build_with_provider_factory(db.factory.clone());

    let mut productive_runs = 0;
    for _ in 0..MAX_RUNS {
        let output = pruner.run(tip).expect("pruner run");
        let pruned: usize = output.segments.iter().map(|(_, s)| s.pruned).sum();
        if pruned == 0 {
            return productive_runs
        }
        productive_runs += 1;
    }
    panic!("pruner did not reach a fixed point in {MAX_RUNS} runs");
}

/// Builds the "engine mode" situation: all `n + 1` blocks in MDBX, and blocks `0..=h` also in
/// static files (headers, transactions, sidecars, receipts).
macro_rules! scenario_inputs {
    ($n:expr, $h:expr => $blocks:ident, $receipts:ident, $last_tx:ident, $total_txs:ident, $build:ident) => {
        let n: u64 = $n;
        let h: u64 = $h;
        let mut rng = generators::rng();
        let $blocks = random_block_range(
            &mut rng,
            0..=n,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 1..4, ..Default::default() },
        );
        assert_eq!($blocks.len() as u64, n + 1);

        let mut $receipts = Vec::new();
        let mut $last_tx = Vec::new();
        let mut next_tx = 0u64;
        for block in &$blocks {
            let mut block_receipts = Vec::new();
            for transaction in &block.body.transactions {
                block_receipts.push((next_tx, random_receipt(&mut rng, transaction, Some(0))));
                next_tx += 1;
            }
            $receipts.push((block.number, block_receipts));
            $last_tx.push(next_tx - 1);
        }
        let $total_txs = next_tx;

        let $build = || {
            let db = TestStageDB::default();
            // MDBX copy of everything (what `insert_block` writes).
            db.insert_blocks($blocks.iter(), StorageKind::Database(None)).unwrap();
            db.insert_receipts_by_block($receipts.clone(), StorageKind::Database(None)).unwrap();
            // static file copy of blocks 0..=h (what `save_header_and_transactions` writes).
            db.insert_blocks($blocks[..=h as usize].iter(), StorageKind::Static).unwrap();
            db.insert_receipts_by_block($receipts[..=h as usize].to_vec(), StorageKind::Static)
                .unwrap();
            db
        };
    };
}

/// Reads every block back through the high level providers (static file first, MDBX fallback)
/// and compares against the data that was originally generated.
macro_rules! assert_chain_intact {
    ($db:expr, $blocks:expr, $receipts:expr, $ctx:expr) => {{
        let provider = $db.factory.provider().unwrap();
        for (i, block) in $blocks.iter().enumerate() {
            let num = block.number;
            assert_eq!(
                provider.header_by_number(num).unwrap().as_ref(),
                Some(block.header.header()),
                "{}: header {num} missing or different",
                $ctx
            );
            assert_eq!(
                provider.block_hash(num).unwrap(),
                Some(block.header.hash()),
                "{}: canonical hash {num} missing or different",
                $ctx
            );
            assert_eq!(
                provider.header_td_by_number(num).unwrap(),
                Some(U256::ZERO),
                "{}: td {num} missing or different",
                $ctx
            );
            assert!(
                provider.sidecars_by_number(num).unwrap().is_some(),
                "{}: sidecars {num} missing",
                $ctx
            );
            let indices = provider
                .block_body_indices(num)
                .unwrap()
                .unwrap_or_else(|| panic!("{}: body indices {num} missing", $ctx));
            assert_eq!(indices.tx_count as usize, block.body.transactions.len());
            for (j, tx) in block.body.transactions.iter().enumerate() {
                let id = indices.first_tx_num + j as u64;
                assert_eq!(
                    provider.transaction_by_id(id).unwrap().map(|t| t.hash()),
                    Some(tx.hash()),
                    "{}: tx {id} (block {num}) missing or different",
                    $ctx
                );
                assert_eq!(
                    provider.receipt(id).unwrap().as_ref(),
                    Some(&$receipts[i].1[j].1),
                    "{}: receipt {id} (block {num}) missing or different",
                    $ctx
                );
            }
        }
    }};
}

/// One full scenario: `n + 1` blocks in MDBX, static files cover `0..=h`, the pruner runs with
/// the persisted tip at `tip`.
fn run_scenario(n: u64, h: u64, tip: u64, limit: usize, timeout: bool) {
    scenario_inputs!(n, h => blocks, receipts, last_tx, total_txs, build);
    let ctx = format!("n={n} h={h} tip={tip} limit={limit} timeout={timeout}");

    // ---- reference: one unbounded run (what a startup catch-up does) ----
    // (scoped so the reference database is dropped before the next one is created)
    let (before, reference_after) = {
        let reference = build();
        let before = raw_tables(&reference);
        assert_chain_intact!(reference, blocks, receipts, format!("{ctx} [before prune]"));
        prune_to_fixed_point(&reference, tip, usize::MAX, false);
        (before, raw_tables(&reference))
    };

    // ---- subject: periodic limited runs ----
    let subject = build();
    // fixture sanity: static files hold exactly 0..=h, MDBX holds everything
    let sf = subject.factory.static_file_provider();
    for segment in [
        StaticFileSegment::Headers,
        StaticFileSegment::Transactions,
        StaticFileSegment::Receipts,
        StaticFileSegment::Sidecars,
    ] {
        assert_eq!(sf.get_highest_static_file_block(segment), Some(h), "{ctx}: {segment:?} head");
    }
    assert_eq!(sf.get_highest_static_file_tx(StaticFileSegment::Transactions), Some(last_tx[h as usize]));
    assert_eq!(keys::<tables::Headers>(&subject), (0..=n).collect::<Vec<_>>());
    assert_eq!(keys::<tables::Transactions>(&subject), (0..total_txs).collect::<Vec<_>>());

    prune_to_fixed_point(&subject, tip, limit, timeout);
    let after = raw_tables(&subject);

    // (3) many small runs == one unbounded run
    assert_eq!(after, reference_after, "{ctx}: incremental pruning differs from one unbounded run");

    // (2) nothing lost or changed, read through the normal providers
    assert_chain_intact!(subject, blocks, receipts, format!("{ctx} [after prune]"));

    // static files themselves are never modified by the pruner
    for segment in [
        StaticFileSegment::Headers,
        StaticFileSegment::Transactions,
        StaticFileSegment::Receipts,
        StaticFileSegment::Sidecars,
    ] {
        assert_eq!(sf.get_highest_static_file_block(segment), Some(h), "{ctx}: {segment:?} head moved");
    }

    // (1) exactly the rows covered by static files are gone, nothing above
    if h > tip {
        // static files are ahead of the pruner's tip: nothing may be pruned
        assert_eq!(after, before, "{ctx}: pruned although static head is above the tip");
        return
    }
    let above_blocks: Vec<u64> = (h + 1..=n).collect();
    let first_kept_tx = last_tx[h as usize] + 1;
    let above_txs: Vec<u64> = (first_kept_tx..total_txs).collect();
    assert_eq!(keys::<tables::Headers>(&subject), above_blocks, "{ctx}: Headers");
    assert_eq!(keys::<tables::CanonicalHeaders>(&subject), above_blocks, "{ctx}: CanonicalHeaders");
    assert_eq!(
        keys::<tables::HeaderTerminalDifficulties>(&subject),
        above_blocks,
        "{ctx}: HeaderTerminalDifficulties"
    );
    assert_eq!(keys::<tables::Sidecars>(&subject), above_blocks, "{ctx}: Sidecars");
    assert_eq!(keys::<tables::Transactions>(&subject), above_txs, "{ctx}: Transactions");
    assert_eq!(keys::<tables::Receipts>(&subject), above_txs, "{ctx}: Receipts");

    // (4) tables that are not static-file segments are byte-for-byte untouched
    for ((name, b), (_, a)) in before.iter().zip(after.iter()) {
        if matches!(*name, "HeaderNumbers" | "BlockBodyIndices" | "TransactionBlocks") {
            assert_eq!(a, b, "{ctx}: {name} must not be touched by the pruner");
        }
    }
}

#[test]
fn prod_limit_static_head_equals_tip() {
    // what the node does: static head == persisted tip (every block goes to static files at once)
    run_scenario(40, 40, 40, 10_000, true);
}

#[test]
fn prod_limit_static_head_behind_tip() {
    run_scenario(40, 25, 40, 10_000, true);
    run_scenario(40, 25, 31, 10_000, true);
    run_scenario(40, 0, 40, 10_000, true);
}

#[test]
fn small_limits_many_runs_static_head_equals_tip() {
    for limit in [6, 7, 9, 13, 100] {
        run_scenario(40, 40, 40, limit, true);
        run_scenario(40, 40, 40, limit, false);
    }
}

#[test]
fn small_limits_many_runs_static_head_behind_tip() {
    for limit in [6, 7, 13] {
        run_scenario(40, 17, 40, limit, true);
    }
}

#[test]
fn static_head_above_tip_prunes_nothing() {
    run_scenario(40, 30, 20, 10_000, true);
    run_scenario(40, 30, 29, 10_000, false);
}

/// `delete_limit == 0` (the old opBNB chainspec value) makes the pruner stop before the first
/// segment: periodic runs delete nothing and the MDBX duplicates pile up.
#[test]
fn zero_limit_prunes_nothing() {
    scenario_inputs!(40, 40 => blocks, receipts, _last_tx, _total, build);
    let db = build();
    let before = raw_tables(&db);
    let runs = prune_to_fixed_point(&db, 40, 0, true);
    assert_eq!(runs, 0);
    assert_eq!(raw_tables(&db), before);
    assert_chain_intact!(db, blocks, receipts, "limit 0");
}
