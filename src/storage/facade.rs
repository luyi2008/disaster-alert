use super::{DeviceDeliveryRecord, FjallStorage, try_now_millis};
use crate::models::{IncidentId, IncidentRecord};
use crate::subscriptions::SubscriptionManager;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Storage {
    inner: FjallStorage,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RetentionPolicy {
    pub(crate) incident_days: u64,
    pub(crate) delivery_ledger_days: u64,
    pub(crate) operation_days: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PruneStats {
    pub(crate) incidents: usize,
    pub(crate) delivery_records: usize,
    pub(crate) events: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BacklogCounts {
    pub(crate) inbox: usize,
    pub(crate) match_jobs: usize,
    pub(crate) delivery_batches: usize,
    pub(crate) retries: usize,
}

impl PruneStats {
    #[must_use]
    pub(crate) const fn total(self) -> usize {
        self.incidents
            .saturating_add(self.delivery_records)
            .saturating_add(self.events)
    }
}

impl Storage {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            inner: FjallStorage::open(path)?,
        })
    }

    #[must_use]
    pub(crate) fn subscription_manager(&self) -> SubscriptionManager {
        SubscriptionManager::new(self.inner.clone())
    }

    pub(crate) fn incident(&self, id: &IncidentId) -> Result<Option<Arc<IncidentRecord>>> {
        Ok(self.inner.incident(id)?.map(Arc::new))
    }

    pub(crate) fn backlog_counts(&self) -> Result<BacklogCounts> {
        self.inner.backlog_counts()
    }

    pub(crate) fn prune_retained_data(&self, policy: RetentionPolicy) -> Result<PruneStats> {
        let now = try_now_millis()?;
        let stats = self.inner.prune(
            now.saturating_sub(days_ms(policy.incident_days)),
            now.saturating_sub(days_ms(policy.delivery_ledger_days)),
            now.saturating_sub(days_ms(policy.operation_days)),
        )?;
        Ok(PruneStats {
            incidents: stats.incidents,
            delivery_records: stats.delivery_records,
            events: stats.events,
        })
    }

    pub(crate) async fn flush(&self) -> Result<()> {
        let storage = self.inner.clone();
        tokio::task::spawn_blocking(move || storage.persist())
            .await
            .context("Fjall persist task failed")?
    }

    pub(crate) fn deliveries_for_device_key(
        &self,
        device_key: Option<&str>,
        limit: usize,
    ) -> Result<Option<Vec<DeviceDeliveryRecord>>> {
        self.inner.deliveries_for_device_key(device_key, limit)
    }

    pub(crate) fn inner(&self) -> FjallStorage {
        self.inner.clone()
    }
}

const fn days_ms(days: u64) -> i64 {
    let millis = days.saturating_mul(86_400_000);
    if millis > i64::MAX as u64 {
        i64::MAX
    } else {
        millis as i64
    }
}
