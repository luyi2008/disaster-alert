use crate::delivery::{
    AlertRecipient, AlertTiming, BarkDeliveryError, BarkNotifier, NotificationContextInput,
    NotificationLinkService,
};
use crate::models::{
    AlertRule, DisasterCategory, DisasterEvent, IncidentId, InterruptionLevel, MonitoringTarget,
    ProviderChannel, SourceSelection, Subscription, mask_device_key,
};
use crate::source_registry;
use crate::storage::try_now_millis;
use crate::subscriptions::SubscriptionManager;
use crate::utils::{distance, intensity};
use anyhow::{Context, Result};
use serde::Serialize;

pub(crate) const HISTORY_SOURCE_MAJOR: &str = "major";
pub(crate) const MAX_DEVICE_LIST: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UtcTimestamp {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
}

impl UtcTimestamp {
    pub(crate) fn from_unix_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let tod = seconds.rem_euclid(86_400) as u32;
        let (year, month, day) = civil_from_unix_days(days);
        Self {
            year,
            month,
            day,
            hour: tod / 3_600,
            minute: (tod % 3_600) / 60,
            second: tod % 60,
        }
    }

    pub(crate) fn compact(self) -> String {
        format!(
            "{:04}{:02}{:02}{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    pub(crate) fn rfc3339_z(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HistoryRecord {
    pub(crate) source: &'static str,
    pub(crate) key: &'static str,
    pub(crate) event_id: &'static str,
    pub(crate) origin_time: &'static str,
    pub(crate) hypocenter: &'static str,
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) magnitude: f64,
    pub(crate) depth_km: f64,
    pub(crate) max_intensity: &'static str,
    pub(crate) note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HistoryRecordView {
    pub(crate) source: String,
    pub(crate) key: String,
    pub(crate) event_id: String,
    pub(crate) origin_time: String,
    pub(crate) hypocenter: String,
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) magnitude: f64,
    pub(crate) depth_km: f64,
    pub(crate) max_intensity: String,
    pub(crate) note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) estimated_intensity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) distance_km: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hypocentral_km: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) enum SimulateMode {
    NotifyLevel(InterruptionLevel),
    History { source: String, key: String },
}

#[derive(Debug, Clone)]
pub(crate) struct SimulateOutcome {
    pub(crate) event_id: String,
    pub(crate) pushed: u32,
    pub(crate) skipped: u32,
    pub(crate) temporary: bool,
    pub(crate) notify_level: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) key: Option<String>,
}

pub(crate) fn parse_notify_level(value: &str) -> Option<InterruptionLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "passive" => Some(InterruptionLevel::Passive),
        "active" => Some(InterruptionLevel::Active),
        "critical" => Some(InterruptionLevel::Critical),
        _ => None,
    }
}

pub(crate) fn builtin_major_records() -> &'static [HistoryRecord] {
    &[
        HistoryRecord {
            source: HISTORY_SOURCE_MAJOR,
            key: "yibin-gaoxian-2026",
            event_id: "CENC-202606290012",
            origin_time: "2026-06-29 00:12:00",
            hypocenter: "四川宜宾市高县地震",
            latitude: 28.5,
            longitude: 104.69,
            magnitude: 5.5,
            depth_km: 6.0,
            max_intensity: "未知",
            note: "2026年6月29日四川宜宾市高县5.5级地震；中国地震台网正式测定，震源深度6公里。",
        },
        HistoryRecord {
            source: HISTORY_SOURCE_MAJOR,
            key: "wenchuan-2008",
            event_id: "USGS-usp000g650",
            origin_time: "2008-05-12 14:28:01",
            hypocenter: "四川汶川地震",
            latitude: 31.002,
            longitude: 103.322,
            magnitude: 7.9,
            depth_km: 19.0,
            max_intensity: "XI",
            note: "2008年汶川地震，USGS Mw7.9；中国地震局常用表述为 Ms8.0。",
        },
        HistoryRecord {
            source: HISTORY_SOURCE_MAJOR,
            key: "tangshan-1976",
            event_id: "USGS-Tangshan-1976",
            origin_time: "1976-07-28 03:42:00",
            hypocenter: "河北唐山地震",
            latitude: 39.57,
            longitude: 117.98,
            magnitude: 7.8,
            depth_km: 15.0,
            max_intensity: "XI",
            note: "1976年唐山地震，USGS 资料记录震级7.8、深度15km、震中烈度XI。",
        },
    ]
}

