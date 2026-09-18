use crate::models::{
    ApiResponse, DisasterCategory, DisasterEvent, IncidentRecord, IncidentReportSummary,
    ProviderChannel,
};
use crate::routes::AppState;
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: u16 = 50;
const MAX_LIMIT: u16 = 200;

#[derive(Deserialize)]
pub(crate) struct AdminEventQuery {
    limit: Option<u16>,
}

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<AdminIncidentView>,
}

#[derive(Serialize)]
struct AdminIncidentView {
    incident_id: String,
    category: DisasterCategory,
    first_seen_at_ms: i64,
    updated_at_ms: i64,
    has_matched_subscribers: bool,
    latest: Vec<AdminEventView>,
    timeline: Vec<IncidentReportSummary>,
}

#[derive(Serialize)]
struct AdminEventView {
    category: DisasterCategory,
    channel: ProviderChannel,
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

impl From<IncidentRecord> for AdminIncidentView {
    fn from(incident: IncidentRecord) -> Self {
        Self {
            incident_id: incident.id.as_str().to_string(),
            category: incident.category,
            first_seen_at_ms: incident.first_seen_at_ms,
            updated_at_ms: incident.updated_at_ms,
            has_matched_subscribers: incident.has_matched_subscribers,
            latest: incident
                .latest_by_source
                .into_iter()
                .map(AdminEventView::from)
                .collect(),
            timeline: incident.timeline.into_iter().collect(),
        }
    }
}

impl From<DisasterEvent> for AdminEventView {
    fn from(event: DisasterEvent) -> Self {
        Self {
            category: event.category,
            channel: event.channel,
            source: event.source,
            event_id: event.event_id,
            revision: event.revision,
            report_num: event.report_num,
            title: event.title,
            description: event.description,
            affected_regions: event.affected_regions,
            latitude: event.latitude,
            longitude: event.longitude,
            magnitude: event.magnitude,
            depth_km: event.depth_km,
            radius_km: event.radius_km,
            level: event.level,
            occurred_at: event.occurred_at,
            final_report: event.final_report,
            cancel: event.cancel,
            training: event.training,
        }
    }
}

pub(crate) async fn admin_events_handler(
    State(state): State<AppState>,
    query: Result<Query<AdminEventQuery>, QueryRejection>,
) -> impl IntoResponse {
    let limit = match parse_admin_event_query(query) {
        Ok(limit) => limit,
        Err((status, message)) => {
            return (status, Json(ApiResponse::<EventsResponse>::error(message)));
        }
    };
    tracing::info!(
        event = "event.admin_lookup_requested",
        limit,
        "event.admin_lookup_requested"
    );
    let Ok(permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<EventsResponse>::error(
                "订阅存储繁忙，请稍后重试",
            )),
        );
    };
    let storage = state.storage.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        storage.recent_incidents(limit)
    })
    .await;
    match result {
        Ok(Ok(incidents)) => (
            StatusCode::OK,
            Json(ApiResponse::success(
                "事件详情获取成功",
                Some(EventsResponse {
                    events: incidents.into_iter().map(AdminIncidentView::from).collect(),
                }),
            )),
        ),
        Ok(Err(error)) => {
            tracing::error!(
                event = "event.admin_lookup_failed",
                error = ?error,
                "event.admin_lookup_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<EventsResponse>::error("事件详情暂时无法获取")),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "event.admin_lookup_task_failed",
                error = ?error,
                "event.admin_lookup_task_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<EventsResponse>::error("事件详情暂时无法获取")),
            )
        }
    }
}

fn parse_admin_event_query(
    query: Result<Query<AdminEventQuery>, QueryRejection>,
) -> Result<usize, (StatusCode, String)> {
    let Query(query) =
        query.map_err(|_rejection| (StatusCode::BAD_REQUEST, "查询参数无效".to_string()))?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err((
            StatusCode::BAD_REQUEST,
            "limit 必须是 1 到 200 的整数".to_string(),
        ));
    }
    Ok(usize::from(limit))
}

#[cfg(test)]
mod tests {
    use super::{AdminEventQuery, admin_events_handler, parse_admin_event_query};
    use crate::delivery::{BarkNotifier, BarkPushConfig, NotificationLinkService};
    use crate::events::EventCoordinator;
    use crate::models::{DisasterCategory, DisasterEvent, ProviderChannel};
    use crate::routes::{AppState, ReverseGeocoder};
    use crate::runtime::RuntimeStatus;
    use crate::storage::Storage;
    use crate::subscriptions::SubscriptionConfirmationService;
    use anyhow::Context;
    use axum::{
        body::to_bytes,
        extract::{Query, State},
        http::StatusCode,
        response::IntoResponse,
    };
    use serde_json::Value;

