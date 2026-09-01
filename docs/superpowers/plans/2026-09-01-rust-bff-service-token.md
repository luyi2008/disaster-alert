# Rust 写接口认 BFF 服务凭证 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `disaster-alert` 的写订阅接口（创建/覆盖、退订、模拟推送）只接受 `Authorization: Bearer <BFF_SERVICE_TOKEN>`；浏览器再用 Bark token 当写凭证会被拒。匹配管道和 Bark 推送内容不变。

**Architecture:** 新增共享校验函数，用恒定时间比较请求头与配置里的服务凭证。凭证进入 `Config` 和 `AppState`。写接口先鉴权再走现有业务。模拟推送的设备身份从 JSON `device_ID_list` 读取，不再把 Bearer 当 Bark Key。通知详情深链、`GET /api/history`、以及现网用 Bark Bearer 的读接口（`GET /api/subscriptions`、`GET /api/deliveries`）本计划不改。

**Tech Stack:** Rust 2024、Axum 0.8、现有 `SecretString` / `zeroize`、cargo test。

## Global Constraints

- 写接口：`POST /api/subscribe`、`DELETE /api/unsubscribe`、`POST /api/simulate` 只接受 `Authorization: Bearer <BFF_SERVICE_TOKEN>`。
- 浏览器 `Authorization: Bearer <bark token>` 写订阅必须失败（401）。
- 服务凭证与用户 cookie 不是同一种东西；本仓不解析 cookie。
- `BFF_SERVICE_TOKEN` 为必填环境变量；空值不得启动。
- 通知详情 `GET /api/incidents/{incident_id}/notifications/{token}` 仍公开。
- `GET /api/history`、`GET /api/subscriptions`、`GET /api/deliveries`、`/api/admin/*` 本计划不改鉴权。
- **禁止**修改匹配逻辑、数据源、Bark 推送内容格式（`src/matching/`、`src/providers/`、`src/delivery/message.rs`、`src/delivery/bark.rs` 的推送体）。
- 不把服务凭证或 Bark token 写入日志正文。
- 用户可见错误文案用中文；配置项、日志 `event` 用英文。
- 新增代码遵守仓内 lint：禁止 `unwrap` / `expect` / `todo` / `unsafe`。
- 不新建 npm/Node 依赖；不改 `disaster-alert-bff`、`disaster-alert-web`。

## File map

| 文件 | 职责 |
| --- | --- |
| `src/routes/bff_auth.rs` | 解析 Bearer、恒定时间比较、401 文案 |
| `src/config.rs` | 必填 `BFF_SERVICE_TOKEN`；`SecretString` 可 Clone |
| `src/routes/subscribe.rs` | `AppState` 持有凭证；subscribe / unsubscribe 先鉴权 |
| `src/routes/simulate.rs` | 写模拟口改验服务凭证；设备来自 `device_ID_list` |
| `src/simulate/mod.rs` | `resolve_device_keys` 不再默认 Bearer |
| `src/routes/mod.rs` | 声明 `bff_auth` 模块 |
| `src/application.rs` | 把配置凭证注入 `AppState` |
| `.env.example` / `README.md` / `CONTRIBUTING.md` / `docs/simulate.md` / `docs/openapi.yaml` | 契约与运维说明 |

---

### Task 1: 配置读取 `BFF_SERVICE_TOKEN`

**Files:**
- Modify: `src/config.rs`
- Modify: `.env.example`

**Interfaces:**
- Consumes: 现有 `required_env_secret`
- Produces: `Config.bff_service_token: SecretString`；`SecretString: Clone`；`SecretString::new(String)`

- [ ] **Step 1: Write the failing test**

在 `src/config.rs` 的 `tests` 模块追加：

```rust
    #[test]
    fn secret_string_is_redacted_and_cloneable() {
        let secret = SecretString::new("bff-service-token-value".to_string());
        let cloned = secret.clone();
        assert_eq!(secret.expose(), "bff-service-token-value");
        assert_eq!(cloned.expose(), "bff-service-token-value");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
    }
```

