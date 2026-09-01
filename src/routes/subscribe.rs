use crate::config::normalize_bark_url;
use crate::delivery::{BarkNotifier, NotificationLinkService};
use crate::models::{
    ApiResponse, MonitoringTarget, NotificationDestination, SubscribeRequest, Subscription,
    UnsubscribeRequest, mask_device_key,
};
use crate::routes::{ReverseGeocodeResult, ReverseGeocoder};
use crate::runtime::{DurableBacklogSnapshot, RuntimeStatus, RuntimeStatusSnapshot};
use crate::source_registry::{CategoryOption, category_options};
use crate::storage::Storage;
use crate::subscriptions::{
    DeleteSubscriptionError, SubscriptionConfirmationOutcome, SubscriptionConfirmationService,
    SubscriptionManager,
};
use crate::utils::distance;
use axum::{
    Json,
    extract::{
        Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_LOCATIONS: usize = 3;
const MAX_LOCATION_NAME_CHARS: usize = 80;
const INSTANCE_TERMS_REQUIRED_MESSAGE: &str = "当前实例尚未确认部署责任，暂不接受新增或覆盖订阅";

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) instance_terms_accepted: bool,
    pub(crate) storage: Storage,
    pub(crate) subscriptions: SubscriptionManager,
    pub(crate) bark_notifier: BarkNotifier,
    bark_urls: Vec<String>,
    runtime_status: RuntimeStatus,
    reverse_geocoder: ReverseGeocoder,
    pub(crate) notification_links: NotificationLinkService,
    pub(crate) p_wave_km_s: f64,
    pub(crate) s_wave_km_s: f64,
    pub(crate) detail_concurrency: Arc<Semaphore>,
    status_concurrency: Arc<Semaphore>,
    pub(crate) storage_concurrency: Arc<Semaphore>,
    subscription_concurrency: Arc<Semaphore>,
    subscription_confirmations: SubscriptionConfirmationService,
    huania_enabled: bool,
    bff_service_token: crate::config::SecretString,
}

impl AppState {
    pub(crate) fn new(
        storage: Storage,
        bark_notifier: BarkNotifier,
        runtime_status: RuntimeStatus,
        reverse_geocoder: ReverseGeocoder,
        notification_links: NotificationLinkService,
        subscription_confirmations: SubscriptionConfirmationService,
        max_concurrent_notifications: usize,
    ) -> Self {
        let subscriptions = storage.subscription_manager();
        let bark_urls = bark_notifier.allowed_bark_urls();
        Self {
            instance_terms_accepted: false,
            storage,
            subscriptions,
            bark_notifier,
            bark_urls,
            runtime_status,
            reverse_geocoder,
            notification_links,
            p_wave_km_s: 6.0,
            s_wave_km_s: 3.5,
            detail_concurrency: Arc::new(Semaphore::new(
                max_concurrent_notifications
                    .saturating_mul(4)
                    .clamp(64, 4_096),
            )),
            status_concurrency: Arc::new(Semaphore::new(8)),
            storage_concurrency: Arc::new(Semaphore::new(32)),
            subscription_concurrency: Arc::new(Semaphore::new(16)),
            subscription_confirmations,
            huania_enabled: false,
            bff_service_token: crate::config::SecretString::new(
                "test-bff-service-token".to_string(),
            ),
        }
    }

    pub(crate) fn with_instance_terms_accepted(mut self, accepted: bool) -> Self {
        self.instance_terms_accepted = accepted;
        self
    }

    pub(crate) fn with_huania_enabled(mut self, enabled: bool) -> Self {
        self.huania_enabled = enabled;
        self
    }

    pub(crate) fn with_wave_speeds(mut self, p_wave_km_s: f64, s_wave_km_s: f64) -> Self {
        self.p_wave_km_s = p_wave_km_s;
        self.s_wave_km_s = s_wave_km_s;
        self
    }

