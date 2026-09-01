use crate::models::{ApiResponse, Subscription, mask_device_key};
use crate::routes::{AppState, validate_device_key};
use crate::simulate::{
    DeviceListError, HISTORY_SOURCE_MAJOR, HistoryRecordView, SimulateMode, SimulateOutcome,
    builtin_major_records, dispatch, find_history_record, history_views, parse_notify_level,
    resolve_device_keys,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

const AUTH_FAILED_MESSAGE: &str = "Bark Key 验证失败";

#[derive(Debug, Deserialize)]
pub(crate) struct SimulateQuery {
    notify_level: Option<String>,
    source: Option<String>,
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulateBody {
    #[serde(rename = "device_ID_list")]
    device_id_list: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HistoryQuery {
    source: Option<String>,
}

#[derive(Debug, Serialize)]
struct SimulateData {
    event_id: String,
    pushed: u32,
    skipped: u32,
    temporary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    notify_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

#[derive(Debug, Serialize)]
struct HistoryData {
    source: String,
    records: Vec<HistoryRecordView>,
}

pub(crate) async fn simulate_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<SimulateQuery>, QueryRejection>,
    body: Bytes,
) -> impl IntoResponse {
    if let Err((status, message)) = crate::routes::bff_auth::require_bff_service_token(
        &headers,
        state.bff_service_token.expose(),
    ) {
        return (status, Json(ApiResponse::<SimulateData>::error(message)));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error("查询参数无效")),
            );
        }
    };
    let mode = match parse_simulate_mode(&query) {
        Ok(mode) => mode,
        Err((status, message)) => {
            return (status, Json(ApiResponse::<SimulateData>::error(message)));
        }
    };
    let device_list = match parse_simulate_body(&body) {
        Ok(value) => value,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error(message)),
            );
        }
    };
    let device_keys = match resolve_device_keys(device_list) {
        Ok(keys) => keys,
        Err(DeviceListError::Missing) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error("需要 device_ID_list")),
            );
        }
        Err(DeviceListError::Empty) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error(
                    "device_ID_list 不能为空",
                )),
            );
        }
        Err(DeviceListError::TooMany) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error(
                    "device_ID_list 最多 32 个",
                )),
            );
        }
        Err(DeviceListError::InvalidKey) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SimulateData>::error(
                    "device_ID_list 含有无效 Bark Key",
                )),
            );
        }
    };
    let log_device_key = device_keys.first().map(String::as_str).unwrap_or_default();

    match dispatch(
        &state.subscriptions,
        &state.bark_notifier,
        &state.notification_links,
        state.p_wave_km_s,
        state.s_wave_km_s,
        &device_keys,
        &mode,
    )
    .await
    {
        Ok(outcome) => {
            let message = success_message(&mode, device_keys.len());
            (
                StatusCode::OK,
                Json(ApiResponse::success(message, Some(simulate_data(outcome)))),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "simulate.dispatch_failed",
                device_key = %mask_device_key(log_device_key),
                error = ?error,
                "simulate.dispatch_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<SimulateData>::error("模拟推送暂时无法完成")),
            )
        }
    }
}

pub(crate) async fn history_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> impl IntoResponse {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<HistoryData>::error("查询参数无效")),
            );
        }
    };
    let source = nonempty(&query.source);
    if source.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<HistoryData>::error(
                "请指定历史目录 source=major",
            )),
        );
    }
    if !source.is_some_and(|value| value.eq_ignore_ascii_case(HISTORY_SOURCE_MAJOR)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<HistoryData>::error("不支持的历史目录")),
        );
    }
    let subscription = match optional_bearer(&headers) {
        Ok(None) => None,
        Ok(Some(device_key)) => match lookup_subscriptions(&state, &device_key).await {
            Ok(mut subscriptions) => subscriptions.pop(),
            Err(error) => {
                tracing::error!(
                    event = "history.subscription_lookup_failed",
                    device_key = %mask_device_key(&device_key),
                    error = ?error,
                    "history.subscription_lookup_failed"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse::<HistoryData>::error("历史目录暂时无法获取")),
                );
            }
        },
        Err((status, message)) => {
            return (status, Json(ApiResponse::<HistoryData>::error(message)));
        }
    };
    let records = history_views(builtin_major_records(), subscription.as_ref());
    (
        StatusCode::OK,
        Json(ApiResponse::success(
            "历史地震目录获取成功",
            Some(HistoryData {
                source: HISTORY_SOURCE_MAJOR.to_string(),
                records,
            }),
        )),
    )
}