把 `use super::{normalize_bark_url, validate_public_base_url};` 改成包含 `SecretString`。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::tests::secret_string_is_redacted_and_cloneable -- --nocapture`

Expected: FAIL — `SecretString::new` 不存在，且 `SecretString` 未实现 `Clone`。

- [ ] **Step 3: Write minimal implementation**

给 `SecretString` 加 `Clone` 和构造函数，并在 `Config` 增加字段：

```rust
    pub(crate) bff_service_token: SecretString,
```

在 `Config::from_env` 里 `alert_signing_key` 旁增加：

```rust
            bff_service_token: required_env_secret("BFF_SERVICE_TOKEN")?,
```

```rust
impl SecretString {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        Self::new(self.0.as_str().to_string())
    }
}
```

把原来的 `expose` 实现合并进上面，不要留两份。

`.env.example` 在 `ALERT_SIGNING_KEY` 后增加：

```dotenv
# Shared bearer token the BFF must send on write APIs
# (POST /api/subscribe, DELETE /api/unsubscribe, POST /api/simulate).
BFF_SERVICE_TOKEN=replace-with-long-random-service-token
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config::tests -- --nocapture`

Expected: PASS。`cargo check` 也要通过（`Config` 结构体字面量已补字段）。

- [ ] **Step 5: Commit**

```bash
git add src/config.rs .env.example
git commit -m "Require BFF_SERVICE_TOKEN in disaster-alert config."
```

---

### Task 2: 共享 BFF Bearer 校验

**Files:**
- Create: `src/routes/bff_auth.rs`
- Modify: `src/routes/mod.rs`

**Interfaces:**
- Consumes: Axum `HeaderMap`、`AUTHORIZATION`
- Produces:
  - `pub(crate) const BFF_AUTH_FAILED_MESSAGE: &str = "服务凭证无效";`
  - `pub(crate) fn require_bff_service_token(headers: &HeaderMap, expected: &str) -> Result<(), (StatusCode, String)>`
  - 恒定时间比较；长度不同直接 false；不校验 Bark Key 字符集

- [ ] **Step 1: Write the failing test**

创建 `src/routes/bff_auth.rs`，先只放测试和将要调用的函数签名（函数体 `panic` 或先不写，让编译失败也算红灯）。推荐完整测试：

```rust
use axum::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};

const BFF_AUTH_FAILED_MESSAGE: &str = "服务凭证无效";

pub(crate) fn require_bff_service_token(
    headers: &HeaderMap,
    expected: &str,
) -> Result<(), (StatusCode, String)> {
    let _ = (headers, expected);
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        "not implemented".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{BFF_AUTH_FAILED_MESSAGE, require_bff_service_token};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {value}")).expect("header"),
        );
        headers
    }

    #[test]
    fn missing_header_is_unauthorized() {
        let error = require_bff_service_token(&HeaderMap::new(), "expected-token").err();
        assert_eq!(
            error,
            Some((StatusCode::UNAUTHORIZED, BFF_AUTH_FAILED_MESSAGE.to_string()))
        );
    }

    #[test]
    fn bark_token_bearer_is_rejected() {
        let error = require_bff_service_token(&bearer("abc123"), "expected-token").err();
        assert_eq!(
            error,
            Some((StatusCode::UNAUTHORIZED, BFF_AUTH_FAILED_MESSAGE.to_string()))
        );
    }

    #[test]
    fn matching_service_token_is_accepted() {
        assert_eq!(
            require_bff_service_token(&bearer("expected-token"), "expected-token").ok(),
            Some(())
        );
    }

    #[test]
    fn scheme_is_case_insensitive_and_value_is_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer  expected-token  "),
        );
        assert!(require_bff_service_token(&headers, "expected-token").is_ok());
    }

    #[test]
    fn length_mismatch_is_rejected_without_accepting_prefix() {
        let error = require_bff_service_token(&bearer("expected-token-extra"), "expected-token").err();
        assert_eq!(
            error,
            Some((StatusCode::UNAUTHORIZED, BFF_AUTH_FAILED_MESSAGE.to_string()))
        );
    }
}
```

`src/routes/mod.rs` 增加 `mod bff_auth;`。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib routes::bff_auth -- --nocapture`

Expected: FAIL — `not implemented` 或断言失败（missing/bark 可能碰巧走 500）。确认不是编译 typo。

