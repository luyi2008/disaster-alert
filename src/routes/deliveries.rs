use crate::models::{ApiResponse, DisasterCategory, InterruptionLevel, mask_device_key};
use crate::routes::{AppState, validate_device_key};
use crate::storage::DeviceDeliveryRecord;
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

const AUTH_FAILED_MESSAGE: &str = "Bark Key 验证失败";
const NOT_FOUND_MESSAGE: &str = "未找到该 Bark Key 的订阅";
const DEFAULT_LIMIT: u16 = 50;
const MAX_LIMIT: u16 = 200;
const DEFAULT_ADMIN_ALL_LIMIT: u16 = 2_000;
const MAX_ADMIN_LIMIT: u16 = 2_000;

#[derive(Deserialize)]
pub(crate) struct DeliveryQuery {
    limit: Option<u16>,
}

#[derive(Deserialize)]
pub(crate) struct AdminDeliveryQuery {
    device_key: Option<String>,
    limit: Option<u16>,
}

#[derive(Serialize)]
struct DeliveriesResponse {
    deliveries: Vec<DeliveryView>,
}

#[derive(Serialize)]
struct DeliveryView {
    delivered_at_ms: i64,
    incident_id: String,
    category: DisasterCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bark_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    magnitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    occurred_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    longitude: Option<f64>,
    interruption_level: InterruptionLevel,
    distance_km: f64,
    estimated_intensity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_label: Option<String>,
}

impl From<DeviceDeliveryRecord> for DeliveryView {
    fn from(record: DeviceDeliveryRecord) -> Self {
        Self {
            delivered_at_ms: record.delivered_at_ms,
            incident_id: record.incident_id.as_str().to_string(),
            category: record.category,
            device_key: record.device_key,
            bark_base_url: record.bark_base_url,
            source: record.source,
            event_id: record.event_id,
            title: record.title,
            description: record.description,
            magnitude: record.magnitude,
            occurred_at: record.occurred_at,
            latitude: record.latitude,
            longitude: record.longitude,
            interruption_level: record.interruption_level,
            distance_km: record.distance_km,
            estimated_intensity: record.estimated_intensity,
            target_label: record.target_label,
        }
    }
}

pub(crate) async fn deliveries_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<DeliveryQuery>, QueryRejection>,
) -> impl IntoResponse {
    let device_key = match require_bearer(&headers) {
        Ok(device_key) => device_key,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<DeliveriesResponse>::error(message)),
            );
        }
    };
    let limit = match parse_delivery_query(query) {
        Ok(limit) => limit,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<DeliveriesResponse>::error(message)),
            );
        }
    };
    lookup_deliveries(state, Some(device_key), limit).await
}

pub(crate) async fn admin_deliveries_handler(
    State(state): State<AppState>,
    query: Result<Query<AdminDeliveryQuery>, QueryRejection>,
) -> impl IntoResponse {
    let (device_key, limit) = match parse_admin_delivery_query(query) {
        Ok(parsed) => parsed,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<DeliveriesResponse>::error(message)),
            );
        }
    };
    match device_key.as_deref() {
        Some(device_key) => tracing::info!(
            event = "delivery.admin_lookup_requested",
            device_key = %mask_device_key(device_key),
            limit,
            "delivery.admin_lookup_requested"
        ),
        None => tracing::info!(
            event = "delivery.admin_lookup_requested",
            list_all = true,
            limit,
            "delivery.admin_lookup_requested"
        ),
    }
    lookup_deliveries(state, device_key, limit).await
}