    pub(crate) fn with_bff_service_token(mut self, token: crate::config::SecretString) -> Self {
        self.bff_service_token = token;
        self
    }
}

#[derive(Deserialize)]
pub(crate) struct ReverseGeocodeQuery {
    latitude: f64,
    longitude: f64,
}

pub(crate) async fn reverse_geocode_handler(
    State(state): State<AppState>,
    query: Result<Query<ReverseGeocodeQuery>, QueryRejection>,
) -> impl IntoResponse {
    let query = match parse_reverse_geocode_query(query) {
        Ok(query) => query,
        Err(response) => return (StatusCode::BAD_REQUEST, Json(response)),
    };
    if !distance::validate_coordinates(query.latitude, query.longitude) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<ReverseGeocodeResult>::error("坐标无效")),
        );
    }
    let started = Instant::now();
    match state
        .reverse_geocoder
        .resolve(query.latitude, query.longitude)
        .await
    {
        Ok(location) => (
            StatusCode::OK,
            Json(ApiResponse::success("区域信息解析成功", Some(location))),
        ),
        Err(error) => {
            tracing::warn!(
                event = "reverse_geocode.failed",
                latitude = query.latitude,
                longitude = query.longitude,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = ?error,
                "reverse_geocode.failed"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiResponse::<ReverseGeocodeResult>::error(
                    "区域信息暂时无法自动解析，请手动填写",
                )),
            )
        }
    }
}

fn parse_reverse_geocode_query(
    query: Result<Query<ReverseGeocodeQuery>, QueryRejection>,
) -> Result<ReverseGeocodeQuery, ApiResponse<ReverseGeocodeResult>> {
    query
        .map(|Query(query)| query)
        .map_err(|_| ApiResponse::error("坐标参数无效"))
}

pub(crate) async fn subscribe_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<SubscribeRequest>, JsonRejection>,
) -> impl IntoResponse {
    if let Err((status, message)) =
        crate::routes::bff_auth::require_bff_service_token(&headers, state.bff_service_token.expose())
    {
        return (
            status,
            Json(ApiResponse::<SubscribeResponse>::error(message)),
        );
    }
    if let Err(response) = require_subscription_creation_enabled(state.instance_terms_accepted) {
        return response;
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SubscribeResponse>::error("订阅请求体无效")),
            );
        }
    };
    let device_key = match validate_device_key(payload.destination.bark_device_key()) {
        Ok(value) => value,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<SubscribeResponse>::error(message)),
            );
        }
    };

    let bark_url = match normalize_bark_url(payload.destination.bark_base_url()) {
        Ok(value) => value,
        Err(_error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SubscribeResponse>::error("Bark URL 无效")),
            );
        }
    };
    if !state.bark_notifier.allows_bark_url(&bark_url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<SubscribeResponse>::error(
                "Bark URL 不在允许列表中",
            )),
        );
    }

    let targets = match normalize_targets(payload.targets) {
        Ok(targets) => targets,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<SubscribeResponse>::error(message)),
            );
        }
    };
    let subscription = Subscription::new(
        NotificationDestination::Bark {
            base_url: bark_url,
            device_key,
        },
        targets,
        payload.alerts,
    );
    if let Err(message) = subscription.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<SubscribeResponse>::error(message)),
        );
    }

    tracing::info!(
        event = "subscription.requested",
        device_key = %mask_device_key(subscription.device_key()),
        target_count = subscription.targets.len(),
        alert_count = subscription.alerts.len(),
        "subscription.requested"
    );
    let masked_device_key = mask_device_key(subscription.device_key());

    let Ok(request_permit) = try_acquire_subscription_slot(&state.subscription_concurrency) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<SubscribeResponse>::error(
                "订阅请求繁忙，请稍后重试",
            )),
        );
    };
    let Ok(database_permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<SubscribeResponse>::error(
                "订阅存储繁忙，请稍后重试",
            )),
        );
    };

    let confirmation = match state.subscription_confirmations.begin(subscription).await {
        Ok(confirmation) => confirmation,
        Err(error) => {
            tracing::error!(
                event = "subscription.operation_store_failed",
                device_key = %masked_device_key,
                error = ?error,
                "subscription.operation_store_failed"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<SubscribeResponse>::error(
                    "订阅暂时无法保存，请稍后重试",
                )),
            );
        }
    };
    drop(database_permit);
    let outcome = state.subscription_confirmations.attempt(confirmation).await;
    drop(request_permit);
    match outcome {
        Ok(SubscriptionConfirmationOutcome::Activated) => {
            tracing::info!(
                event = "subscription.request_completed",
                device_key = %masked_device_key,
                "subscription.request_completed"
            );
            (
                StatusCode::OK,
                Json(ApiResponse::success(
                    "订阅已保存，确认通知已发送",
                    Some(SubscribeResponse { saved: true }),
                )),
            )
        }
        Ok(SubscriptionConfirmationOutcome::Pending) => (
            StatusCode::ACCEPTED,
            Json(ApiResponse::success(
                "Bark 服务暂时不可用，订阅确认将在后台重试",
                Some(SubscribeResponse { saved: false }),
            )),
        ),
        Ok(SubscriptionConfirmationOutcome::Rejected) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiResponse::<SubscribeResponse>::error(
                "Bark 接收测试失败，请检查 Bark Key；订阅未激活",
            )),
        ),
        Ok(SubscriptionConfirmationOutcome::Superseded) => (
            StatusCode::CONFLICT,
            Json(ApiResponse::<SubscribeResponse>::error(
                "该 Bark 目标已有更新的订阅请求，请以最新请求为准",
            )),
        ),
        Err(error) => {
            tracing::error!(
                event = "subscription.request_failed",
                device_key = %masked_device_key,
                error = ?error,
                "subscription.request_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<SubscribeResponse>::error(
                    "订阅确认状态暂时无法更新，后台将自动恢复",
                )),
            )
        }
    }
}

