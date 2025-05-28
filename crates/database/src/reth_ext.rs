use reth_db::{
    metrics::{DatabaseEnvMetrics, FxHashMap, Labels, Operation, OperationMetrics},
    table::TableInfo,
    DatabaseEnv, TableSet,
};
use strum::{EnumCount, IntoEnumIterator};

pub trait IrysRethDatabaseEnvMetricsExt {
    fn with_metrics_and_tables<T: TableSet + TableInfo>(self, tables: &[T]) -> Self;
}

impl IrysRethDatabaseEnvMetricsExt for DatabaseEnv {
    /// register custom tables with DatabaseEnvMetrics
    fn with_metrics_and_tables<T: TableSet + TableInfo>(mut self, tables: &[T]) -> Self {
        self.metrics = Some(DatabaseEnvMetrics::new_with_tables(tables).into());
        self
    }
}

// TODO: better name
pub trait IrysRethDatabaseEnvMetricsExt2 {
    fn new_with_tables<T: TableSet + TableInfo>(tables: &[T]) -> Self;
    fn generate_operation_handles_with_tables<T: TableSet + TableInfo>(
        tables: &[T],
    ) -> FxHashMap<(&'static str, Operation), OperationMetrics>;
}

impl IrysRethDatabaseEnvMetricsExt2 for DatabaseEnvMetrics {
    fn new_with_tables<T: TableSet + TableInfo>(tables: &[T]) -> Self {
        // Pre-populate metric handle maps with all possible combinations of labels
        // to avoid runtime locks on the map when recording metrics.
        Self {
            operations: Self::generate_operation_handles_with_tables(tables),
            transactions: Self::generate_transaction_handles(),
            transaction_outcomes: Self::generate_transaction_outcome_handles(),
        }
    }

    fn generate_operation_handles_with_tables<T: TableSet + TableInfo>(
        tables: &[T],
    ) -> FxHashMap<(&'static str, Operation), OperationMetrics> {
        let mut operations = FxHashMap::with_capacity_and_hasher(
            tables.len() * Operation::COUNT,
            Default::default(),
        );
        for table in tables {
            for operation in Operation::iter() {
                operations.insert(
                    (table.name(), operation),
                    OperationMetrics::new_with_labels(&[
                        (Labels::Table.as_str(), table.name()),
                        (Labels::Operation.as_str(), operation.as_str()),
                    ]),
                );
            }
        }
        operations
    }
}