- [ ] **Step 3: Write minimal implementation**

替换函数体：

```rust
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};

pub(crate) const BFF_AUTH_FAILED_MESSAGE: &str = "服务凭证无效";

pub(crate) fn require_bff_service_token(
    headers: &HeaderMap,
    expected: &str,
) -> Result<(), (StatusCode, String)> {
    let failed = || (StatusCode::UNAUTHORIZED, BFF_AUTH_FAILED_MESSAGE.to_string());
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err(failed());
    };
    let Ok(text) = value.to_str() else {
        return Err(failed());
    };
    let trimmed = text.trim();
    let Some((scheme, rest)) = trimmed.split_once(' ') else {
        return Err(failed());
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(failed());
    }
    let provided = rest.trim();
    if provided.is_empty() || !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(failed());
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib routes::bff_auth -- --nocapture`

Expected: PASS（5 tests）。

- [ ] **Step 5: Commit**

```bash
git add src/routes/bff_auth.rs src/routes/mod.rs
git commit -m "Add constant-time BFF service bearer checks."
```

---

### Task 3: subscribe / unsubscribe 先验服务凭证

**Files:**
- Modify: `src/routes/subscribe.rs`
- Modify: `src/application.rs`

**Interfaces:**
- Consumes: `require_bff_service_token`、`Config.bff_service_token`
- Produces: `AppState.bff_service_token: SecretString`（`AppState::new` 默认 `"test-bff-service-token"`，生产用 `with_bff_service_token`）；写接口无凭证或凭证是 Bark Key → 401 `"服务凭证无效"`，且不进入订阅存储

- [ ] **Step 1: Write the failing tests**

在 `src/routes/subscribe.rs` 的 `tests` 模块增加 harness 与鉴权测试。`subscribe_handler` / `unsubscribe_handler` 此时还没有 `HeaderMap` 参数，测试先按目标签名编写，编译失败即红灯。

在现有 `use super::*;` 测试模块里追加（需要 `HeaderMap` / `AUTHORIZATION` / `to_bytes`）：

```rust
    const TEST_BFF_TOKEN: &str = "test-bff-service-token";

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

    fn test_state(terms_accepted: bool) -> (AppState, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(directory.path()).expect("storage");
        let notifier = BarkNotifier::new(
            vec!["https://api.day.app".to_string()],
            2,
            4,
            BarkPushConfig::new(None, 10, "test".to_string(), false),
        )
        .expect("notifier");
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
        (state, directory)
    }

    async fn json_body(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&body).expect("json"),
        )
    }

    #[tokio::test]
    async fn subscribe_without_service_token_is_unauthorized() {
        let (state, _directory) = test_state(true);
        let response = subscribe_handler(
            State(state),
            HeaderMap::new(),
            Ok(Json(request())),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "服务凭证无效");
    }

    #[tokio::test]
    async fn subscribe_with_bark_bearer_is_unauthorized() {
        let (state, _directory) = test_state(true);
        let response = subscribe_handler(
            State(state),
            bark_headers(),
            Ok(Json(request())),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "服务凭证无效");
    }

    #[tokio::test]
    async fn subscribe_checks_instance_terms_after_service_token() {
        let (state, _directory) = test_state(false);
        let response = subscribe_handler(
            State(state),
            bff_headers(),
            Ok(Json(request())),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["message"], INSTANCE_TERMS_REQUIRED_MESSAGE);
    }

    #[tokio::test]
    async fn unsubscribe_without_service_token_is_unauthorized() {
        let (state, _directory) = test_state(true);
        let payload = UnsubscribeRequest {
            destination: NotificationDestination::Bark {
                base_url: "https://api.day.app".to_string(),
                device_key: "abc123".to_string(),
            },
        };
        let response = unsubscribe_handler(
            State(state),
            HeaderMap::new(),
            Ok(Json(payload)),
        )
        .await
        .into_response();
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "服务凭证无效");
    }
```

