use crate::models::{ApiResponse, Subscription, mask_device_key};
use crate::routes::{AppState, validate_device_key};
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

const AUTH_FAILED_MESSAGE: &str = "Bark Key 验证失败";

#[derive(Serialize)]
struct DeviceKeysResponse {
    device_keys: Vec<String>,
}

#[derive(Serialize)]
struct SubscriptionsResponse {
    subscriptions: Vec<Subscription>,
}

#[derive(Deserialize)]
pub(crate) struct AdminSubscriptionQuery {
    device_key: String,
}

pub(crate) async fn admin_device_keys_handler(State(state): State<AppState>) -> impl IntoResponse {
    let Ok(permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<DeviceKeysResponse>::error(
                "订阅存储繁忙，请稍后重试",
            )),
        );
    };
    let manager = state.subscriptions.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        manager.list_active_device_keys()
    })
    .await;
    match result {
        Ok(Ok(device_keys)) => (
            StatusCode::OK,
            Json(ApiResponse::success(
                "Bark Key 列表获取成功",
                Some(DeviceKeysResponse { device_keys }),
            )),
        ),
        Ok(Err(error)) => {
            tracing::error!(
                event = "subscription.admin_device_keys_failed",
                error = ?error,
                "subscription.admin_device_keys_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<DeviceKeysResponse>::error(
                    "订阅列表暂时无法获取",
                )),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "subscription.admin_device_keys_task_failed",
                error = ?error,
                "subscription.admin_device_keys_task_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<DeviceKeysResponse>::error(
                    "订阅列表暂时无法获取",
                )),
            )
        }
    }
}

pub(crate) async fn subscriptions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let device_key = match require_bearer(&headers) {
        Ok(device_key) => device_key,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<SubscriptionsResponse>::error(message)),
            );
        }
    };
    tracing::info!(
        event = "subscription.lookup_requested",
        device_key = %mask_device_key(&device_key),
        "subscription.lookup_requested"
    );
    lookup_subscriptions(state, device_key).await
}

pub(crate) async fn admin_subscriptions_handler(
    State(state): State<AppState>,
    query: Result<Query<AdminSubscriptionQuery>, QueryRejection>,
) -> impl IntoResponse {
    let device_key = match parse_admin_subscription_query(query) {
        Ok(device_key) => device_key,
        Err((status, message)) => {
            return (
                status,
                Json(ApiResponse::<SubscriptionsResponse>::error(message)),
            );
        }
    };
    tracing::info!(
        event = "subscription.admin_lookup_requested",
        device_key = %mask_device_key(&device_key),
        "subscription.admin_lookup_requested"
    );
    lookup_subscriptions(state, device_key).await
}

async fn lookup_subscriptions(
    state: AppState,
    device_key: String,
) -> (StatusCode, Json<ApiResponse<SubscriptionsResponse>>) {
    let Ok(permit) = state.storage_concurrency.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiResponse::<SubscriptionsResponse>::error(
                "订阅存储繁忙，请稍后重试",
            )),
        );
    };
    let manager = state.subscriptions.clone();
    let lookup_key = device_key.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        manager.active_subscriptions_by_device_key(&lookup_key)
    })
    .await;
    match result {
        Ok(Ok(subscriptions)) if subscriptions.is_empty() => (
            StatusCode::OK,
            Json(ApiResponse::<SubscriptionsResponse>::error(
                "订阅不存在或已取消",
            )),
        ),
        Ok(Ok(subscriptions)) => (
            StatusCode::OK,
            Json(ApiResponse::success(
                "订阅详情获取成功",
                Some(SubscriptionsResponse { subscriptions }),
            )),
        ),
        Ok(Err(error)) => {
            tracing::error!(
                event = "subscription.lookup_failed",
                device_key = %mask_device_key(&device_key),
                error = ?error,
                "subscription.lookup_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<SubscriptionsResponse>::error(
                    "订阅详情暂时无法获取",
                )),
            )
        }
        Err(error) => {
            tracing::error!(
                event = "subscription.lookup_task_failed",
                device_key = %mask_device_key(&device_key),
                error = ?error,
                "subscription.lookup_task_failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<SubscriptionsResponse>::error(
                    "订阅详情暂时无法获取",
                )),
            )
        }
    }
}