pub(crate) fn find_history_record<'a>(
    records: &'a [HistoryRecord],
    source: &str,
    key: &str,
) -> Option<&'a HistoryRecord> {
    records
        .iter()
        .find(|record| record.source.eq_ignore_ascii_case(source) && record.key == key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceListError {
    Empty,
    TooMany,
    InvalidKey,
}

pub(crate) fn resolve_device_keys(
    bearer_key: &str,
    device_id_list: Option<Vec<String>>,
) -> std::result::Result<Vec<String>, DeviceListError> {
    let Some(list) = device_id_list else {
        return Ok(vec![bearer_key.to_string()]);
    };
    if list.is_empty() {
        return Err(DeviceListError::Empty);
    }
    if list.len() > MAX_DEVICE_LIST {
        return Err(DeviceListError::TooMany);
    }
    let mut resolved = Vec::with_capacity(list.len());
    for raw in list {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.len() > 64
            || !trimmed.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(DeviceListError::InvalidKey);
        }
        if !resolved.iter().any(|existing| existing == trimmed) {
            resolved.push(trimmed.to_string());
        }
    }
    if resolved.is_empty() {
        return Err(DeviceListError::Empty);
    }
    Ok(resolved)
}

pub(crate) fn history_views(
    records: &[HistoryRecord],
    subscription: Option<&Subscription>,
) -> Vec<HistoryRecordView> {
    records
        .iter()
        .map(|record| {
            let mut view = HistoryRecordView {
                source: record.source.to_string(),
                key: record.key.to_string(),
                event_id: record.event_id.to_string(),
                origin_time: record.origin_time.to_string(),
                hypocenter: record.hypocenter.to_string(),
                latitude: record.latitude,
                longitude: record.longitude,
                magnitude: record.magnitude,
                depth_km: record.depth_km,
                max_intensity: record.max_intensity.to_string(),
                note: record.note.to_string(),
                estimated_intensity: None,
                distance_km: None,
                hypocentral_km: None,
            };
            if let Some(subscription) = subscription
                && let Some((distance_km, hypocentral_km, estimated)) = preview_timing(
                    subscription,
                    record.latitude,
                    record.longitude,
                    record.depth_km,
                    record.magnitude,
                )
            {
                view.distance_km = Some((distance_km * 10.0).round() / 10.0);
                view.hypocentral_km = Some((hypocentral_km * 10.0).round() / 10.0);
                view.estimated_intensity = Some((estimated * 10.0).round() / 10.0);
            }
            view
        })
        .collect()
}

pub(crate) fn simulated_event(
    subscription: &Subscription,
    notify_level: InterruptionLevel,
    event_id: &str,
    occurred_at: &str,
) -> Option<DisasterEvent> {
    let rule = subscription
        .alert(DisasterCategory::EarthquakeWarning)
        .cloned()
        .unwrap_or_else(|| AlertRule::default_for(DisasterCategory::EarthquakeWarning));
    let (channel, source) = event_source(&rule);
    let target = nearest_or_first_target(subscription, None)?;
    let (magnitude, depth_km) = simulation_magnitude(notify_level);
    let wanted = target_intensity(&rule, notify_level);
    let distance_km = simulation_distance_for_intensity(magnitude, depth_km, wanted);
    let mut latitude = target.point.latitude + distance_km / 111.32;
    if latitude > 89.0 {
        latitude = target.point.latitude - distance_km / 111.32;
    }
    let level = match notify_level {
        InterruptionLevel::Passive => 1,
        InterruptionLevel::Active => 2,
        InterruptionLevel::Critical => 4,
    };
    Some(DisasterEvent {
        category: DisasterCategory::EarthquakeWarning,
        channel,
        source,
        event_id: event_id.to_string(),
        revision: "1".to_string(),
        report_num: 1,
        title: format!("地震预警 模拟震源（{} 测试）", notify_level.as_str()),
        description: String::new(),
        latitude: Some(latitude),
        longitude: Some(target.point.longitude),
        magnitude: Some(magnitude),
        depth_km: Some(depth_km),
        affected_regions: Vec::new(),
        radius_km: None,
        level,
        occurred_at: occurred_at.to_string(),
        final_report: false,
        cancel: false,
        training: true,
    })
}

