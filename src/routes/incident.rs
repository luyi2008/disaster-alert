use crate::delivery::{
    NotificationEventSnapshot, NotificationRuleSnapshot, NotificationSnapshot,
    NotificationSourcesSnapshot, NotificationVerifyError,
};
use crate::models::{
    AlertRule, ApiResponse, DisasterCategory, DisasterEvent, IncidentId, IncidentRecord,
    IncidentReportSummary, IntensityBand, SourceSelection,
};
use crate::routes::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

enum DetailLoadError {
    InvalidLink(anyhow::Error),
    Storage(anyhow::Error),
}

#[derive(Debug, Serialize)]
struct IncidentDetailResponse {
    snapshot: PublicNotificationSnapshot,
    incident: Option<PublicIncident>,
}

#[derive(Debug, Serialize)]
struct PublicNotificationSnapshot {
    incident_id: String,
    issued_at_ms: i64,
    event: PublicSnapshotEvent,
    target: PublicTarget,
    timing: Option<PublicTiming>,
    interruption_level: String,
    matched_rule: AlertRule,
}

#[derive(Debug, Serialize)]
struct PublicSnapshotEvent {
    category: DisasterCategory,
    source: String,
    event_id: String,
    revision: String,
    report_num: u32,
    title: String,
    description: String,
    affected_regions: Vec<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    magnitude: Option<f64>,
    depth_km: Option<f64>,
    radius_km: Option<f64>,
    level: u8,
    occurred_at: String,
    final_report: bool,
    cancel: bool,
    training: bool,
}

#[derive(Debug, Serialize)]
struct PublicTarget {
    label: String,
    latitude: f64,
    longitude: f64,
    province: String,
    city: String,
    district: String,
}

#[derive(Debug, Serialize)]
struct PublicTiming {
    epicentral_distance_km: f64,
    hypocentral_distance_km: f64,
    estimated_intensity: f64,
    p_arrival_at_ms: i64,
    s_arrival_at_ms: i64,
}

#[derive(Debug, Serialize)]
struct PublicIncident {
    id: String,
    category: DisasterCategory,
    updated_at_ms: i64,
    latest_by_source: Vec<PublicSnapshotEvent>,
    timeline: Vec<IncidentReportSummary>,
}

pub(crate) async fn incident_detail_handler(
    State(state): State<AppState>,
    Path((incident_id, token)): Path<(String, String)>,
) -> Response {
    let Some(incident_id) = IncidentId::parse(&incident_id) else {
        return detail_not_found();
    };
    let Ok(permit) = state.detail_concurrency.clone().try_acquire_owned() else {
        tracing::warn!(
            event = "incident.detail_overloaded",
            incident_id = %incident_id.as_str(),
            "incident.detail_overloaded"
        );
        return detail_unavailable();
    };
    let Ok(database_permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        tracing::warn!(
            event = "incident.detail_storage_overloaded",
            incident_id = %incident_id.as_str(),
            "incident.detail_storage_overloaded"
        );
        return detail_unavailable();
    };
    let links = state.notification_links.clone();
    let verify_incident_id = incident_id.clone();
    let storage = state.storage.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _database_permit = database_permit;
        let snapshot = links
            .verify(&verify_incident_id, &token)
            .map_err(|error| match error {
                NotificationVerifyError::Invalid(error) => DetailLoadError::InvalidLink(error),
                NotificationVerifyError::Storage(error) => DetailLoadError::Storage(error),
            })?;
        let incident = storage
            .incident(&verify_incident_id)
            .map_err(DetailLoadError::Storage)?;
        Ok::<_, DetailLoadError>((snapshot, incident))
    })
    .await;
    let (snapshot, incident) = match loaded {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(DetailLoadError::InvalidLink(error))) => {
            tracing::warn!(
                event = "incident.invalid_notification_link",
                incident_id = %incident_id.as_str(),
                error = %error,
                "incident.invalid_notification_link"
            );
            return detail_not_found();
        }
        Ok(Err(DetailLoadError::Storage(error))) => {
            tracing::error!(
                event = "incident.read_failed",
                incident_id = %incident_id.as_str(),
                error = ?error,
                "incident.read_failed"
            );
            return detail_error();
        }
        Err(error) => {
            tracing::error!(
                event = "incident.notification_verify_task_failed",
                incident_id = %incident_id.as_str(),
                error = ?error,
                "incident.notification_verify_task_failed"
            );
            return detail_error();
        }
    };
    detail_json(
        StatusCode::OK,
        ApiResponse::success(
            "灾害详情获取成功",
            Some(IncidentDetailResponse {
                snapshot: PublicNotificationSnapshot::from_snapshot(&snapshot),
                incident: incident.as_deref().map(PublicIncident::from_record),
            }),
        ),
    )
}