fn parse_admin_subscription_query(
    query: Result<Query<AdminSubscriptionQuery>, QueryRejection>,
) -> Result<String, (StatusCode, String)> {
    let Query(query) =
        query.map_err(|_rejection| (StatusCode::BAD_REQUEST, "Bark Key 不能为空".to_string()))?;
    validate_device_key(&query.device_key)
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
        AdminSubscriptionQuery, admin_device_keys_handler, admin_subscriptions_handler,
        parse_admin_subscription_query, subscriptions_handler,
    };
    use crate::delivery::{BarkNotifier, BarkPushConfig, NotificationLinkService};
    use crate::models::{
        AlertRule, DisasterCategory, GeoPoint, MonitoringTarget, NotificationDestination,
        Subscription,
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

    fn subscription(device_key: &str, base_url: &str, label: &str) -> Subscription {
        Subscription::new(
            NotificationDestination::Bark {
                base_url: base_url.to_string(),
                device_key: device_key.to_string(),
            },
            vec![MonitoringTarget {
                label: label.to_string(),
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

    fn bearer_headers(device_key: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {device_key}"))?,
        );
        Ok(headers)
    }

    #[tokio::test]
    async fn device_key_list_is_empty_when_nothing_is_active() -> anyhow::Result<()> {
        let harness = harness()?;
        let response = admin_device_keys_handler(State(harness.state))
            .await
            .into_response();
        anyhow::ensure!(response.status() == StatusCode::OK);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == true);
        anyhow::ensure!(body["data"]["device_keys"] == serde_json::json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn device_key_list_and_lookup_return_active_subscriptions() -> anyhow::Result<()> {
        let harness = harness()?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("abc123", "https://api.day.app", "home"))?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("abc123", "https://bark.example.com", "office"))?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("otherkey", "https://api.day.app", "other"))?;

        let list_response = admin_device_keys_handler(State(harness.state.clone()))
            .await
            .into_response();
        anyhow::ensure!(list_response.status() == StatusCode::OK);
        let list_body = json_body(list_response).await?;
        anyhow::ensure!(
            list_body["data"]["device_keys"] == serde_json::json!(["abc123", "abc123", "otherkey"])
        );

        let lookup = admin_subscriptions_handler(
            State(harness.state.clone()),
            Ok(Query(AdminSubscriptionQuery {
                device_key: "abc123".to_string(),
            })),
        )
        .await
        .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let lookup_body = json_body(lookup).await?;
        anyhow::ensure!(lookup_body["success"] == true);
        let subscriptions = lookup_body["data"]["subscriptions"]
            .as_array()
            .context("missing subscriptions array")?;
        anyhow::ensure!(subscriptions.len() == 2);
        anyhow::ensure!(
            subscriptions
                .iter()
                .all(|item| item["destination"]["device_key"] == "abc123")
        );
        Ok(())
    }

    #[tokio::test]
    async fn lookup_returns_ok_with_success_false_after_unsubscribe() -> anyhow::Result<()> {
        let harness = harness()?;
        let value = subscription("abc123", "https://api.day.app", "home");
        harness
            .state
            .subscriptions
            .upsert_subscription(value.clone())?;
        harness
            .state
            .subscriptions
            .delete_subscription(&value.destination_id())?;

        let lookup = admin_subscriptions_handler(
            State(harness.state),
            Ok(Query(AdminSubscriptionQuery {
                device_key: "abc123".to_string(),
            })),
        )
        .await
        .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let body = json_body(lookup).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "订阅不存在或已取消");
        anyhow::ensure!(body["data"].is_null());
        Ok(())
    }

    #[tokio::test]
    async fn admin_lookup_returns_ok_with_success_false_for_unknown_key() -> anyhow::Result<()> {
        let harness = harness()?;
        let lookup = admin_subscriptions_handler(
            State(harness.state),
            Ok(Query(AdminSubscriptionQuery {
                device_key: "unknownkey".to_string(),
            })),
        )
        .await
        .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let body = json_body(lookup).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "订阅不存在或已取消");
        anyhow::ensure!(body["data"].is_null());
        Ok(())
    }

    #[test]
    fn missing_or_invalid_device_key_is_rejected() -> anyhow::Result<()> {
        let uri = axum::http::Uri::from_static("/api/admin/subscriptions");
        let query = Query::<AdminSubscriptionQuery>::try_from_uri(&uri);
        let Err((status, _)) = parse_admin_subscription_query(query) else {
            anyhow::bail!("missing key should be rejected");
        };
        anyhow::ensure!(status == StatusCode::BAD_REQUEST);

        let invalid = parse_admin_subscription_query(Ok(Query(AdminSubscriptionQuery {
            device_key: "bad key".to_string(),
        })));
        let Err((status, message)) = invalid else {
            anyhow::bail!("invalid key should be rejected");
        };
        anyhow::ensure!(status == StatusCode::BAD_REQUEST);
        anyhow::ensure!(message == "Bark Key 只能包含字母、数字");
        Ok(())
    }

    #[tokio::test]
    async fn bearer_lookup_returns_active_subscriptions() -> anyhow::Result<()> {
        let harness = harness()?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("abc123", "https://api.day.app", "home"))?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("abc123", "https://bark.example.com", "office"))?;
        harness
            .state
            .subscriptions
            .upsert_subscription(subscription("otherkey", "https://api.day.app", "other"))?;

        let lookup = subscriptions_handler(State(harness.state), bearer_headers("abc123")?)
            .await
            .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let lookup_body = json_body(lookup).await?;
        anyhow::ensure!(lookup_body["success"] == true);
        anyhow::ensure!(lookup_body["message"] == "订阅详情获取成功");
        let subscriptions = lookup_body["data"]["subscriptions"]
            .as_array()
            .context("missing subscriptions array")?;
        anyhow::ensure!(subscriptions.len() == 2);
        anyhow::ensure!(
            subscriptions
                .iter()
                .all(|item| item["destination"]["device_key"] == "abc123")
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_bearer_is_unauthorized() -> anyhow::Result<()> {
        let harness = harness()?;
        let response = subscriptions_handler(State(harness.state), HeaderMap::new())
            .await
            .into_response();
        anyhow::ensure!(response.status() == StatusCode::UNAUTHORIZED);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "Bark Key 验证失败");
        Ok(())
    }

    #[tokio::test]
    async fn invalid_bearer_is_unauthorized() -> anyhow::Result<()> {
        let harness = harness()?;
        let response = subscriptions_handler(State(harness.state), bearer_headers("bad key")?)
            .await
            .into_response();
        anyhow::ensure!(response.status() == StatusCode::UNAUTHORIZED);
        let body = json_body(response).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "Bark Key 验证失败");
        Ok(())
    }

    #[tokio::test]
    async fn bearer_lookup_returns_ok_with_success_false_after_unsubscribe() -> anyhow::Result<()> {
        let harness = harness()?;
        let value = subscription("abc123", "https://api.day.app", "home");
        harness
            .state
            .subscriptions
            .upsert_subscription(value.clone())?;
        harness
            .state
            .subscriptions
            .delete_subscription(&value.destination_id())?;

        let lookup = subscriptions_handler(State(harness.state), bearer_headers("abc123")?)
            .await
            .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let body = json_body(lookup).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "订阅不存在或已取消");
        anyhow::ensure!(body["data"].is_null());
        Ok(())
    }

    #[tokio::test]
    async fn bearer_lookup_returns_ok_with_success_false_for_unknown_key() -> anyhow::Result<()> {
        let harness = harness()?;
        let lookup = subscriptions_handler(State(harness.state), bearer_headers("unknownkey")?)
            .await
            .into_response();
        anyhow::ensure!(lookup.status() == StatusCode::OK);
        let body = json_body(lookup).await?;
        anyhow::ensure!(body["success"] == false);
        anyhow::ensure!(body["message"] == "订阅不存在或已取消");
        anyhow::ensure!(body["data"].is_null());
        Ok(())
    }
}