fn require_subscription_creation_enabled(
    instance_terms_accepted: bool,
) -> std::result::Result<(), (StatusCode, Json<ApiResponse<SubscribeResponse>>)> {
    if instance_terms_accepted {
        Ok(())
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::error(INSTANCE_TERMS_REQUIRED_MESSAGE)),
        ))
    }
}

fn try_acquire_subscription_slot(
    concurrency: &Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
    Arc::clone(concurrency).try_acquire_owned()
}

#[derive(Serialize)]
pub(crate) struct SubscriptionOptionsResponse {
    pub(crate) categories: Vec<CategoryOption>,
}

pub(crate) async fn subscription_options_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    Json(ApiResponse::success(
        "订阅选项获取成功",
        Some(SubscriptionOptionsResponse {
            categories: category_options(state.huania_enabled),
        }),
    ))
}

pub(crate) async fn unsubscribe_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<UnsubscribeRequest>, JsonRejection>,
) -> impl IntoResponse {
    if let Err((status, message)) =
        crate::routes::bff_auth::require_bff_service_token(&headers, state.bff_service_token.expose())
    {
        return (status, Json(ApiResponse::<()>::error(message)));
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<()>::error("取消订阅请求体无效")),
            );
        }
    };
    let device_key = match validate_device_key(payload.destination.bark_device_key()) {
        Ok(value) => value,
        Err((status, message)) => {
            return (status, Json(ApiResponse::<()>::error(message)));
        }
    };
    let base_url = match normalize_bark_url(payload.destination.bark_base_url()) {
        Ok(value) if state.bark_notifier.allows_bark_url(&value) => value,
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<()>::error("Bark URL 不在允许列表中")),
            );
        }
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<()>::error("Bark URL 无效")),
            );
        }
    };
    let destination_id = crate::models::DestinationId {
        base_url,
        device_key,
    };

    tracing::info!(
        event = "subscription.delete_requested",
        device_key = %mask_device_key(&destination_id.device_key),
        "subscription.delete_requested"
    );

    let manager = state.subscriptions.clone();
    let destination_to_delete = destination_id.clone();
    let Ok(permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<()>::error("订阅存储繁忙，请稍后重试")),
        );
    };
    match run_store(permit, move || {
        manager.delete_subscription(&destination_to_delete)
    })
    .await
    {
        Ok(_) => {
            tracing::info!(
                event = "subscription.delete_completed",
                device_key = %mask_device_key(&destination_id.device_key),
                "subscription.delete_completed"
            );
            (
                StatusCode::OK,
                Json(ApiResponse::<()>::success("已取消订阅", None)),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "subscription.delete_failed",
                device_key = %mask_device_key(&destination_id.device_key),
                error = ?error,
                "subscription.delete_failed"
            );
            let status = match error {
                DeleteSubscriptionError::NotFound => StatusCode::NOT_FOUND,
                DeleteSubscriptionError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(ApiResponse::<()>::error(match status {
                    StatusCode::NOT_FOUND => "订阅不存在或已取消",
                    _ => "取消订阅暂时无法完成，请稍后重试",
                })),
            )
        }
    }
}