补全测试模块缺失的 import：`BarkNotifier`、`BarkPushConfig`、`NotificationLinkService`、`Storage`、`SubscriptionConfirmationService`、`RuntimeStatus`、`ReverseGeocoder`、`UnsubscribeRequest`、`HeaderMap`、`HeaderValue`、`AUTHORIZATION`、`to_bytes`、`IntoResponse`、`Json`。`TEST_BFF_TOKEN` 若未使用可删，避免 unused lint。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib routes::subscribe::tests::subscribe_without_service_token_is_unauthorized -- --nocapture`

Expected: FAIL compile — `subscribe_handler` 没有 `HeaderMap` 参数。

- [ ] **Step 3: Write minimal implementation**

`AppState` 增加字段并默认测试凭证：

```rust
    pub(crate) bff_service_token: crate::config::SecretString,
```

在 `AppState::new` 的结构体字面量中：

```rust
            bff_service_token: crate::config::SecretString::new(
                "test-bff-service-token".to_string(),
            ),
```

增加：

```rust
    pub(crate) fn with_bff_service_token(
        mut self,
        token: crate::config::SecretString,
    ) -> Self {
        self.bff_service_token = token;
        self
    }
```

`subscribe_handler` 签名改为接受 `headers: HeaderMap`，函数**第一件事**（先于 terms / JSON）：

```rust
    if let Err((status, message)) =
        crate::routes::bff_auth::require_bff_service_token(
            &headers,
            state.bff_service_token.expose(),
        )
    {
        return (
            status,
            Json(ApiResponse::<SubscribeResponse>::error(message)),
        );
    }
```

`unsubscribe_handler` 同样先鉴权，再解析 body。失败时 `Json(ApiResponse::<()>::error(message))`。

`src/application.rs` 在 `AppState::new(...).with_instance_terms_accepted(...)` 链上增加：

```rust
        .with_bff_service_token(config.bff_service_token.clone())
```

- [ ] **Step 4: Run test to verify it passes**

Run:

```bash
cargo test --lib routes::subscribe::tests -- --nocapture
```

Expected: PASS。原先的 terms / 状态测试仍过。

- [ ] **Step 5: Commit**

```bash
git add src/routes/subscribe.rs src/application.rs
git commit -m "Reject subscribe and unsubscribe without the BFF service token."
```

---

### Task 4: simulate 改验服务凭证，设备改从 body 来

**Files:**
- Modify: `src/routes/simulate.rs`
- Modify: `src/simulate/mod.rs`

**Interfaces:**
- Consumes: `require_bff_service_token`、`AppState.bff_service_token`
- Produces: `POST /api/simulate` 必须带服务凭证；设备列表来自 `device_ID_list`（省略 → 400 `"需要 device_ID_list"`）；`resolve_device_keys(device_id_list: Option<Vec<String>>)` 不再接收 bearer 默认值。`GET /api/history` 仍可用 Bark Bearer 标注距离。详情深链测试仍 200。

- [ ] **Step 1: Write the failing tests**

1. 改 `src/simulate/mod.rs` 测试 `resolve_device_keys_defaults_to_bearer_and_rejects_empty_list` 为：

```rust
    #[test]
    fn resolve_device_keys_requires_a_non_empty_list() {
        assert_eq!(
            resolve_device_keys(None),
            Err(DeviceListError::Missing)
        );
        assert_eq!(
            resolve_device_keys(Some(Vec::new())),
            Err(DeviceListError::Empty)
        );
        assert_eq!(
            resolve_device_keys(Some(vec!["keyA".to_string(), "keyA".to_string()])).ok(),
            Some(vec!["keyA".to_string()])
        );
    }