pub(crate) fn historical_event(
    record: &HistoryRecord,
    event_id: &str,
    occurred_at: &str,
) -> DisasterEvent {
    let source = source_registry::find("wolfx.cenc_eew");
    DisasterEvent {
        category: DisasterCategory::EarthquakeWarning,
        channel: source.map_or(ProviderChannel::Wolfx, |item| item.channel),
        source: source.map_or_else(|| "wolfx.cenc_eew".to_string(), |item| item.id.to_string()),
        event_id: event_id.to_string(),
        revision: "1".to_string(),
        report_num: 1,
        title: format!("地震预警 {}", record.hypocenter),
        description: format!("原发震时间：{}", record.origin_time),
        latitude: Some(record.latitude),
        longitude: Some(record.longitude),
        magnitude: Some(record.magnitude),
        depth_km: Some(record.depth_km),
        affected_regions: Vec::new(),
        radius_km: None,
        level: 4,
        occurred_at: occurred_at.to_string(),
        final_report: false,
        cancel: false,
        training: false,
    }
}

pub(crate) async fn dispatch(
    manager: &SubscriptionManager,
    notifier: &BarkNotifier,
    links: &NotificationLinkService,
    p_wave_km_s: f64,
    s_wave_km_s: f64,
    device_keys: &[String],
    mode: &SimulateMode,
) -> Result<SimulateOutcome> {
    let now_ms = try_now_millis()?;
    let timestamp = UtcTimestamp::from_unix_seconds(now_ms.div_euclid(1_000));
    let occurred_at = timestamp.rfc3339_z();
    let compact = timestamp.compact();
    let (shared_event, notify_level, source, key) = match mode {
        SimulateMode::NotifyLevel(level) => (None, Some(*level), None, None),
        SimulateMode::History { source, key } => {
            let record = find_history_record(builtin_major_records(), source, key)
                .context("history record disappeared after validation")?;
            let event_id = format!(
                "HIST-{}-{}-{compact}",
                source.to_ascii_uppercase(),
                record.event_id
            );
            (
                Some(historical_event(record, &event_id, &occurred_at)),
                None,
                Some(source.clone()),
                Some(key.clone()),
            )
        }
    };
    let response_event_id = shared_event
        .as_ref()
        .map(|event| event.event_id.clone())
        .unwrap_or_else(|| format!("SIM-{compact}"));

    let manager_for_lookup = manager.clone();
    let lookup_keys = device_keys.to_vec();
    let grouped = tokio::task::spawn_blocking(move || {
        lookup_keys
            .into_iter()
            .map(|device_key| {
                manager_for_lookup
                    .simulate_subscriptions_by_device_key(&device_key)
                    .map(|subscriptions| (device_key, subscriptions))
            })
            .collect::<Result<Vec<_>>>()
    })
    .await
    .context("simulate subscription lookup task failed")??;

    let mut pushed = 0_u32;
    let mut skipped = 0_u32;
    for (index, (device_key, subscriptions)) in grouped.into_iter().enumerate() {
        if subscriptions.is_empty() {
            tracing::info!(
                event = "simulate.subscription_missing",
                device_key = %mask_device_key(&device_key),
                "simulate.subscription_missing"
            );
            skipped = skipped.saturating_add(1);
            continue;
        }
        for subscription in subscriptions {
            let event = if let Some(event) = &shared_event {
                event.clone()
            } else {
                let Some(level) = notify_level else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let event_id = if index == 0 {
                    response_event_id.clone()
                } else {
                    format!("{response_event_id}-{index}")
                };
                let Some(event) = simulated_event(&subscription, level, &event_id, &occurred_at)
                else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                event
            };
            match send_one(
                DispatchContext {
                    notifier,
                    links,
                    p_wave_km_s,
                    s_wave_km_s,
                },
                &subscription,
                &event,
                notify_level.unwrap_or(InterruptionLevel::Active),
                now_ms,
            )
            .await
            {
                Ok(()) => pushed = pushed.saturating_add(1),
                Err(error) => {
                    tracing::warn!(
                        event = "simulate.push_failed",
                        device_key = %mask_device_key(&device_key),
                        error = ?error,
                        "simulate.push_failed"
                    );
                    skipped = skipped.saturating_add(1);
                }
            }
        }
    }

    Ok(SimulateOutcome {
        event_id: response_event_id,
        pushed,
        skipped,
        temporary: false,
        notify_level: notify_level
            .map(InterruptionLevel::as_str)
            .map(str::to_string),
        source,
        key,
    })
}