fn detail_not_found() -> Response {
    detail_json(
        StatusCode::NOT_FOUND,
        ApiResponse::<()>::error("无法打开灾害详情"),
    )
}

fn detail_error() -> Response {
    detail_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiResponse::<()>::error("灾害详情加载失败"),
    )
}

fn detail_unavailable() -> Response {
    detail_json(
        StatusCode::SERVICE_UNAVAILABLE,
        ApiResponse::<()>::error("灾害详情暂不可用"),
    )
}

fn detail_json<T: Serialize>(status: StatusCode, body: ApiResponse<T>) -> Response {
    let mut response = (status, Json(body)).into_response();
    apply_detail_headers(response.headers_mut());
    response
}

fn apply_detail_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(header::EXPIRES, HeaderValue::from_static("0"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow, noarchive"),
    );
}

impl PublicNotificationSnapshot {
    fn from_snapshot(snapshot: &NotificationSnapshot) -> Self {
        Self {
            incident_id: snapshot.incident_id.as_str().to_string(),
            issued_at_ms: snapshot.issued_at_ms,
            event: PublicSnapshotEvent::from_notification_event(&snapshot.event),
            target: PublicTarget {
                label: snapshot.target.label.clone(),
                latitude: snapshot.target.latitude,
                longitude: snapshot.target.longitude,
                province: snapshot.target.province.clone(),
                city: snapshot.target.city.clone(),
                district: snapshot.target.district.clone(),
            },
            timing: snapshot.timing.map(|timing| PublicTiming {
                epicentral_distance_km: timing.epicentral_distance_km,
                hypocentral_distance_km: timing.hypocentral_distance_km,
                estimated_intensity: timing.estimated_intensity,
                p_arrival_at_ms: timing.p_arrival_at_ms,
                s_arrival_at_ms: timing.s_arrival_at_ms,
            }),
            interruption_level: snapshot.interruption_level.clone(),
            matched_rule: public_rule(&snapshot.matched_rule),
        }
    }
}

impl PublicSnapshotEvent {
    fn from_notification_event(event: &NotificationEventSnapshot) -> Self {
        Self {
            category: event.category,
            source: event.source.clone(),
            event_id: event.source_event_id.clone(),
            revision: event.revision.clone(),
            report_num: event.report_num,
            title: event.title.clone(),
            description: event.description.clone(),
            affected_regions: event.affected_regions.clone(),
            latitude: event.latitude,
            longitude: event.longitude,
            magnitude: event.magnitude,
            depth_km: event.depth_km,
            radius_km: event.radius_km,
            level: event.level,
            occurred_at: event.occurred_at.clone(),
            final_report: event.final_report,
            cancel: event.cancel,
            training: event.training,
        }
    }

    fn from_disaster_event(event: &DisasterEvent) -> Self {
        Self {
            category: event.category,
            source: event.source.clone(),
            event_id: event.event_id.clone(),
            revision: event.revision.clone(),
            report_num: event.report_num,
            title: event.title.clone(),
            description: event.description.clone(),
            affected_regions: event.affected_regions.clone(),
            latitude: event.latitude,
            longitude: event.longitude,
            magnitude: event.magnitude,
            depth_km: event.depth_km,
            radius_km: event.radius_km,
            level: event.level,
            occurred_at: event.occurred_at.clone(),
            final_report: event.final_report,
            cancel: event.cancel,
            training: event.training,
        }
    }
}

