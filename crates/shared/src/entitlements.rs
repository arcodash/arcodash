//! Extension point for restricting what an organization may do. Self-hosted deployments use
//! [`Unrestricted`]; a deployment can supply its own [`OrgEntitlements`].

use sea_orm::DatabaseConnection;

use crate::entities::organization;

/// Outcome of asking whether an org may run background work (scheduled queries, monitors,
/// notifications, digests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkGate {
    Allowed,
    /// `reason` is a short human-readable skip reason recorded on the skipped run.
    Blocked {
        reason: &'static str,
    },
}

impl WorkGate {
    #[must_use]
    pub const fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
}

/// Lets a deployment restrict what an organization may do. The default, [`Unrestricted`],
/// allows everything.
#[async_trait::async_trait]
pub trait OrgEntitlements: Send + Sync {
    /// Whether background work for `org_id` may run. Implementations that cannot decide should
    /// fail closed with a `reason` that says so.
    async fn scheduled_work(&self, db: &DatabaseConnection, org_id: i64) -> WorkGate;

    /// Whether the org may use AI features.
    fn ai_features(&self, org: &organization::Model) -> bool;
}

/// Allows everything.
pub struct Unrestricted;

#[async_trait::async_trait]
impl OrgEntitlements for Unrestricted {
    async fn scheduled_work(&self, _db: &DatabaseConnection, _org_id: i64) -> WorkGate {
        WorkGate::Allowed
    }

    fn ai_features(&self, _org: &organization::Model) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrestricted_allows_everything() {
        let db = DatabaseConnection::default();
        let org = organization::Model {
            id: 1,
            name: "o".into(),
            slug: "o".into(),
            settings: serde_json::json!({}),
            created_at: chrono::Utc::now().into(),
            updated_at: chrono::Utc::now().into(),
        };
        assert_eq!(Unrestricted.scheduled_work(&db, 1).await, WorkGate::Allowed);
        assert!(!WorkGate::Allowed.is_blocked());
        assert!(WorkGate::Blocked { reason: "x" }.is_blocked());
        assert!(Unrestricted.ai_features(&org));
    }
}
