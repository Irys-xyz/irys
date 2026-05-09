use crate::tables::IrysTables;
use metrics::{Label, gauge};
use reth_db::DatabaseEnv;
use reth_db_api::Database as _;
use tracing::warn;

/// Emits per-table size, page-count, entry-count, freelist, page-size, and
/// timed-out-reader gauges for an Irys consensus `DatabaseEnv`.
///
/// Mirrors `<DatabaseEnv as DatabaseMetrics>::gauge_metrics` (in
/// `reth-db/src/implementation/mdbx/mod.rs`) but iterates `IrysTables::ALL`.
/// Reth's built-in impl iterates `reth_db::Tables::ALL` — which doesn't match
/// the Irys schema, so the consensus DB has no other path to these gauges.
///
/// Intended for a periodic hook driven from chain init; the consensus DB has
/// no equivalent of Reth's `metrics_hooks()` provider-factory hook.
///
/// All gauges are tagged `scope="irys-consensus"` to disambiguate from Reth's
/// EVM DB gauges, which share the same metric names.
pub fn report_irys_consensus_db_gauges(db: &DatabaseEnv) {
    let view_outcome = db.view(|tx| -> eyre::Result<()> {
        for table in IrysTables::ALL {
            let name = table.name();
            let table_db = tx
                .inner()
                .open_db(Some(name))
                .map_err(|e| eyre::eyre!("open table {name}: {e}"))?;
            let stats = tx
                .inner()
                .db_stat(table_db.dbi())
                .map_err(|e| eyre::eyre!("stat table {name}: {e}"))?;

            let page_size_bytes = usize::try_from(stats.page_size()).unwrap_or(usize::MAX);
            let leaf = stats.leaf_pages();
            let branch = stats.branch_pages();
            let overflow = stats.overflow_pages();
            let total_pages = leaf.saturating_add(branch).saturating_add(overflow);
            let bytes = page_size_bytes.saturating_mul(total_pages);

            let table_label = Label::new("table", name);
            let scope_label = Label::new("scope", "irys-consensus");

            // MDBX page/entry counts always fit in f64's 53-bit mantissa
            // (>9 PB at 1KB pages), so the cast is lossless in practice and
            // matches reth's upstream gauge convention.
            gauge!(
                "db.table_size",
                vec![table_label.clone(), scope_label.clone()]
            )
            .set(bytes as f64);
            gauge!(
                "db.table_pages",
                vec![
                    table_label.clone(),
                    Label::new("type", "leaf"),
                    scope_label.clone(),
                ]
            )
            .set(leaf as f64);
            gauge!(
                "db.table_pages",
                vec![
                    table_label.clone(),
                    Label::new("type", "branch"),
                    scope_label.clone(),
                ]
            )
            .set(branch as f64);
            gauge!(
                "db.table_pages",
                vec![
                    table_label.clone(),
                    Label::new("type", "overflow"),
                    scope_label.clone(),
                ]
            )
            .set(overflow as f64);
            gauge!("db.table_entries", vec![table_label, scope_label]).set(stats.entries() as f64);
        }
        Ok(())
    });

    match view_outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = ?e, "Irys consensus DB gauge enumeration failed"),
        Err(e) => warn!(error = ?e, "Irys consensus DB gauge view tx failed"),
    }

    match db.freelist() {
        Ok(freelist) => {
            gauge!("db.freelist", "scope" => "irys-consensus").set(freelist as f64);
        }
        Err(e) => warn!(error = ?e, "Irys consensus DB freelist read failed"),
    }

    match db.stat() {
        Ok(stat) => {
            gauge!("db.page_size", "scope" => "irys-consensus").set(f64::from(stat.page_size()));
        }
        Err(e) => warn!(error = ?e, "Irys consensus DB stat read failed"),
    }

    gauge!(
        "db.timed_out_not_aborted_transactions",
        "scope" => "irys-consensus"
    )
    .set(db.timed_out_not_aborted_transactions() as f64);
}