#[derive(Serialize)]
pub(crate) struct SubscribeResponse {
    pub(crate) saved: bool,
}

fn normalize_targets(mut targets: Vec<MonitoringTarget>) -> Result<Vec<MonitoringTarget>, String> {
    if targets.is_empty() {
        return Err("请至少添加一个有效监测地点".to_string());
    }
    if targets.len() > MAX_LOCATIONS {
        return Err(format!("监测地点最多 {MAX_LOCATIONS} 个"));
    }
    if targets.iter().any(|target| {
        !distance::validate_coordinates(target.point.latitude, target.point.longitude)
    }) {
        return Err("监测地点坐标无效".to_string());
    }
    for target in &mut targets {
        for (label, value) in [
            ("名称", &mut target.label),
            ("省级行政区", &mut target.region.province),
            ("城市", &mut target.region.city),
            ("区县", &mut target.region.district),
        ] {
            let trimmed = value.trim();
            if trimmed.chars().count() > MAX_LOCATION_NAME_CHARS {
                return Err(format!(
                    "监测地点{label}最多 {MAX_LOCATION_NAME_CHARS} 个字符"
                ));
            }
            *value = trimmed.to_string();
        }
    }
    Ok(targets)
}

pub(crate) fn validate_device_key(raw: &str) -> std::result::Result<String, (StatusCode, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Bark Key 不能为空".to_string()));
    }
    if trimmed.len() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Bark Key 过长（最大64字符）".to_string(),
        ));
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Bark Key 只能包含字母、数字".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

async fn run_store<P, F>(
    permit: P,
    operation: F,
) -> std::result::Result<(), DeleteSubscriptionError>
where
    P: Send + 'static,
    F: FnOnce() -> std::result::Result<(), DeleteSubscriptionError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| DeleteSubscriptionError::Storage(anyhow::Error::from(error)))?
}

#[derive(Serialize)]
pub(crate) struct BarkUrlsResponse {
    pub(crate) bark_urls: Vec<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    instance_terms_accepted: bool,
    total_subscriptions: usize,
    #[serde(flatten)]
    runtime: RuntimeStatusSnapshot,
}

pub(crate) async fn bark_urls_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(ApiResponse::success(
        "Bark URL 列表获取成功",
        Some(BarkUrlsResponse {
            bark_urls: state.bark_urls,
        }),
    ))
}

pub(crate) async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(ApiResponse::<()>::success("OK", None)))
}

