use crate::models::{
    AlertRule, DisasterCategory, DisasterEvent, InterruptionLevel, SourceSelection, Subscription,
};
use crate::utils::region;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReferenceMatch {
    pub(crate) target_ordinal: u8,
    pub(crate) match_kind: u8,
    pub(crate) interruption_level: InterruptionLevel,
}

pub(crate) fn match_subscription(
    subscription: &Subscription,
    event: &DisasterEvent,
) -> Option<ReferenceMatch> {
    let rule = subscription
        .alerts
        .iter()
        .find(|rule| rule.category() == event.category)?;
    if !source_matches(rule.sources(), &event.source) || !threshold_matches(rule, event) {
        return None;
    }

    let event_regions = event
        .affected_regions
        .iter()
        .map(|value| region::normalize(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let mut best = None;
    for (ordinal, target) in subscription.targets.iter().enumerate() {
        let administrative = [
            target.region.province.as_str(),
            target.region.city.as_str(),
            target.region.district.as_str(),
        ]
        .into_iter()
        .map(region::normalize)
        .filter(|value| !value.is_empty())
        .any(|target_region| event_regions.contains(&target_region));
        let distance = event
            .latitude
            .zip(event.longitude)
            .map(|(latitude, longitude)| {
                spherical_distance_km(
                    latitude,
                    longitude,
                    target.point.latitude,
                    target.point.longitude,
                )
            });
        let distance_limit = rule_distance_km(rule);
        let (distance_km, match_kind) = match event.category {
            DisasterCategory::EarthquakeWarning | DisasterCategory::EarthquakeReport => {
                (distance?, 1)
            }
            DisasterCategory::WeatherWarning if administrative => (distance.unwrap_or(0.0), 2),
            DisasterCategory::WeatherWarning => {
                (distance.filter(|value| *value <= distance_limit)?, 1)
            }
            DisasterCategory::Tsunami if administrative => (distance.unwrap_or(0.0), 2),
            DisasterCategory::Tsunami => continue,
            DisasterCategory::Typhoon => (distance.filter(|value| *value <= distance_limit)?, 1),
        };
        if event.category == DisasterCategory::EarthquakeWarning && distance_km > distance_limit {
            continue;
        }
        let interruption_level = match rule {
            AlertRule::EarthquakeWarning {
                estimated_intensity_bands,
                ..
            } => {
                let depth = event.depth_km.unwrap_or_default().max(0.0);
                let hypocentral = (distance_km.mul_add(distance_km, depth * depth)).sqrt();
                let estimated =
                    crate::utils::intensity::estimate_intensity(event.magnitude?, hypocentral);
                let rounded = estimated.round() as u8;
                let Some(band) = estimated_intensity_bands
                    .iter()
                    .find(|band| rounded >= band.min && rounded <= band.max)
                else {
                    continue;
                };
                band.interruption_level
            }
            AlertRule::EarthquakeReport { .. } => InterruptionLevel::Passive,
            AlertRule::WeatherWarning { .. }
            | AlertRule::Tsunami { .. }
            | AlertRule::Typhoon { .. } => bark_level(event.level),
        };
        if best.is_none_or(|(_, current, _, _)| distance_km < current) {
            best = Some((ordinal, distance_km, match_kind, interruption_level));
        }
    }
    let (ordinal, _distance_km, match_kind, interruption_level) = best?;
    Some(ReferenceMatch {
        target_ordinal: u8::try_from(ordinal).ok()?,
        match_kind,
        interruption_level,
    })
}

pub(crate) fn sample_events(subscriptions: &[Subscription], limit: usize) -> Vec<DisasterEvent> {
    if limit == 0 {
        return Vec::new();
    }
    let mut events = Vec::new();
    for subscription in subscriptions {
        for rule in &subscription.alerts {
            for target in &subscription.targets {
                let Some(source) = sample_source(rule) else {
                    continue;
                };
                let affected_regions = [
                    target.region.district.as_str(),
                    target.region.city.as_str(),
                    target.region.province.as_str(),
                ]
                .into_iter()
                .find(|value| !region::normalize(value).is_empty())
                .map_or_else(Vec::new, |value| vec![value.to_string()]);
                events.push(DisasterEvent {
                    category: rule.category(),
                    channel: source.channel,
                    source: source.id.to_string(),
                    event_id: format!("migration-sample-{}", events.len()),
                    revision: "1".to_string(),
                    report_num: 1,
                    title: String::new(),
                    description: String::new(),
                    latitude: Some(target.point.latitude),
                    longitude: Some(target.point.longitude),
                    magnitude: Some(8.0),
                    depth_km: Some(0.0),
                    affected_regions,
                    radius_km: None,
                    level: 4,
                    occurred_at: "2026-01-01T00:00:00Z".to_string(),
                    final_report: false,
                    cancel: false,
                    training: false,
                });
                if events.len() >= limit {
                    return events;
                }
            }
        }
    }
    events
}

fn sample_source(rule: &AlertRule) -> Option<&'static crate::source_registry::SourceDefinition> {
    match rule.sources() {
        SourceSelection::All => crate::source_registry::SOURCES
            .iter()
            .find(|source| source.category == rule.category()),
        SourceSelection::Include { ids } => {
            ids.first().and_then(|id| crate::source_registry::find(id))
        }
    }
}

fn source_matches(selection: &SourceSelection, source: &str) -> bool {
    match selection {
        SourceSelection::All => true,
        SourceSelection::Include { ids } => ids.iter().any(|value| value == source),
    }
}

fn threshold_matches(rule: &AlertRule, event: &DisasterEvent) -> bool {
    match rule {
        AlertRule::EarthquakeWarning { .. } | AlertRule::Typhoon { .. } => true,
        AlertRule::EarthquakeReport { min_magnitude, .. } => {
            event.magnitude.unwrap_or_default() >= *min_magnitude
        }
        AlertRule::WeatherWarning { min_severity, .. }
        | AlertRule::Tsunami { min_severity, .. } => event.level >= *min_severity,
    }
}

fn rule_distance_km(rule: &AlertRule) -> f64 {
    match rule {
        AlertRule::EarthquakeWarning { .. }
        | AlertRule::EarthquakeReport { .. }
        | AlertRule::Tsunami { .. } => 20_000.0,
        AlertRule::WeatherWarning {
            fallback_radius_km, ..
        } => *fallback_radius_km,
        AlertRule::Typhoon {
            max_center_distance_km,
            ..
        } => *max_center_distance_km,
    }
}

fn spherical_distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let latitude_delta = (lat2 - lat1).to_radians();
    let longitude_delta = (lon2 - lon1).to_radians();
    let left = lat1.to_radians();
    let right = lat2.to_radians();
    let a = ((latitude_delta / 2.0).sin().powi(2)
        + left.cos() * right.cos() * (longitude_delta / 2.0).sin().powi(2))
    .clamp(0.0, 1.0);
    6_371.008_8 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn bark_level(level: u8) -> InterruptionLevel {
    if level >= 3 {
        InterruptionLevel::Critical
    } else if level >= 2 {
        InterruptionLevel::Active
    } else {
        InterruptionLevel::Passive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AlertRule, DisasterCategory, DisasterEvent, InterruptionLevel, SourceSelection,
        Subscription,
    };

    #[test]
    fn zero_sample_limit_returns_no_events() {
        assert!(sample_events(&[], 0).is_empty());
    }

    fn subscription(alerts: Vec<AlertRule>) -> Subscription {
        Subscription::new(
            crate::models::NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: "abc123".to_string(),
            },
            vec![crate::models::MonitoringTarget {
                label: "shanghai".to_string(),
                point: crate::models::GeoPoint {
                    latitude: 31.2304,
                    longitude: 121.4737,
                },
                region: crate::models::AdministrativeRegion::default(),
            }],
            alerts,
        )
    }

    fn distant_xinjiang(category: DisasterCategory) -> DisasterEvent {
        DisasterEvent {
            category,
            channel: crate::models::ProviderChannel::FanStudio,
            source: "fanstudio.cenc".to_string(),
            event_id: "xinjiang-1".to_string(),
            revision: "1".to_string(),
            report_num: 1,
            title: "地震信息 新疆阿克苏地区沙雅县".to_string(),
            description: "M3.1 新疆阿克苏地区沙雅县".to_string(),
            latitude: Some(40.99),
            longitude: Some(83.54),
            magnitude: Some(3.1),
            depth_km: Some(21.0),
            affected_regions: vec!["新疆".to_string()],
            radius_km: None,
            level: 1,
            occurred_at: "2026-08-30T00:05:04Z".to_string(),
            final_report: true,
            cancel: false,
            training: false,
        }
    }

    #[test]
    fn earthquake_report_notifies_far_events_that_meet_min_magnitude() {
        let subscription = subscription(vec![AlertRule::EarthquakeReport {
            sources: SourceSelection::All,
            min_magnitude: 1.0,
        }]);
        assert!(
            match_subscription(
                &subscription,
                &distant_xinjiang(DisasterCategory::EarthquakeReport)
            )
            .is_some()
        );
    }

    #[test]
    fn earthquake_report_uses_passive_interruption_regardless_of_event_level() {
        let subscription = subscription(vec![AlertRule::EarthquakeReport {
            sources: SourceSelection::All,
            min_magnitude: 1.0,
        }]);
        let mut report = distant_xinjiang(DisasterCategory::EarthquakeReport);
        report.level = 4;
        report.magnitude = Some(7.2);
        let matched = match_subscription(&subscription, &report)
            .expect("reviewed reports that meet min magnitude should match");
        assert_eq!(matched.interruption_level, InterruptionLevel::Passive);
    }

    #[test]
    fn earthquake_warning_does_not_notify_when_local_intensity_is_below_bands() {
        let subscription = subscription(vec![AlertRule::EarthquakeWarning {
            sources: SourceSelection::All,
            estimated_intensity_bands: vec![crate::models::IntensityBand {
                min: 1,
                max: 7,
                interruption_level: InterruptionLevel::Passive,
            }],
        }]);
        assert!(
            match_subscription(
                &subscription,
                &distant_xinjiang(DisasterCategory::EarthquakeWarning)
            )
            .is_none()
        );
    }
}