impl PublicIncident {
    fn from_record(incident: &IncidentRecord) -> Self {
        Self {
            id: incident.id.as_str().to_string(),
            category: incident.category,
            updated_at_ms: incident.updated_at_ms,
            latest_by_source: incident
                .latest_by_source
                .iter()
                .map(PublicSnapshotEvent::from_disaster_event)
                .collect(),
            timeline: incident.timeline.iter().cloned().collect(),
        }
    }
}

fn public_rule(rule: &NotificationRuleSnapshot) -> AlertRule {
    match rule {
        NotificationRuleSnapshot::EarthquakeWarning {
            sources,
            intensity_bands,
        } => AlertRule::EarthquakeWarning {
            sources: public_sources(sources),
            estimated_intensity_bands: intensity_bands
                .iter()
                .map(|band| IntensityBand {
                    min: band.min,
                    max: band.max,
                    interruption_level: band.interruption_level,
                })
                .collect(),
        },
        NotificationRuleSnapshot::EarthquakeReport {
            sources,
            min_magnitude,
        } => AlertRule::EarthquakeReport {
            sources: public_sources(sources),
            min_magnitude: *min_magnitude,
        },
        NotificationRuleSnapshot::WeatherWarning {
            sources,
            min_severity,
            fallback_radius_km,
        } => AlertRule::WeatherWarning {
            sources: public_sources(sources),
            min_severity: *min_severity,
            fallback_radius_km: *fallback_radius_km,
        },
        NotificationRuleSnapshot::Tsunami {
            sources,
            min_severity,
        } => AlertRule::Tsunami {
            sources: public_sources(sources),
            min_severity: *min_severity,
        },
        NotificationRuleSnapshot::Typhoon {
            sources,
            max_center_distance_km,
        } => AlertRule::Typhoon {
            sources: public_sources(sources),
            max_center_distance_km: *max_center_distance_km,
        },
    }
}