    struct Harness {
        state: AppState,
        _directory: tempfile::TempDir,
    }

    fn harness() -> anyhow::Result<Harness> {
        let directory = tempfile::TempDir::new()?;
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

    fn report(
        event_id: &str,
        title: &str,
        latitude: f64,
        longitude: f64,
        occurred_at: &str,
    ) -> DisasterEvent {
        DisasterEvent {
            category: DisasterCategory::EarthquakeReport,
            channel: ProviderChannel::FanStudio,
            source: "fanstudio.cenc".to_string(),
            event_id: event_id.to_string(),
            revision: "1".to_string(),
            report_num: 1,
            title: title.to_string(),
            description: format!("{title} description"),
            latitude: Some(latitude),
            longitude: Some(longitude),
            magnitude: Some(5.5),
            depth_km: Some(10.0),
            affected_regions: vec!["四川".to_string()],
            radius_km: None,
            level: 2,
            occurred_at: occurred_at.to_string(),
            final_report: false,
            cancel: false,
            training: false,
        }
    }

    fn ingest(state: &AppState, event: DisasterEvent) -> anyhow::Result<()> {
        state
            .storage
            .inner()
            .ingest_with_cursor(event.channel, vec![event], None)?;
        EventCoordinator::new(state.storage.inner()).process_next()?;
        Ok(())
    }

    async fn json_body(response: axum::response::Response) -> anyhow::Result<Value> {
        let body = to_bytes(response.into_body(), 64 * 1024).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    #[tokio::test]
    async fn empty_store_returns_an_empty_event_list() -> anyhow::Result<()> {
        let harness = harness()?;
        let response = admin_events_handler(
            State(harness.state),
            Ok(Query(AdminEventQuery { limit: None })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == true);
        anyhow::ensure!(body["data"]["events"] == serde_json::json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn two_incidents_include_event_details_newest_first() -> anyhow::Result<()> {
        let harness = harness()?;
        ingest(
            &harness.state,
            report("older-quake", "older", 31.2, 121.5, "2026-01-01T00:00:00Z"),
        )?;
        std::thread::sleep(std::time::Duration::from_millis(2));
        ingest(
            &harness.state,
            report("newer-quake", "newer", 25.0, 80.0, "2026-06-01T00:00:00Z"),
        )?;

        let response = admin_events_handler(
            State(harness.state),
            Ok(Query(AdminEventQuery { limit: None })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        let events = body["data"]["events"]
            .as_array()
            .context("missing events")?;
        anyhow::ensure!(events.len() == 2);
        anyhow::ensure!(events[0]["latest"][0]["title"] == "newer");
        anyhow::ensure!(events[0]["latest"][0]["source"] == "fanstudio.cenc");
        anyhow::ensure!(events[0]["latest"][0]["latitude"] == 25.0);
        anyhow::ensure!(events[0]["latest"][0]["longitude"] == 80.0);
        anyhow::ensure!(events[0]["latest"][0]["description"] == "newer description");
        anyhow::ensure!(events[1]["latest"][0]["title"] == "older");
        Ok(())
    }

    #[tokio::test]
    async fn same_event_update_keeps_one_incident_and_grows_timeline() -> anyhow::Result<()> {
        let harness = harness()?;
        ingest(
            &harness.state,
            report("same-quake", "first", 35.0, 105.0, "2026-07-13T00:00:00Z"),
        )?;
        let mut update = report("same-quake", "second", 35.0, 105.0, "2026-07-13T00:00:00Z");
        update.report_num = 2;
        update.revision = "2".to_string();
        ingest(&harness.state, update)?;

        let response = admin_events_handler(
            State(harness.state.clone()),
            Ok(Query(AdminEventQuery { limit: Some(1) })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        let events = body["data"]["events"]
            .as_array()
            .context("missing events")?;
        anyhow::ensure!(events.len() == 1);
        anyhow::ensure!(events[0]["latest"][0]["title"] == "second");
        anyhow::ensure!(events[0]["latest"][0]["report_num"] == 2);
        anyhow::ensure!(
            events[0]["timeline"]
                .as_array()
                .context("missing timeline")?
                .len()
                == 2
        );
        Ok(())
    }

    #[test]
    fn invalid_limit_is_rejected() -> anyhow::Result<()> {
        let uri = axum::http::Uri::from_static("/api/admin/events?limit=0");
        let query = Query::<AdminEventQuery>::try_from_uri(&uri);
        let Err((status, message)) = parse_admin_event_query(query) else {
            anyhow::bail!("limit=0 should be rejected");
        };
        anyhow::ensure!(status == StatusCode::BAD_REQUEST);
        anyhow::ensure!(message == "limit 必须是 1 到 200 的整数");
        Ok(())
    }
}