pub(crate) async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let Ok(permit) = state.status_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::error("运行状态查询繁忙，请稍后重试")),
        );
    };
    let Ok(database_permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::error("运行状态查询繁忙，请稍后重试")),
        );
    };
    let storage = state.storage.clone();
    let subscriptions = state.subscriptions.clone();
    let status = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _database_permit = database_permit;
        let backlog = storage.backlog_counts()?;
        let durable = DurableBacklogSnapshot {
            inbox_pending: backlog.inbox,
            match_jobs_pending: backlog.match_jobs,
            delivery_batches_pending: backlog.delivery_batches,
            retries_pending: backlog.retries,
            subscription_confirmations_pending: subscriptions.pending_confirmation_count()?,
        };
        Ok::<_, anyhow::Error>((subscriptions.total_count()?, durable))
    })
    .await;
    match status {
        Ok(Ok((total_subscriptions, durable))) => (
            StatusCode::OK,
            Json(ApiResponse::success(
                "运行状态获取成功",
                Some(StatusResponse {
                    instance_terms_accepted: state.instance_terms_accepted,
                    total_subscriptions,
                    runtime: state.runtime_status.snapshot(durable, state.huania_enabled),
                }),
            )),
        ),
        Ok(Err(error)) => {
            tracing::error!(event = "status.load_failed", error = ?error, "status.load_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error("运行状态暂时无法获取")),
            )
        }
        Err(error) => {
            tracing::error!(event = "status.task_failed", error = ?error, "status.task_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error("运行状态暂时无法获取")),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{BarkNotifier, BarkPushConfig, NotificationLinkService};
    use crate::storage::Storage;
    use crate::subscriptions::SubscriptionConfirmationService;
    use axum::{
        body::to_bytes,
        http::{HeaderMap, HeaderValue, header::AUTHORIZATION},
        response::IntoResponse,
    };

    fn request() -> SubscribeRequest {
        SubscribeRequest {
            destination: NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: "abc123".to_string(),
            },
            targets: vec![MonitoringTarget {
                label: "home".to_string(),
                point: crate::models::GeoPoint {
                    latitude: 35.0,
                    longitude: 105.0,
                },
                region: crate::models::AdministrativeRegion::default(),
            }],
            alerts: vec![crate::models::AlertRule::default_for(
                crate::models::DisasterCategory::WeatherWarning,
            )],
        }
    }

    #[test]
    fn weather_only_subscription_does_not_require_intensity_bands() {
        let payload = request();
        let subscription = Subscription::new(payload.destination, payload.targets, payload.alerts);
        assert!(subscription.validate().is_ok());
    }

    #[test]
    fn administrative_fields_obey_location_length_limit() {
        let mut payload = request();
        payload.targets[0].region.province = "省".repeat(MAX_LOCATION_NAME_CHARS + 1);
        assert!(normalize_targets(payload.targets).is_err());
    }

    #[test]
    fn reverse_geocode_query_rejection_uses_the_api_envelope() {
        let uri = axum::http::Uri::from_static("/api/reverse-geocode?latitude=31.2");
        let query = Query::<ReverseGeocodeQuery>::try_from_uri(&uri);
        let response = match parse_reverse_geocode_query(query) {
            Ok(_) => panic!("missing longitude should be rejected"),
            Err(response) => response,
        };

        assert!(!response.success);
        assert_eq!(response.message, "坐标参数无效");
        assert!(response.data.is_none());
    }

    #[test]
    fn subscription_admission_rejects_before_external_work_when_saturated() {
        let concurrency = Arc::new(Semaphore::new(1));
        let held = try_acquire_subscription_slot(&concurrency);
        assert!(held.is_ok());
        assert!(try_acquire_subscription_slot(&concurrency).is_err());
        drop(held);
        assert!(try_acquire_subscription_slot(&concurrency).is_ok());
    }

    #[test]
    fn subscription_creation_requires_accepted_instance_terms() {
        assert!(require_subscription_creation_enabled(true).is_ok());

        let result = require_subscription_creation_enabled(false);
        assert!(result.is_err());
        if let Err((status, Json(response))) = result {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!response.success);
            assert_eq!(response.message, INSTANCE_TERMS_REQUIRED_MESSAGE);
            assert!(response.data.is_none());
        }
    }

    #[test]
    fn status_response_omits_huania_when_disabled() {
        let response = StatusResponse {
            instance_terms_accepted: true,
            total_subscriptions: 12,
            runtime: RuntimeStatus::default().snapshot(DurableBacklogSnapshot::default(), false),
        };
        let value = serde_json::to_value(response).expect("status response should serialize");

        assert_eq!(value["instance_terms_accepted"], true);
        assert_eq!(value["total_subscriptions"], 12);
        assert!(value.get("wolfx").is_some());
        assert!(value.get("fanstudio").is_some());
        assert!(value.get("huania").is_none());
        assert!(value.get("durable").is_some());
        assert!(value.get("ready_queues").is_some());
        assert!(value.get("runtime").is_none());
    }

    #[test]
    fn status_response_includes_huania_when_enabled() {
        let response = StatusResponse {
            instance_terms_accepted: true,
            total_subscriptions: 12,
            runtime: RuntimeStatus::default().snapshot(DurableBacklogSnapshot::default(), true),
        };
        let value = serde_json::to_value(response).expect("status response should serialize");

        assert!(value.get("wolfx").is_some());
        assert!(value.get("fanstudio").is_some());
        assert!(value.get("huania").is_some());
        assert!(value.get("runtime").is_none());
    }

    fn bff_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer test-bff-service-token"),
        );
        headers
    }

    fn bark_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123"));
        headers
    }

    fn test_state(terms_accepted: bool) -> anyhow::Result<(AppState, tempfile::TempDir)> {
        let directory = tempfile::tempdir()?;
        let storage = Storage::open(directory.path())?;
        let notifier = BarkNotifier::new(
            vec!["https://api.day.app".to_string()],
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
        )
        .with_instance_terms_accepted(terms_accepted);
        Ok((state, directory))
    }

    async fn json_body(
        response: axum::response::Response,
    ) -> anyhow::Result<(StatusCode, serde_json::Value)> {
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await?;
        Ok((status, serde_json::from_slice(&body)?))
    }

    #[tokio::test]
    async fn subscribe_without_service_token_is_unauthorized() -> anyhow::Result<()> {
        let (state, _directory) = test_state(true)?;
        let response = subscribe_handler(State(state), HeaderMap::new(), Ok(Json(request())))
            .await
            .into_response();
        let (status, body) = json_body(response).await?;
        anyhow::ensure!(status == StatusCode::UNAUTHORIZED);
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "服务凭证无效");
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_with_bark_bearer_is_unauthorized() -> anyhow::Result<()> {
        let (state, _directory) = test_state(true)?;
        let response = subscribe_handler(State(state), bark_headers(), Ok(Json(request())))
            .await
            .into_response();
        let (status, body) = json_body(response).await?;
        anyhow::ensure!(status == StatusCode::UNAUTHORIZED);
        anyhow::ensure!(body["message"] == "服务凭证无效");
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_checks_instance_terms_after_service_token() -> anyhow::Result<()> {
        let (state, _directory) = test_state(false)?;
        let response = subscribe_handler(State(state), bff_headers(), Ok(Json(request())))
            .await
            .into_response();
        let (status, body) = json_body(response).await?;
        anyhow::ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
        anyhow::ensure!(body["message"] == INSTANCE_TERMS_REQUIRED_MESSAGE);
        Ok(())
    }

    #[tokio::test]
    async fn unsubscribe_without_service_token_is_unauthorized() -> anyhow::Result<()> {
        let (state, _directory) = test_state(true)?;
        let payload = UnsubscribeRequest {
            destination: NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: "abc123".to_string(),
            },
        };
        let response =
            unsubscribe_handler(State(state), HeaderMap::new(), Ok(Json(payload)))
                .await
                .into_response();
        let (status, body) = json_body(response).await?;
        anyhow::ensure!(status == StatusCode::UNAUTHORIZED);
        anyhow::ensure!(body["message"] == "服务凭证无效");
        Ok(())
    }
}