fn public_sources(sources: &NotificationSourcesSnapshot) -> SourceSelection {
    match sources {
        NotificationSourcesSnapshot::All => SourceSelection::All,
        NotificationSourcesSnapshot::Include(ids) => SourceSelection::Include { ids: ids.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_detail_headers, detail_error, detail_not_found, detail_unavailable,
        incident_detail_handler, public_rule,
    };
    use crate::delivery::{
        BarkNotifier, BarkPushConfig, NotificationContextInput, NotificationLinkService,
        NotificationRuleSnapshot, NotificationSourcesSnapshot,
    };
    use crate::models::{
        AdministrativeRegion, AlertRule, DisasterCategory, DisasterEvent, GeoPoint, IncidentId,
        IncidentRecord, InterruptionLevel, MonitoringTarget, ProviderChannel,
    };
    use crate::routes::{AppState, ReverseGeocoder};
    use crate::runtime::RuntimeStatus;
    use crate::storage::Storage;
    use crate::subscriptions::SubscriptionConfirmationService;
    use anyhow::Context;
    use axum::{
        body::to_bytes,
        extract::{Path, State},
        http::{StatusCode, header},
    };
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    struct Harness {
        state: AppState,
        _directory: tempfile::TempDir,
    }

    fn event(source: &str) -> DisasterEvent {
        DisasterEvent {
            category: DisasterCategory::EarthquakeWarning,
            channel: ProviderChannel::FanStudio,
            source: source.to_string(),
            event_id: format!("{source}-event"),
            revision: "1".to_string(),
            report_num: 1,
            title: "<script>alert(1)</script>".to_string(),
            description: "A & B".to_string(),
            latitude: Some(35.0),
            longitude: Some(139.0),
            magnitude: Some(5.0),
            depth_km: Some(10.0),
            affected_regions: vec!["<东京>".to_string()],
            radius_km: Some(120.0),
            level: 3,
            occurred_at: "2026-07-12 12:00:00".to_string(),
            final_report: false,
            cancel: false,
            training: false,
        }
    }

    fn target() -> MonitoringTarget {
        MonitoringTarget {
            label: "<住所>".to_string(),
            point: GeoPoint {
                latitude: 35.6,
                longitude: 139.6,
            },
            region: AdministrativeRegion {
                province: "东京都".to_string(),
                city: "东京".to_string(),
                district: String::new(),
            },
        }
    }

    fn harness() -> anyhow::Result<Harness> {
        let directory = tempfile::tempdir()?;
        let storage = Storage::open(directory.path())?;
        let notifier = BarkNotifier::new(
            vec!["https://api.day.app".to_string()],
            1,
            1,
            BarkPushConfig::new(None, 10, "test".to_string(), false),
        )?;
        let links = NotificationLinkService::for_test(&storage);
        let confirmations = SubscriptionConfirmationService::new(
            storage.subscription_manager(),
            notifier.clone(),
            1,
        );
        let state = AppState::new(
            storage,
            notifier,
            RuntimeStatus::default(),
            ReverseGeocoder::disabled(),
            links,
            confirmations,
            1,
        );
        Ok(Harness {
            state,
            _directory: directory,
        })
    }

    async fn json_body(response: axum::response::Response) -> anyhow::Result<Value> {
        let body = to_bytes(response.into_body(), 64 * 1024).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    fn assert_private_headers(response: &axum::response::Response) {
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("private, no-store")
        );
        assert_eq!(
            response
                .headers()
                .get(header::REFERRER_POLICY)
                .and_then(|value| value.to_str().ok()),
            Some("no-referrer")
        );
        assert_eq!(
            response
                .headers()
                .get("x-robots-tag")
                .and_then(|value| value.to_str().ok()),
            Some("noindex, nofollow, noarchive")
        );
    }

    #[test]
    fn public_rule_uses_api_field_names() {
        let rule = public_rule(&NotificationRuleSnapshot::EarthquakeWarning {
            sources: NotificationSourcesSnapshot::All,
            intensity_bands: vec![crate::delivery::NotificationIntensityBandSnapshot {
                min: 3,
                max: 7,
                interruption_level: InterruptionLevel::Critical,
            }],
        });
        let value = serde_json::to_value(rule).expect("rule should serialize");
        assert_eq!(value["category"], "earthquake_warning");
        assert_eq!(value["sources"]["mode"], "all");
        assert!(value.get("k").is_none());
        assert!(value.get("s").is_none());
        assert!(value.get("b").is_none());
    }

    #[test]
    fn detail_error_pages_are_json_and_private() {
        for (response, status, message) in [
            (
                detail_not_found(),
                StatusCode::NOT_FOUND,
                "无法打开灾害详情",
            ),
            (
                detail_error(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "灾害详情加载失败",
            ),
            (
                detail_unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "灾害详情暂不可用",
            ),
        ] {
            assert_eq!(response.status(), status);
            assert_private_headers(&response);
            let mut headers = axum::http::HeaderMap::new();
            apply_detail_headers(&mut headers);
            assert_eq!(
                headers
                    .get(header::CACHE_CONTROL)
                    .map(|value| value.as_bytes()),
                Some(b"private, no-store".as_slice())
            );
            let _ = message;
        }
    }

    #[tokio::test]
    async fn invalid_incident_id_is_not_found() {
        let harness = harness().expect("harness");
        let response = incident_detail_handler(
            State(harness.state.clone()),
            Path(("bad-id".to_string(), "token".to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_private_headers(&response);
        let body = json_body(response).await.expect("json");
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "无法打开灾害详情");
        assert!(body.get("data").is_none());
    }

    #[tokio::test]
    async fn invalid_token_is_not_found() {
        let harness = harness().expect("harness");
        let incident_id = IncidentId::derive("source:event");
        let response = incident_detail_handler(
            State(harness.state.clone()),
            Path((incident_id.as_str().to_string(), "not.a.token".to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = json_body(response).await.expect("json");
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "无法打开灾害详情");
    }

    #[tokio::test]
    async fn detail_overloaded_returns_unavailable() {
        let mut harness = harness().expect("harness");
        harness.state.detail_concurrency = Arc::new(Semaphore::new(0));
        let incident_id = IncidentId::derive("source:event");
        let response = incident_detail_handler(
            State(harness.state.clone()),
            Path((incident_id.as_str().to_string(), "token".to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_private_headers(&response);
        let body = json_body(response).await.expect("json");
        assert_eq!(body["message"], "灾害详情暂不可用");
    }

    #[tokio::test]
    async fn valid_link_returns_public_json_without_short_keys() -> anyhow::Result<()> {
        let harness = harness()?;
        let event = event("source");
        let incident_id = IncidentId::derive(&event.event_key());
        let prepared =
            harness
                .state
                .notification_links
                .prepare_url_for(NotificationContextInput {
                    incident_id: &incident_id,
                    event: &event,
                    target: &target(),
                    timing: None,
                    interruption_level: InterruptionLevel::Critical.as_str(),
                    matched_rule: &AlertRule::default_for(DisasterCategory::EarthquakeWarning),
                    issued_at_ms: 1_700_000_000_000,
                })?;
        harness
            .state
            .notification_links
            .persist_prepared(&prepared)?;
        let token = prepared.url.rsplit('/').next().context("token in url")?;

        let mut incident = IncidentRecord::new(incident_id.clone(), &event, 1);
        incident.has_matched_subscribers = true;
        harness
            .state
            .storage
            .inner()
            .commit_incident_without_match(&incident, &event, 0)?;

        let response = incident_detail_handler(
            State(harness.state.clone()),
            Path((incident_id.as_str().to_string(), token.to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private_headers(&response);
        let body = json_body(response).await?;
        assert_eq!(body["success"], true);
        let data = body["data"].as_object().context("data object")?;
        assert!(data.get("v").is_none());
        assert!(data.get("e").is_none());
        assert!(data.get("t").is_none());
        assert_eq!(data["snapshot"]["incident_id"], incident_id.as_str());
        assert_eq!(
            data["snapshot"]["event"]["title"],
            "<script>alert(1)</script>"
        );
        assert_eq!(data["snapshot"]["target"]["label"], "<住所>");
        assert_eq!(
            data["snapshot"]["matched_rule"]["category"],
            "earthquake_warning"
        );
        assert!(data["incident"]["has_matched_subscribers"].is_null());
        assert!(data["incident"].get("has_matched_subscribers").is_none());
        assert!(data["incident"].get("pending_match_jobs").is_none());
        assert_eq!(data["incident"]["latest_by_source"][0]["source"], "source");
        assert!(
            data["incident"]["latest_by_source"][0]
                .get("channel")
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_incident_keeps_snapshot() -> anyhow::Result<()> {
        let harness = harness()?;
        let event = event("source");
        let incident_id = IncidentId::derive(&event.event_key());
        let prepared =
            harness
                .state
                .notification_links
                .prepare_url_for(NotificationContextInput {
                    incident_id: &incident_id,
                    event: &event,
                    target: &target(),
                    timing: None,
                    interruption_level: "active",
                    matched_rule: &AlertRule::default_for(DisasterCategory::EarthquakeWarning),
                    issued_at_ms: 1,
                })?;
        harness
            .state
            .notification_links
            .persist_prepared(&prepared)?;
        let token = prepared.url.rsplit('/').next().context("token")?;
        let response = incident_detail_handler(
            State(harness.state.clone()),
            Path((incident_id.as_str().to_string(), token.to_string())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await?;
        assert_eq!(body["data"]["snapshot"]["event"]["source"], "source");
        assert!(body["data"]["incident"].is_null());
        Ok(())
    }
}