struct DispatchContext<'a> {
    notifier: &'a BarkNotifier,
    links: &'a NotificationLinkService,
    p_wave_km_s: f64,
    s_wave_km_s: f64,
}

async fn send_one(
    context: DispatchContext<'_>,
    subscription: &Subscription,
    event: &DisasterEvent,
    requested_level: InterruptionLevel,
    now_ms: i64,
) -> Result<()> {
    let Some(target) = nearest_or_first_target(subscription, event.latitude.zip(event.longitude))
    else {
        anyhow::bail!("subscription has no monitoring target");
    };
    let timing = alert_timing(
        event,
        target,
        context.p_wave_km_s,
        context.s_wave_km_s,
        now_ms,
    );
    let interruption =
        interruption_for_event(subscription, event, target).unwrap_or(requested_level);
    let rule = subscription
        .alert(event.category)
        .cloned()
        .unwrap_or_else(|| AlertRule::default_for(event.category));
    let incident_id = IncidentId::derive(&event.event_key());
    let prepared = context.links.prepare_url_for(NotificationContextInput {
        incident_id: &incident_id,
        event,
        target,
        timing: timing.as_ref(),
        interruption_level: interruption.as_str(),
        matched_rule: &rule,
        issued_at_ms: now_ms,
    })?;
    let persist_links = context.links.clone();
    let persist_context = prepared.clone();
    tokio::task::spawn_blocking(move || persist_links.persist_prepared(&persist_context))
        .await
        .context("notification context persist task failed")??;
    let recipient = AlertRecipient::new(subscription, target);
    context
        .notifier
        .send_disaster_alert(
            &recipient,
            interruption.as_str(),
            event,
            timing.as_ref(),
            &prepared.url,
        )
        .await
        .map_err(|error| match error {
            BarkDeliveryError::Transient(error) | BarkDeliveryError::Permanent(error) => error,
        })?;
    Ok(())
}

fn preview_timing(
    subscription: &Subscription,
    latitude: f64,
    longitude: f64,
    depth_km: f64,
    magnitude: f64,
) -> Option<(f64, f64, f64)> {
    let target = nearest_or_first_target(subscription, Some((latitude, longitude)))?;
    let distance_km = distance::vincenty_distance(
        latitude,
        longitude,
        target.point.latitude,
        target.point.longitude,
    )?;
    let hypocentral_km = distance_km
        .mul_add(distance_km, depth_km.max(0.0) * depth_km.max(0.0))
        .sqrt();
    let estimated = intensity::estimate_intensity(magnitude, hypocentral_km);
    Some((distance_km, hypocentral_km, estimated))
}