async fn lookup_deliveries(
    state: AppState,
    device_key: Option<String>,
    limit: usize,
) -> (StatusCode, Json<ApiResponse<DeliveriesResponse>>) {
    let Ok(permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<DeliveriesResponse>::error(
                "订阅存储繁忙，请稍后重试",
            )),
        );
    };
    let storage = state.storage.clone();
    let lookup_key = device_key.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        storage.deliveries_for_device_key(lookup_key.as_deref(), limit)
    })
    .await;
    match result {
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(ApiResponse::<DeliveriesResponse>::error(
                NOT_FOUND_MESSAGE.to_string(),
            )),
        ),
        Ok(Ok(Some(deliveries))) => (
            StatusCode::OK,
            Json(ApiResponse::success(
                "推送记录获取成功",
                Some(DeliveriesResponse {
                    deliveries: deliveries.into_iter().map(DeliveryView::from).collect(),
                }),
            )),
        ),
        Ok(Err(error)) => {
            tracing::error!(
                event = "delivery.lookup_failed",
                device_key = device_key.as_deref().map(mask_device_key),
                error = ?error,
                "delivery.lookup_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<DeliveriesResponse>::error(
                    "推送记录暂时无法获取",
                )),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "delivery.lookup_task_failed",
                device_key = device_key.as_deref().map(mask_device_key),
                error = ?error,
                "delivery.lookup_task_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<DeliveriesResponse>::error(
                    "推送记录暂时无法获取",
                )),
            )
        }
    }
}

fn parse_delivery_query(
    query: Result<Query<DeliveryQuery>, QueryRejection>,
) -> Result<usize, (StatusCode, String)> {
    let Query(query) =
        query.map_err(|_rejection| (StatusCode::BAD_REQUEST, "查询参数无效".to_string()))?;
    parse_limit(query.limit, DEFAULT_LIMIT, MAX_LIMIT)
}

fn parse_admin_delivery_query(
    query: Result<Query<AdminDeliveryQuery>, QueryRejection>,
) -> Result<(Option<String>, usize), (StatusCode, String)> {
    let Query(query) =
        query.map_err(|_rejection| (StatusCode::BAD_REQUEST, "查询参数无效".to_string()))?;
    let device_key = match query.device_key.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(device_key) => Some(validate_device_key(device_key)?),
    };
    let limit = parse_limit(
        query.limit,
        if device_key.is_none() {
            DEFAULT_ADMIN_ALL_LIMIT
        } else {
            DEFAULT_LIMIT
        },
        MAX_ADMIN_LIMIT,
    )?;
    Ok((device_key, limit))
}

fn parse_limit(limit: Option<u16>, default: u16, max: u16) -> Result<usize, (StatusCode, String)> {
    let limit = limit.unwrap_or(default);
    if limit == 0 || limit > max {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("limit 必须是 1 到 {max} 的整数"),
        ));
    }
    Ok(usize::from(limit))
}