fn parse_simulate_mode(
    query: &SimulateQuery,
) -> std::result::Result<SimulateMode, (StatusCode, String)> {
    let notify_level = nonempty(&query.notify_level);
    let source = nonempty(&query.source);
    let key = nonempty(&query.key);
    match (notify_level, source, key) {
        (Some(level), None, None) => parse_notify_level(level)
            .map(SimulateMode::NotifyLevel)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "notify_level 无效".to_string())),
        (None, Some(source), Some(key)) => {
            if !source.eq_ignore_ascii_case(HISTORY_SOURCE_MAJOR) {
                return Err((StatusCode::BAD_REQUEST, "不支持的历史目录".to_string()));
            }
            find_history_record(builtin_major_records(), HISTORY_SOURCE_MAJOR, key)
                .ok_or_else(|| (StatusCode::NOT_FOUND, "未找到该历史地震".to_string()))?;
            Ok(SimulateMode::History {
                source: HISTORY_SOURCE_MAJOR.to_string(),
                key: key.to_string(),
            })
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err((
            StatusCode::BAD_REQUEST,
            "notify_level 与 source、key 不能同时使用".to_string(),
        )),
        (None, Some(_), None) | (None, None, Some(_)) => Err((
            StatusCode::BAD_REQUEST,
            "历史回放需要同时提供 source 与 key".to_string(),
        )),
        (None, None, None) => Err((
            StatusCode::BAD_REQUEST,
            "请指定 notify_level 或 source 与 key".to_string(),
        )),
    }
}

fn parse_simulate_body(body: &Bytes) -> std::result::Result<Option<Vec<String>>, &'static str> {
    if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let parsed = serde_json::from_slice::<SimulateBody>(body).map_err(|_error| "请求体无效")?;
    Ok(parsed.device_id_list)
}

fn optional_bearer(
    headers: &HeaderMap,
) -> std::result::Result<Option<String>, (StatusCode, String)> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Ok(None);
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
        Ok(device_key) => Ok(Some(device_key)),
        Err(_) => Err((StatusCode::UNAUTHORIZED, AUTH_FAILED_MESSAGE.to_string())),
    }
}

async fn lookup_subscriptions(
    state: &AppState,
    device_key: &str,
) -> anyhow::Result<Vec<Subscription>> {
    let manager = state.subscriptions.clone();
    let lookup_key = device_key.to_string();
    tokio::task::spawn_blocking(move || manager.simulate_subscriptions_by_device_key(&lookup_key))
        .await
        .map_err(anyhow::Error::from)?
}

fn success_message(mode: &SimulateMode, target_count: usize) -> &'static str {
    match mode {
        SimulateMode::History { .. } => "已使用历史真实地震数据发送测试预警",
        SimulateMode::NotifyLevel(_) if target_count > 1 => "已发送模拟预警",
        SimulateMode::NotifyLevel(_) => "已向当前 Bark Key 发送模拟预警",
    }
}