fn nearest_or_first_target(
    subscription: &Subscription,
    epicenter: Option<(f64, f64)>,
) -> Option<&MonitoringTarget> {
    let Some((latitude, longitude)) = epicenter else {
        return subscription.targets.first();
    };
    subscription
        .targets
        .iter()
        .min_by(|left, right| {
            let left_distance = distance::vincenty_distance(
                latitude,
                longitude,
                left.point.latitude,
                left.point.longitude,
            )
            .unwrap_or(f64::MAX);
            let right_distance = distance::vincenty_distance(
                latitude,
                longitude,
                right.point.latitude,
                right.point.longitude,
            )
            .unwrap_or(f64::MAX);
            left_distance
                .partial_cmp(&right_distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .or_else(|| subscription.targets.first())
}

fn event_source(rule: &AlertRule) -> (ProviderChannel, String) {
    let preferred = match rule.sources() {
        SourceSelection::All => source_registry::SOURCES
            .iter()
            .find(|source| source.category == rule.category()),
        SourceSelection::Include { ids } => ids
            .iter()
            .find_map(|id| source_registry::find(id))
            .filter(|source| source.category == rule.category()),
    };
    preferred
        .or_else(|| source_registry::find("wolfx.cenc_eew"))
        .map_or(
            (ProviderChannel::Wolfx, "wolfx.cenc_eew".to_string()),
            |source| (source.channel, source.id.to_string()),
        )
}

fn simulation_magnitude(level: InterruptionLevel) -> (f64, f64) {
    match level {
        InterruptionLevel::Passive => (3.6, 12.0),
        InterruptionLevel::Active => (4.6, 10.0),
        InterruptionLevel::Critical => (6.2, 10.0),
    }
}

fn target_intensity(rule: &AlertRule, level: InterruptionLevel) -> u8 {
    let AlertRule::EarthquakeWarning {
        estimated_intensity_bands,
        ..
    } = rule
    else {
        return match level {
            InterruptionLevel::Passive => 1,
            InterruptionLevel::Active => 2,
            InterruptionLevel::Critical => 3,
        };
    };
    let matching = estimated_intensity_bands
        .iter()
        .filter(|band| band.interruption_level == level)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return match level {
            InterruptionLevel::Passive => 1,
            InterruptionLevel::Active => 2,
            InterruptionLevel::Critical => 3,
        };
    }
    match level {
        InterruptionLevel::Passive => matching.iter().map(|band| band.max).max().unwrap_or(1),
        InterruptionLevel::Active => {
            let min = matching.iter().map(|band| band.min).min().unwrap_or(2);
            let max = matching.iter().map(|band| band.max).max().unwrap_or(2);
            min.saturating_add(max) / 2
        }
        InterruptionLevel::Critical => matching.iter().map(|band| band.min).min().unwrap_or(3),
    }
}

fn simulation_distance_for_intensity(magnitude: f64, depth_km: f64, target: u8) -> f64 {
    let mut best_distance = 1.0_f64;
    let mut best_delta = 99_i32;
    let mut distance = 1.0_f64;
    while distance <= 1_500.0 {
        let hypocentral = distance.mul_add(distance, depth_km * depth_km).sqrt();
        let estimated = intensity::estimate_intensity(magnitude, hypocentral);
        let delta = (estimated.round() as i32 - i32::from(target)).abs();
        if delta < best_delta {
            best_distance = distance;
            best_delta = delta;
        }
        if delta == 0 {
            return distance;
        }
        distance += 1.0;
    }
    best_distance
}

fn alert_timing(
    event: &DisasterEvent,
    target: &MonitoringTarget,
    p_wave_km_s: f64,
    s_wave_km_s: f64,
    now_ms: i64,
) -> Option<AlertTiming> {
    let (latitude, longitude) = event.latitude.zip(event.longitude)?;
    let distance_km = distance::vincenty_distance(
        latitude,
        longitude,
        target.point.latitude,
        target.point.longitude,
    )?;
    let depth = event.depth_km.unwrap_or_default().max(0.0);
    let hypocentral_km = distance_km.mul_add(distance_km, depth * depth).sqrt();
    let estimated_intensity = event.magnitude.map_or(0.0, |magnitude| {
        intensity::estimate_intensity(magnitude, hypocentral_km)
    });
    let occurred_at_ms = crate::models::parse_event_epoch(event)
        .map(|seconds| seconds.saturating_mul(1_000))
        .unwrap_or(now_ms);
    Some(AlertTiming {
        distance_km,
        hypocentral_km,
        estimated_intensity,
        p_arrival_at_ms: arrival_at(occurred_at_ms, hypocentral_km, p_wave_km_s),
        s_arrival_at_ms: arrival_at(occurred_at_ms, hypocentral_km, s_wave_km_s),
    })
}

fn interruption_for_event(
    subscription: &Subscription,
    event: &DisasterEvent,
    target: &MonitoringTarget,
) -> Option<InterruptionLevel> {
    let AlertRule::EarthquakeWarning {
        estimated_intensity_bands,
        ..
    } = subscription.alert(event.category)?
    else {
        return None;
    };
    let timing = alert_timing(event, target, 6.0, 3.5, 0)?;
    let rounded = timing.estimated_intensity.round() as u8;
    estimated_intensity_bands
        .iter()
        .find(|band| rounded >= band.min && rounded <= band.max)
        .map(|band| band.interruption_level)
}

fn arrival_at(occurred_at_ms: i64, distance_km: f64, speed_km_s: f64) -> i64 {
    if speed_km_s <= 0.0 {
        return occurred_at_ms;
    }
    occurred_at_ms.saturating_add((distance_km / speed_km_s * 1_000.0).round() as i64)
}

fn civil_from_unix_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{GeoPoint, NotificationDestination};

    fn subscription() -> Subscription {
        Subscription::new(
            NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: "abc123".to_string(),
            },
            vec![MonitoringTarget {
                label: "成都锦江".to_string(),
                point: GeoPoint {
                    latitude: 30.5954,
                    longitude: 104.0982,
                },
                region: Default::default(),
            }],
            vec![AlertRule::default_for(DisasterCategory::EarthquakeWarning)],
        )
    }

    #[test]
    fn utc_compact_round_trips_a_known_instant() {
        let stamp = UtcTimestamp::from_unix_seconds(1_788_017_584);
        assert_eq!(stamp.compact(), "20260829153304");
        assert_eq!(stamp.rfc3339_z(), "2026-08-29T15:33:04Z");
    }

    #[test]
    fn builtin_catalog_contains_live_major_keys() {
        let keys = builtin_major_records()
            .iter()
            .map(|record| record.key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            ["yibin-gaoxian-2026", "wenchuan-2008", "tangshan-1976"]
        );
    }

    #[test]
    fn simulated_event_hits_the_requested_notify_band() {
        let subscription = subscription();
        for level in [
            InterruptionLevel::Passive,
            InterruptionLevel::Active,
            InterruptionLevel::Critical,
        ] {
            let event = simulated_event(&subscription, level, "SIM-1", "2026-08-29T15:33:04Z");
            assert!(event.is_some());
            if let Some(event) = event {
                let target = subscription.targets.first();
                assert!(target.is_some());
                if let Some(target) = target {
                    let chosen = interruption_for_event(&subscription, &event, target);
                    assert_eq!(chosen, Some(level), "level {}", level.as_str());
                }
                assert!(event.training);
                assert_eq!(
                    crate::source_registry::find(&event.source).map(|source| source.category),
                    Some(DisasterCategory::EarthquakeWarning)
                );
            }
        }
    }

    #[test]
    fn historical_event_keeps_catalog_coordinates() {
        let record = find_history_record(builtin_major_records(), "major", "yibin-gaoxian-2026");
        assert!(record.is_some());
        if let Some(record) = record {
            let event = historical_event(record, "HIST-1", "2026-08-29T15:33:04Z");
            assert!(!event.training);
            assert_eq!(event.latitude, Some(28.5));
            assert_eq!(event.magnitude, Some(5.5));
            assert!(event.description.contains("2026-06-29 00:12:00"));
        }
    }

    #[test]
    fn yibin_preview_matches_chengdu_distance_scale() {
        let views = history_views(builtin_major_records(), Some(&subscription()));
        let yibin = views
            .iter()
            .find(|record| record.key == "yibin-gaoxian-2026");
        assert!(yibin.is_some());
        if let Some(yibin) = yibin {
            assert!(yibin.distance_km.is_some());
            if let Some(distance) = yibin.distance_km {
                assert!((230.0..250.0).contains(&distance), "distance {distance}");
            }
            assert!(yibin.estimated_intensity.is_some());
            if let Some(intensity) = yibin.estimated_intensity {
                assert!(intensity > 0.0 && intensity < 4.0, "intensity {intensity}");
            }
        }
    }

    #[test]
    fn resolve_device_keys_defaults_to_bearer_and_rejects_empty_list() {
        assert_eq!(
            resolve_device_keys("abc123", None).ok(),
            Some(vec!["abc123".to_string()])
        );
        assert_eq!(
            resolve_device_keys("abc123", Some(Vec::new())),
            Err(DeviceListError::Empty)
        );
        assert_eq!(
            resolve_device_keys("abc123", Some(vec!["keyA".to_string(), "keyA".to_string()])).ok(),
            Some(vec!["keyA".to_string()])
        );
    }
}
