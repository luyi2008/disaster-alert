# 贡献者指南

项目主要面向中国用户，用户可见文案、文档和代码注释优先使用中文；协议字段、配置项、日志 `event`、依赖 feature 和代码标识保持英文原文

## 改动边界

- `src/` 包含完整 Rust 应用：订阅 API、WebSocket 监听、订阅匹配和 Bark 推送。本仓不嵌入网页。
- 网页在独立仓库 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web)，由该仓自行构建和部署。
- 仓库不维护特定平台的反向代理、进程守护或静态托管配置。合并进 `main` 后的容器发布由 GitHub Actions 完成（构建镜像、上传到 `ghcr.io`、SSH 到已有 Docker 主机，并用 Secret `DEPLOY_ENV_FILE` 写出 `.env`）；ECS 上的 nginx/Caddy 与安全组仍由运维自行管理。

服务端行为和 Web 交互尽量分开改，跨层改动需要说明数据流如何变化

## 本地检查

本仓只跑 API，入口是 `disaster-alert`：

```bash
cp .env.example .env
# 在 .env 中填写 ALERT_DETAIL_BASE_URL、ALERT_SIGNING_KEY 等
cargo run
```

要打开订阅页或通知详情页，另开终端克隆并启动 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web)：

```bash
cd ../disaster-alert-web
npm install
npm run dev
```

Vite 会把 `/api` 和 `/health` 代理到本机 API。浏览器打开 Vite 提示的本地地址，不要只开 `http://127.0.0.1:30010/` 当网页。

提交前至少跑：

```bash
cargo fmt --check
cargo check
cargo test
```

如果改动涉及依赖、并发、错误处理、HTTP/WebSocket 或共享模型，也跑：

```bash
cargo clippy --all-targets --all-features
```

## Rust 和依赖

`Cargo.toml` 已启用严格 lint，新增代码不要使用 `unwrap()`、`expect()`、`dbg!()`、`println!()`、`todo!()`、`unimplemented!()`，也不要引入 `unsafe`

新增或升级依赖时，默认保持 `default-features = false`，只开启实际用到的 feature，不要启用 `tokio/full`、TLS 双栈或框架默认全量功能来省配置，依赖变更后检查：

```bash
cargo tree -e features
cargo check
cargo test
```

## 日志和注释

后端统一使用 `tracing`，日志面向排障，动态值放字段里；用户可见文案继续中文

```rust
tracing::info!(
    event = "subscription.request_completed",
    device_key = %mask_device_key(&device_key),
    "subscription.request_completed"
);
```

约定：

- `event` 使用稳定英文标识，格式为 `domain.action`
- Bark Key、通知 token、URL 中的密钥查询参数必须脱敏：Bark Key 和 token 保留首尾各 3 位、中间 `***`（≤6 字符则全 `***`）；高德 `key` 等查询参数整段替换为 `***`
- 日志可以输出 `latitude` / `longitude`
- 意外失败用 `error = ?error`（Debug，保留错误链），不要只写字符串。预期的客户端拒绝（例如无效通知链接）可用 `error = %error`（Display）
- 高频心跳、pong、重复事件、Huania 秒级轮询使用 `debug`
- 出站 HTTP 在 `info` 打 `outbound.http`（method、脱敏 URL、status、截断后的响应体），但 Huania 每秒轮询属于高频，成功和失败的 `outbound.http` 都记 `debug`（失败另有 `huania.poll_failed`）。入站 HTTP 由 `TraceLayer` 在 `info` 记录 method / 脱敏 URI / status，不记录请求体或响应体

### 如何查看和控制日志

日志写进程 stdout。Docker 下：

```bash
docker compose logs -f disaster-alert
docker compose logs -f disaster-alert 2>&1 | grep outbound.http
```

本地 `cargo run` 时日志直接出现在终端。

过滤器是环境变量 `RUST_LOG`（`tracing-subscriber` 的 EnvFilter，不是本项目自定义项）。未设置时默认为 `disaster_alert=info,tower_http=info`，即本 crate 与入站 `TraceLayer` 的 info 及以上（含 warn/error）。`debug` 默认关闭。进程环境变量优先于 `.env`。

```dotenv
# RUST_LOG=disaster_alert=debug,tower_http=info
# RUST_LOG=disaster_alert=warn,tower_http=warn
```

生产默认会打：启动与关停、订阅写入/删除（Key 已脱敏）、数据源 WebSocket 连上、入站请求（method / 脱敏 URI / status）、低频出站 HTTP（Bark、高德、Nominatim）、以及 warn/error。生产默认不打：心跳/pong、Huania 秒级轮询、`subscription.confirmation_attempt_completed`、入站请求体、完整 Bark Key、高德 Key、完整通知 token。

`TraceLayer` 只覆盖打进本服务的入站请求，不会记录出站的高德 / Nominatim / Bark / Huania；那些走 `outbound.http`。Huania 轮询默认静默，需要看完整响应时把 `RUST_LOG` 调到 `debug`。

注释只解释代码本身看不出的内容，例如上游字段拼写、时区、算法边界、平台限制和业务规则来源，不要写「创建变量」「保存数据」这类逐行复述，也不要写没有指标支撑的「高并发」「百万级」「优化版」

## 安全和隐私确认

这个项目会保存 Bark Key、监测地点和通知级别，任何相关改动都要先确认下面几条约束：

- 用户面只允许通过 `POST /api/subscribe` 创建或覆盖订阅，通过 `DELETE /api/unsubscribe` 删除订阅
- `POST /api/simulate` 与 `GET /api/history` 是旁路测试口：只对 Bearer 或 `device_ID_list` 中的已存订阅调用 Bark，**不**进入 `EventRuntime` / inbox / 匹配 / 投递账本，也**不**对全站扇出。不要求 `INSTANCE_TERMS_ACCEPTED`。真实 Bark Key 不得写入测试、文档或提交。架构与测试方法见 [docs/simulate.md](docs/simulate.md)
- `GET /api/deliveries` 用同一套 Bearer Bark Key 读取**调用方自己的**投递账本（成功送达的直播灾害通知）。不得接受空 Key，也不得列出其他设备的记录。不包含模拟旁路、订阅确认通知或失败/重试中的投递。取消订阅后，在账本保留期内仍可查询
- 另有未写入 README / OpenAPI 的运营只读接口：列出当前激活订阅的 Bark Key、按 Key 返回地点和规则、查询投递账本（`device_key` 可空，空则返回全部成功投递，每条含 Bark Key 与事件内容），以及列出保留期内已接入灾害的事件详情（`GET /api/admin/events`）。当前无鉴权，后续补上；不要把这些路径写进用户文档
- 退订接口只返回操作结果，不回显订阅内容
- 公开的统计接口只返回聚合数量，不返回 Bark Key、位置或通知规则
- 日志中只输出 `mask_device_key` 处理后的 Bark Key 和通知 token，不输出完整 Bark Key、高德 Key 和原始订阅请求体；入站 URI 中的 `device_key` 查询参数同样必须脱敏
- 示例、测试、截图和 issue 不使用真实 Bark Key 或真实用户位置
- 不提交真实 `.env`、数据库文件、Bark key、访问 token 或生产私密配置
- 修改 CORS、反代或静态托管规则时，确认不会把未鉴权的订阅详情读取面暴露给非运营调用方

涉及隐私边界的 PR 或提交说明里，需要明确写出是否新增了读取接口、是否回显订阅数据、日志里是否可能出现完整 Bark Key
