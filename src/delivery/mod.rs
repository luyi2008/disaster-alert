mod bark;
mod context;
mod message;

pub(crate) use bark::{AlertRecipient, BarkDeliveryError, BarkPermit, CountdownRecipient};
pub(crate) use bark::{BarkNotifier, BarkPushConfig};
#[cfg(test)]
pub(crate) use context::NotificationIntensityBandSnapshot;
pub(crate) use context::NotificationLinkService;
pub(crate) use context::{NotificationContextInput, NotificationVerifyError};
pub(crate) use context::{
    NotificationEventSnapshot, NotificationRuleSnapshot, NotificationSnapshot,
    NotificationSourcesSnapshot,
};
pub(crate) use message::{AlertTiming, remaining_seconds};

use crate::models::{DisasterCategory, IncidentId, InterruptionLevel};
use crate::subscriptions::{DestinationNumericId, SubscriptionId};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_DELIVERY_BATCH_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryBatch {
    pub(crate) id: u64,
    pub(crate) incident_id: IncidentId,
    pub(crate) event_revision: u64,
    pub(crate) category: DisasterCategory,
    pub(crate) shard: u16,
    pub(crate) created_at_ms: i64,
    pub(crate) rows: Vec<DeliveryRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryRow {
    pub(crate) destination_id: DestinationNumericId,
    pub(crate) subscription_id: SubscriptionId,
    pub(crate) generation: u64,
    pub(crate) target_ordinal: u8,
    pub(crate) match_kind: u8,
    pub(crate) interruption_level: InterruptionLevel,
    pub(crate) distance_m: u32,
    pub(crate) intensity_cent: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryItem {
    pub(crate) id: u64,
    pub(crate) batch_id: u64,
    pub(crate) row_index: u32,
    pub(crate) destination_id: DestinationNumericId,
    pub(crate) due_at_ms: i64,
    pub(crate) attempts: u16,
    pub(crate) created_at_ms: i64,
    pub(crate) last_error: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DeliverySuccess {
    pub(crate) row_index: u32,
    pub(crate) row: DeliveryRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeadLetterItem {
    pub(crate) id: u64,
    pub(crate) batch_id: u64,
    pub(crate) row_index: u32,
    pub(crate) destination_id: DestinationNumericId,
    pub(crate) attempts: u16,
    pub(crate) created_at_ms: i64,
    pub(crate) failed_at_ms: i64,
    pub(crate) permanent: bool,
    pub(crate) last_error: String,
}

impl DeliveryBatch {
    pub(crate) fn encoded_len(&self) -> anyhow::Result<usize> {
        crate::storage::encode_record(self).map(|value| value.len())
    }
}