```

`DeviceListError` 增加 `Missing`。

2. 改 `src/routes/simulate.rs` 测试：

- `start_harness` 继续用 `AppState::new`（默认测试凭证）。
- 新增 `fn bff_headers() -> HeaderMap`：`Bearer test-bff-service-token`。
- 新增 `fn device_list_body(keys: &[&str]) -> Bytes`：序列化 `{"device_ID_list":[...]}`。
- 所有 `simulate_handler(..., bearer_headers("targetKey")?, ..., Bytes::new())` 改为 `bff_headers()` + `device_list_body(&["targetKey"])`。
- `bearer_key_matches_stored_subscription_case_insensitively`：body 用 `["TARGETKEY"]`。
- `unknown_bearer_key_is_not_found` 改为：服务凭证正确 + `["unknownKey"]` → 200 且 `data.skipped == 1`、`captured_keys` 为空（与现网「列表里没有订阅记 skipped」一致）。
- `simulate_without_bearer_fails`：无头 → 401 `"服务凭证无效"`。
- 新增 `simulate_with_bark_bearer_fails`：`bearer_headers("targetKey")` + 有效 `device_ID_list` → 401 `"服务凭证无效"`，mock 无 POST。
- 新增 `simulate_without_device_list_is_bad_request`：`bff_headers()` + 空 body → 400 `"需要 device_ID_list"`。
- `missing_authorization_is_a_bark_key_failure`：删除对 simulate `require_bearer` 的断言（该函数若仍被 `history_handler` 使用则保留 history 侧测试；simulate 不再调用它）。
- `simulate_push_detail_link_loads_snapshot_without_incident`：改用服务凭证 + `device_ID_list`，详情 handler 仍不要求登录。
- `history_catalog_is_public_and_can_annotate_a_subscription`：**不要改** history 的 Bark Bearer 行为。

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test --lib simulate:: -- --nocapture
cargo test --lib routes::simulate::tests -- --nocapture
```

Expected: FAIL — `DeviceListError::Missing` 不存在；simulate 仍把 Bark Bearer 当身份，bark bearer 请求会成功而不是 401。

- [ ] **Step 3: Write minimal implementation**

`DeviceListError`：

```rust
pub(crate) enum DeviceListError {
    Missing,
    Empty,
    TooMany,
    InvalidKey,
}
```

```rust
pub(crate) fn resolve_device_keys(
    device_id_list: Option<Vec<String>>,
) -> std::result::Result<Vec<String>, DeviceListError> {
    let Some(list) = device_id_list else {
        return Err(DeviceListError::Missing);
    };
    // 其余逻辑与现网相同：空、>32、字符集、去重
```

`simulate_handler` 开头：

```rust
    if let Err((status, message)) = crate::routes::bff_auth::require_bff_service_token(
        &headers,
        state.bff_service_token.expose(),
    ) {
        return (status, Json(ApiResponse::<SimulateData>::error(message)));
    }
```

删除用 `require_bearer` 取 `bearer_key` 的路径。`resolve_device_keys(device_list)`。`DeviceListError::Missing` → 400 `"需要 device_ID_list"`。

原先 `if !listed_explicitly { lookup_subscriptions(&bearer_key) }` 整段删除（不再有「空 body 默认当前 Key」）。日志字段不要写服务凭证；设备相关日志继续 `mask_device_key` 列表中的 key（可用 `device_keys.first()`）。

`success_message`：`listed_explicitly` 在必填列表后恒为 true。保留函数，单设备时仍可用「已向当前 Bark Key 发送模拟预警」（`target_count == 1` 且 `NotifyLevel`）。把 `listed_explicitly && target_count > 1` 改为 `target_count > 1` 即可。

`require_bearer` / `optional_bearer` 仍留给 `history_handler`，文案维持 `"Bark Key 验证失败"`。

- [ ] **Step 4: Run tests to verify they pass**

Run:

```bash
cargo test --lib routes::simulate::tests simulate:: -- --nocapture
```

Expected: PASS。详情深链测试仍 200 且 snapshot 含「模拟」。

- [ ] **Step 5: Commit**

```bash
git add src/routes/simulate.rs src/simulate/mod.rs
git commit -m "Authenticate simulate with the BFF service token."
```

---

### Task 5: 契约文档

**Files:**
- Modify: `docs/openapi.yaml`
- Modify: `docs/simulate.md`
- Modify: `README.md`
- Modify: `CONTRIBUTING.md`

**Interfaces:**
- Consumes: Task 3–4 的实际行为
- Produces: OpenAPI / 运维文档与实现一致

- [ ] **Step 1: 按实现改文档（无独立红灯测试；对照 OpenAPI 与代码）**

`docs/openapi.yaml`：

- `components.securitySchemes` 增加：

```yaml
    bffServiceBearer:
      type: http
      scheme: bearer
      description: |
        BFF 与 Rust 共享的服务凭证，对应环境变量 `BFF_SERVICE_TOKEN`。
        与 Bark 设备 Key、用户 session cookie 都不是同一种凭据。
```

