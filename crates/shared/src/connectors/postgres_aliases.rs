//! Connectors that share the PostgreSQL wire protocol: Redshift, `CockroachDB`.
//! Each delegates entirely to the Postgres implementation under a different type name.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::postgres::PostgresConnector;
use super::{NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema};

macro_rules! pg_alias {
    ($name:ident, $type_name:literal, $default_port:expr) => {
        pub struct $name {
            inner: PostgresConnector,
        }

        impl $name {
            pub const fn new() -> Self {
                $name { inner: PostgresConnector::new() }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        #[async_trait]
        impl QueryRunner for $name {
            fn type_name(&self) -> &'static str {
                $type_name
            }

            fn configuration_schema(&self) -> Value {
                let mut schema = self.inner.configuration_schema();
                if let Some(port) = schema.pointer_mut("/properties/port") {
                    *port = json!({ "type": "integer", "title": "Port", "default": $default_port });
                }
                schema
            }

            async fn test_connection(&self, opts: &Value) -> Result<()> {
                self.inner.test_connection(opts).await
            }

            async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
                self.inner.get_schema(opts).await
            }

            async fn get_foreign_keys(&self, opts: &Value) -> Result<Vec<super::ForeignKey>> {
                self.inner.get_foreign_keys(opts).await
            }

            async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
                self.inner.get_view_names(opts).await
            }

            async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
                self.inner.run_query(opts, query).await
            }

            fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
                self.inner.network_targets(opts)
            }

            fn supports_auto_limit(&self) -> bool {
                self.inner.supports_auto_limit()
            }
        }
    };
}

pg_alias!(RedshiftConnector, "redshift", 5439);
pg_alias!(CockroachDbConnector, "cockroachdb", 26257);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::QueryRunner;

    #[test]
    fn aliases_forward_supports_auto_limit_from_postgres() {
        let postgres = PostgresConnector::new().supports_auto_limit();
        assert_eq!(RedshiftConnector::new().supports_auto_limit(), postgres);
        assert_eq!(CockroachDbConnector::new().supports_auto_limit(), postgres);
    }
}