fn simulate_data(outcome: SimulateOutcome) -> SimulateData {
    SimulateData {
        event_id: outcome.event_id,
        pushed: outcome.pushed,
        skipped: outcome.skipped,
        temporary: outcome.temporary,
        notify_level: outcome.notify_level,
        source: outcome.source,
        key: outcome.key,
    }
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryQuery, SimulateQuery, history_handler, optional_bearer, parse_simulate_body,
        parse_simulate_mode, simulate_handler,
    };
    use crate::delivery::{BarkNotifier, BarkPushConfig, NotificationLinkService};
    use crate::models::{
        AlertRule, DisasterCategory, GeoPoint, MonitoringTarget, NotificationDestination,
        Subscription,
    };
    use crate::routes::{AppState, ReverseGeocoder, incident_detail_handler};
    use crate::runtime::RuntimeStatus;
    use crate::simulate::SimulateMode;
    use crate::storage::Storage;
    use crate::subscriptions::SubscriptionConfirmationService;
    use anyhow::Context;
    use axum::{
        body::{Bytes, to_bytes},
        extract::{Path, Query, State},
        http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    struct Harness {
        state: AppState,
        bark_base_url: String,
        captured: Arc<Mutex<Vec<Value>>>,
        _directory: tempfile::TempDir,
        _server: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    }

    fn subscription(
        device_key: &str,
        base_url: &str,
        latitude: f64,
        longitude: f64,
    ) -> Subscription {
        Subscription::new(
            NotificationDestination::Bark {
                base_url: base_url.to_string(),
                device_key: device_key.to_string(),
            },
            vec![MonitoringTarget {
                label: "监测点".to_string(),
                point: GeoPoint {
                    latitude,
                    longitude,
                },
                region: Default::default(),
            }],
            vec![AlertRule::default_for(DisasterCategory::EarthquakeWarning)],
        )
    }

    async fn start_harness() -> anyhow::Result<Harness> {
        async fn capture(
            axum::extract::State(captured): axum::extract::State<Arc<Mutex<Vec<Value>>>>,
            axum::Json(payload): axum::Json<Value>,
        ) -> axum::Json<Value> {
            if let Ok(mut payloads) = captured.lock() {
                payloads.push(payload.clone());
            }
            axum::Json(serde_json::json!({ "code": 200 }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app = axum::Router::new()
            .route("/push", axum::routing::post(capture))
            .with_state(captured.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let base_url = format!("http://{address}");
        let directory = tempfile::tempdir()?;
        let storage = Storage::open(directory.path())?;
        let notifier = BarkNotifier::new(
            vec![base_url.clone()],
            2,
            4,
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
            4,
        );
        state.subscriptions.upsert_subscription(subscription(
            "targetKey",
            &base_url,
            30.5954,
            104.0982,
        ))?;
        state
            .subscriptions
            .upsert_subscription(subscription("otherKey", &base_url, 31.2304, 121.4737))?;
        Ok(Harness {
            state,
            bark_base_url: base_url,
            captured,
            _directory: directory,
            _server: server,
        })
    }

    fn bff_headers() -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str("Bearer test-bff-service-token")?,
        );
        Ok(headers)
    }

    fn device_list_body(keys: &[&str]) -> Bytes {
        Bytes::from(serde_json::to_vec(&serde_json::json!({ "device_ID_list": keys })).unwrap_or_default())
    }

    fn bearer_headers(device_key: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {device_key}"))?,
        );
        Ok(headers)
    }

    async fn json_body(response: axum::response::Response) -> anyhow::Result<(StatusCode, Value)> {
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await?;
        Ok((status, serde_json::from_slice(&body)?))
    }

    fn captured_keys(harness: &Harness) -> anyhow::Result<Vec<String>> {
        harness
            .captured
            .lock()
            .map(|payloads| {
                payloads
                    .iter()
                    .filter_map(|payload| {
                        payload
                            .get("device_key")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .map_err(|error| anyhow::anyhow!("capture lock poisoned: {error}"))
    }

    fn captured_detail_url(harness: &Harness) -> anyhow::Result<String> {
        let payloads = harness
            .captured
            .lock()
            .map_err(|error| anyhow::anyhow!("capture lock poisoned: {error}"))?;
        payloads
            .last()
            .and_then(|payload| payload.get("url").and_then(Value::as_str))
            .map(str::to_string)
            .context("Bark payload missing detail url")
    }

    #[test]
    fn missing_authorization_is_a_bark_key_failure() {
        assert_eq!(optional_bearer(&HeaderMap::new()).ok(), Some(None));
    }

    #[test]
    fn empty_device_list_body_is_rejected_before_dispatch() {
        let body = Bytes::from_static(br#"{"device_ID_list":[]}"#);
        let parsed = parse_simulate_body(&body);
        assert_eq!(parsed.ok(), Some(Some(Vec::new())));
    }

    #[test]
    fn simulate_modes_are_mutually_exclusive() {
        let both = parse_simulate_mode(&SimulateQuery {
            notify_level: Some("active".to_string()),
            source: Some("major".to_string()),
            key: Some("yibin-gaoxian-2026".to_string()),
        });
        assert!(both.is_err());
        let neither = parse_simulate_mode(&SimulateQuery {
            notify_level: None,
            source: None,
            key: None,
        });
        assert!(neither.is_err());
        let history = parse_simulate_mode(&SimulateQuery {
            notify_level: None,
            source: Some("major".to_string()),
            key: Some("yibin-gaoxian-2026".to_string()),
        });
        assert!(matches!(
            history,
            Ok(SimulateMode::History { ref key, .. }) if key == "yibin-gaoxian-2026"
        ));
        let missing = parse_simulate_mode(&SimulateQuery {
            notify_level: None,
            source: Some("major".to_string()),
            key: Some("not-a-real-quake".to_string()),
        });
        assert_eq!(
            missing.err(),
            Some((StatusCode::NOT_FOUND, "未找到该历史地震".to_string()))
        );
        let unsupported = parse_simulate_mode(&SimulateQuery {
            notify_level: None,
            source: Some("cenc".to_string()),
            key: Some("No1".to_string()),
        });
        assert_eq!(
            unsupported.err(),
            Some((StatusCode::BAD_REQUEST, "不支持的历史目录".to_string()))
        );
    }

    #[tokio::test]
    async fn unknown_listed_key_is_skipped() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["unknownKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["data"]["pushed"], 0);
        assert_eq!(body["data"]["skipped"], 1);
        assert!(captured_keys(&harness)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bearer_key_matches_stored_subscription_case_insensitively() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["TARGETKEY"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK, "body {body}");
        assert_eq!(body["data"]["pushed"], 1);
        assert_eq!(captured_keys(&harness)?, vec!["targetKey".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn pending_confirmation_can_receive_simulate_push() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        harness.state.subscriptions.begin_confirmation(
            subscription("pendingKey", &harness.bark_base_url, 30.5954, 104.0982),
            crate::storage::try_now_millis()?,
            60_000,
        )?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["pendingKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK, "body {body}");
        assert_eq!(body["data"]["pushed"], 1);
        assert_eq!(captured_keys(&harness)?, vec!["pendingKey".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn simulate_without_bearer_fails() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            HeaderMap::new(),
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            Bytes::new(),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "服务凭证无效");
        assert!(captured_keys(&harness)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn simulate_with_bark_bearer_fails() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bearer_headers("targetKey")?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["targetKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "服务凭证无效");
        assert!(captured_keys(&harness)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn simulate_without_device_list_is_bad_request() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            Bytes::new(),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["message"], "需要 device_ID_list");
        assert!(captured_keys(&harness)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn empty_device_list_is_bad_request() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            Bytes::from_static(br#"{"device_ID_list":[]}"#),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["message"], "device_ID_list 不能为空");
        Ok(())
    }

    #[tokio::test]
    async fn unknown_history_key_is_not_found() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: None,
                source: Some("major".to_string()),
                key: Some("does-not-exist".to_string()),
            })),
            device_list_body(&["targetKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["message"], "未找到该历史地震");
        Ok(())
    }

    #[tokio::test]
    async fn bearer_only_pushes_the_current_key() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["targetKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["message"], "已向当前 Bark Key 发送模拟预警");
        assert_eq!(body["data"]["pushed"], 1);
        assert_eq!(body["data"]["skipped"], 0);
        assert_eq!(body["data"]["temporary"], false);
        assert_eq!(body["data"]["notify_level"], "active");
        let event_id = body["data"]["event_id"]
            .as_str()
            .context("event_id")?
            .to_string();
        assert!(event_id.starts_with("SIM-"), "event_id {event_id}");
        assert_eq!(captured_keys(&harness)?, vec!["targetKey".to_string()]);
        let backlog = harness.state.storage.backlog_counts()?;
        assert_eq!(backlog.inbox, 0);
        assert_eq!(backlog.match_jobs, 0);
        assert_eq!(backlog.delivery_batches, 0);
        Ok(())
    }

    #[tokio::test]
    async fn device_list_does_not_fan_out_to_other_subscribers() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("critical".to_string()),
                source: None,
                key: None,
            })),
            Bytes::from_static(br#"{"device_ID_list":["targetKey","missingKey"]}"#),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "已发送模拟预警");
        assert_eq!(body["data"]["pushed"], 1);
        assert_eq!(body["data"]["skipped"], 1);
        assert_eq!(captured_keys(&harness)?, vec!["targetKey".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn history_replay_uses_catalog_event_id() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: None,
                source: Some("major".to_string()),
                key: Some("yibin-gaoxian-2026".to_string()),
            })),
            device_list_body(&["targetKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "已使用历史真实地震数据发送测试预警");
        assert_eq!(body["data"]["pushed"], 1);
        assert_eq!(body["data"]["source"], "major");
        assert_eq!(body["data"]["key"], "yibin-gaoxian-2026");
        let event_id = body["data"]["event_id"]
            .as_str()
            .context("event_id")?
            .to_string();
        assert!(
            event_id.starts_with("HIST-MAJOR-CENC-202606290012-"),
            "event_id {event_id}"
        );
        assert_eq!(captured_keys(&harness)?, vec!["targetKey".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn history_catalog_is_public_and_can_annotate_a_subscription() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let public = history_handler(
            State(harness.state.clone()),
            HeaderMap::new(),
            Ok(Query(HistoryQuery {
                source: Some("major".to_string()),
            })),
        )
        .await
        .into_response();
        let (status, body) = json_body(public).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["records"].as_array().map(Vec::len), Some(3));
        assert!(body["data"]["records"][0]["distance_km"].is_null());

        let annotated = history_handler(
            State(harness.state.clone()),
            bearer_headers("targetKey")?,
            Ok(Query(HistoryQuery {
                source: Some("major".to_string()),
            })),
        )
        .await
        .into_response();
        let (status, body) = json_body(annotated).await?;
        assert_eq!(status, StatusCode::OK);
        let distance = body["data"]["records"][0]["distance_km"]
            .as_f64()
            .context("distance")?;
        assert!((230.0..250.0).contains(&distance), "distance {distance}");
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_history_source_is_rejected() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = history_handler(
            State(harness.state.clone()),
            HeaderMap::new(),
            Ok(Query(HistoryQuery {
                source: Some("cenc".to_string()),
            })),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["message"], "不支持的历史目录");
        Ok(())
    }

    #[tokio::test]
    async fn simulate_push_detail_link_loads_snapshot_without_incident() -> anyhow::Result<()> {
        let harness = start_harness().await?;
        let response = simulate_handler(
            State(harness.state.clone()),
            bff_headers()?,
            Ok(Query(SimulateQuery {
                notify_level: Some("active".to_string()),
                source: None,
                key: None,
            })),
            device_list_body(&["targetKey"]),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await?;
        assert_eq!(status, StatusCode::OK, "body {body}");
        assert_eq!(body["data"]["pushed"], 1);
        let url = captured_detail_url(&harness)?;
        let path = url
            .rsplit_once("/incidents/")
            .map(|(_, rest)| rest)
            .context("incident path")?;
        let (incident_id, token) = path.split_once("/notifications/").context("token path")?;
        let detail = incident_detail_handler(
            State(harness.state.clone()),
            Path((incident_id.to_string(), token.to_string())),
        )
        .await;
        assert_eq!(detail.status(), StatusCode::OK);
        let detail_body = json_body(detail).await?.1;
        assert_eq!(detail_body["success"], true);
        assert!(
            detail_body["data"]["snapshot"]["event"]["title"]
                .as_str()
                .is_some_and(|title| title.contains("模拟")),
            "title {}",
            detail_body["data"]["snapshot"]["event"]["title"]
        );
        assert!(detail_body["data"]["incident"].is_null());
        Ok(())
    }
}