- `/api/subscribe`、`/api/unsubscribe`、`/api/simulate` 增加 `security: [bffServiceBearer: []]`，401 描述改为缺少或无效的 BFF 服务凭证。
- simulate description：删除「空 body 时目标为当前 Authorization Bearer Key」；改为必须 JSON `device_ID_list`。examples 去掉 `currentDevice: {}`，只保留 listedDevices。
- `SimulateRequest.device_ID_list` description：改为必填；省略 → 400。
- `barkBearer` 方案保留给 `/api/subscriptions`、`/api/deliveries`、`/api/history`。

`docs/simulate.md`：

- 「有」列表第一项改为 `Authorization: Bearer <BFF_SERVICE_TOKEN>`。
- 「推送目标」：必须 `device_ID_list`；空 body → 400「需要 device_ID_list」。
- curl 示例：`-H "Authorization: Bearer $BFF_SERVICE_TOKEN"` 且 body 含 `device_ID_list`。
- 排障表 401 文案改为「服务凭证无效」。
- 测试名表格与 Task 4 对齐。
- `GET /api/history` 仍写 Bark Bearer 可选标注。

`README.md` 环境变量表（Bark 或新「BFF」小节）增加 `BFF_SERVICE_TOKEN` 必填。安全节注明写接口不再接受 Bark Bearer。

`CONTRIBUTING.md`：`POST /api/simulate` 那一行改为服务凭证 + `device_ID_list`；强调匹配/推送内容本改动未触及。

- [ ] **Step 2: 快速核对**

```bash
rg -n "Bark Key 验证失败" docs/simulate.md docs/openapi.yaml src/routes/simulate.rs
rg -n "bffServiceBearer|BFF_SERVICE_TOKEN" docs README.md CONTRIBUTING.md .env.example src
```

Expected: simulate 写口 401 不再写「Bark Key 验证失败」；history / subscriptions / deliveries 仍可出现 Bark Bearer。

- [ ] **Step 3: Commit**

```bash
git add docs/openapi.yaml docs/simulate.md README.md CONTRIBUTING.md
git commit -m "Document BFF service-token auth on Rust write APIs."
```

---

### Task 6: 全量回归（订阅/匹配/推送测试仍过）

**Files:** 无新生产代码。若测试因默认凭证或 `resolve_device_keys` 签名失败，只修测试夹具，不改匹配与推送体。

- [ ] **Step 1: Run the full library test suite**

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Expected:

- `cargo fmt --check` 无 diff
- `cargo test` 全部 PASS，包括 `src/matching/`、`src/delivery/`、`src/runtime/pipeline.rs`、incident 详情
- clippy 无 warning
- `git diff main -- src/matching src/providers src/delivery/message.rs src/delivery/bark.rs` 为空

- [ ] **Step 2: 若 fmt 有 diff，只跑 fmt 再测一次后纳入最后提交**

```bash
cargo fmt
cargo test
```

- [ ] **Step 3: Commit only if Step 2 produced fixes**

```bash
git add -u
git commit -m "Fix formatting after BFF service-token auth."
```

无 diff 则跳过本 commit。

---

## Spec coverage (self-review)

| Spec | Task |
| --- | --- |
| §4 写接口只认 `Authorization: Bearer <BFF_SERVICE_TOKEN>` | 2, 3, 4 |
| §4 浏览器 Bark token 不能写订阅 | 3, 4 |
| §4 详情深链仍公开 | 4 既有 incident 测试；不改 `incident.rs` |
| §8 Rust 校验同一 `BFF_SERVICE_TOKEN` | 1, 3 |
| §9 无凭证写订阅被拒；有凭证时订阅/匹配/推送测试仍过；详情不变 | 3, 4, 6 |
| §10.2 写接口改验服务凭证；匹配/数据源/推送内容不改 | Global Constraints, Task 6 diff 检查 |
| GET 订阅/投递仍 Bark Bearer（本仓后续、BFF 现状） | 明确不改 |
| 网页 / BFF 仓 | 不在本计划 |

## Placeholder scan

无 TBD /「类似 Task N」占位。simulate 设备身份从 Bearer 迁到 `device_ID_list` 已写明测试与文案。