fn require_bearer(headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string()));
    };
    let Ok(text) = value.to_str() else {
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string()));
    };
    let trimmed = text.trim();
    let Some((scheme, rest)) = trimmed.split_once(' ') else {
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string()));
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string()));
    }
    match validate_device_key(rest.trim()) {
        Ok(device_key) => Ok(device_key),
        Err(_) => Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdminDeliveryQuery, DEFAULT_ADMIN_ALL_LIMIT, DeliveryQuery, admin_deliveries_handler,
        deliveries_handler, parse_admin_delivery_query, parse_delivery_query,
    };
    use crate::delivery::{BarkNotifier, BarkPushConfig, DeliveryRow, NotificationLinkService};
    use crate::models::{
        AlertRule, DisasterCategory, GeoPoint, IncidentId, InterruptionLevel, MonitoringTarget,
        NotificationDestination, Subscription,
    };
    use crate::routes::{AppState, ReverseGeocoder};
    use crate::runtime::RuntimeStatus;
    use crate::storage::Storage;
    use crate::subscriptions::SubscriptionConfirmationService;
    use anyhow::Context;
    use axum::{
        body::to_bytes,
        extract::{Query, State},
        http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };
    use serde_json::Value;

    struct Harness {
        state: AppState,
        _directory: tempfile::TempDir,
    }

    fn subscription(device_key: &str) -> Subscription {
        Subscription::new(
            NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: device_key.to_string(),
            },
            vec![MonitoringTarget {
                label: "home".to_string(),
                point: GeoPoint {
                    latitude: 31.2,
                    longitude: 121.5,
                },
                region: Default::default(),
            }],
            vec![AlertRule::default_for(DisasterCategory::EarthquakeReport)],
        )
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

    async fn json_body(response: axum::response::Response) -> anyhow::Result<Value> {
        let body = to_bytes(response.into_body(), 64 * 1024).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    fn bearer_headers(device_key: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {device_key}"))?,
        );
        Ok(headers)
    }

    #[tokio::test]
    async fn bearer_lookup_returns_newest_delivery() -> anyhow::Result<()> {
        let harness = harness()?;
        let stored = harness
            .state
            .storage
            .inner()
            .store_subscription(subscription("abc123"))?;
        harness.state.storage.inner().put_ledger_delivery_for_test(
            &IncidentId::derive("device-delivery"),
            DisasterCategory::EarthquakeReport,
            1_700_000_000_000,
            1,
            DeliveryRow {
                destination_id: stored.destination_id,
                subscription_id: stored.id,
                generation: stored.generation,
                target_ordinal: 0,
                match_kind: 1,
                interruption_level: InterruptionLevel::Critical,
                distance_m: 12_300,
                intensity_cent: 350,
            },
        )?;

        let response = deliveries_handler(
            State(harness.state.clone()),
            bearer_headers("abc123")?,
            Ok(Query(DeliveryQuery { limit: None })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == true);
        anyhow::ensure!(body["message"] == "推送记录获取成功");
        let deliveries = body["data"]["deliveries"]
            .as_array()
            .context("missing deliveries")?;
        anyhow::ensure!(deliveries.len() == 1);
        anyhow::ensure!(deliveries[0]["category"] == "earthquake_report");
        anyhow::ensure!(deliveries[0]["interruption_level"] == "critical");
        anyhow::ensure!(deliveries[0]["distance_km"] == 12.3);
        anyhow::ensure!(deliveries[0]["estimated_intensity"] == 3.5);
        anyhow::ensure!(deliveries[0]["target_label"] == "home");
        anyhow::ensure!(deliveries[0]["device_key"] == "abc123");
        anyhow::ensure!(deliveries[0]["bark_base_url"] == "https://api.day.app");

        let unknown = deliveries_handler(
            State(harness.state),
            bearer_headers("missingkey")?,
            Ok(Query(DeliveryQuery { limit: None })),
        )
        .await
        .into_response();
        anyhow::ensure!(unknown.status() == StatusCode::NOT_FOUND);
        let unknown_body = json_body(unknown).await?;
        anyhow::ensure!(unknown_body["success"] == false);
        anyhow::ensure!(unknown_body["message"] == "未找到该 Bark Key 的订阅");
        Ok(())
    }

    #[tokio::test]
    async fn missing_bearer_is_unauthorized() -> anyhow::Result<()> {
        let harness = harness()?;
        let response = deliveries_handler(
            State(harness.state),
            HeaderMap::new(),
            Ok(Query(DeliveryQuery { limit: None })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::UNAUTHORIZED);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "Bark Key 验证失败");
        Ok(())
    }

    #[tokio::test]
    async fn admin_lookup_finds_inactive_subscription_history() -> anyhow::Result<()> {
        let harness = harness()?;
        let value = subscription("abc123");
        let stored = harness
            .state
            .storage
            .inner()
            .store_subscription(value.clone())?;
        harness.state.storage.inner().put_ledger_delivery_for_test(
            &IncidentId::derive("inactive-delivery"),
            DisasterCategory::EarthquakeWarning,
            50,
            1,
            DeliveryRow {
                destination_id: stored.destination_id,
                subscription_id: stored.id,
                generation: stored.generation,
                target_ordinal: 0,
                match_kind: 1,
                interruption_level: InterruptionLevel::Passive,
                distance_m: 500,
                intensity_cent: 100,
            },
        )?;
        harness
            .state
            .subscriptions
            .delete_subscription(&value.destination_id())?;

        let response = admin_deliveries_handler(
            State(harness.state),
            Ok(Query(AdminDeliveryQuery {
                device_key: Some("abc123".to_string()),
                limit: Some(10),
            })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == true);
        let deliveries = body["data"]["deliveries"]
            .as_array()
            .context("missing deliveries")?;
        anyhow::ensure!(deliveries.len() == 1);
        anyhow::ensure!(deliveries[0]["category"] == "earthquake_warning");
        anyhow::ensure!(deliveries[0]["device_key"] == "abc123");
        Ok(())
    }

    #[tokio::test]
    async fn admin_lookup_without_key_returns_all_deliveries() -> anyhow::Result<()> {
        let harness = harness()?;
        let first = harness
            .state
            .storage
            .inner()
            .store_subscription(subscription("abc123"))?;
        let second = harness
            .state
            .storage
            .inner()
            .store_subscription(subscription("otherkey"))?;
        harness.state.storage.inner().put_ledger_delivery_for_test(
            &IncidentId::derive("first-delivery"),
            DisasterCategory::EarthquakeReport,
            10,
            1,
            DeliveryRow {
                destination_id: first.destination_id,
                subscription_id: first.id,
                generation: first.generation,
                target_ordinal: 0,
                match_kind: 1,
                interruption_level: InterruptionLevel::Active,
                distance_m: 1_000,
                intensity_cent: 200,
            },
        )?;
        harness.state.storage.inner().put_ledger_delivery_for_test(
            &IncidentId::derive("second-delivery"),
            DisasterCategory::EarthquakeWarning,
            20,
            2,
            DeliveryRow {
                destination_id: second.destination_id,
                subscription_id: second.id,
                generation: second.generation,
                target_ordinal: 0,
                match_kind: 1,
                interruption_level: InterruptionLevel::Critical,
                distance_m: 3_000,
                intensity_cent: 400,
            },
        )?;

        let response = admin_deliveries_handler(
            State(harness.state),
            Ok(Query(AdminDeliveryQuery {
                device_key: None,
                limit: None,
            })),
        )
        .await
        .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == true);
        let deliveries = body["data"]["deliveries"]
            .as_array()
            .context("missing deliveries")?;
        anyhow::ensure!(deliveries.len() == 2);
        anyhow::ensure!(deliveries[0]["device_key"] == "otherkey");
        anyhow::ensure!(deliveries[0]["category"] == "earthquake_warning");
        anyhow::ensure!(deliveries[0]["title"].is_null());
        anyhow::ensure!(deliveries[1]["device_key"] == "abc123");
        Ok(())
    }

    #[test]
    fn invalid_limit_is_rejected_and_empty_admin_key_lists_all() -> anyhow::Result<()> {
        let uri = axum::http::Uri::from_static("/api/deliveries?limit=0");
        let query = Query::<DeliveryQuery>::try_from_uri(&uri);
        let Err((status, message)) = parse_delivery_query(query) else {
            anyhow::bail!("limit=0 should be rejected");
        };
        anyhow::ensure!(status == StatusCode::BAD_REQUEST);
        anyhow::ensure!(message == "limit 必须是 1 到 200 的整数");

        let uri = axum::http::Uri::from_static("/api/admin/deliveries");
        let query = Query::<AdminDeliveryQuery>::try_from_uri(&uri);
        let Ok((device_key, limit)) = parse_admin_delivery_query(query) else {
            anyhow::bail!("omitted admin key should list all deliveries");
        };
        anyhow::ensure!(device_key.is_none());
        anyhow::ensure!(limit == usize::from(DEFAULT_ADMIN_ALL_LIMIT));

        let Ok((empty_key, _)) = parse_admin_delivery_query(Ok(Query(AdminDeliveryQuery {
            device_key: Some(String::new()),
            limit: None,
        }))) else {
            anyhow::bail!("empty admin key should list all deliveries");
        };
        anyhow::ensure!(empty_key.is_none());
        Ok(())
    }
}
