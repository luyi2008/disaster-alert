# 灾害预警 Bark 订阅系统

通过 Bark 接收地震、气象、海啸和台风信息。服务提供 HTTP JSON API。网页订阅界面在独立仓库 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web)，可单独部署。

## 功能

- 接收 Wolfx、FAN Studio 和 Huania 提供的灾害信息
- 支持地震预警、地震速报、气象预警、海啸预警和台风信息
- 每个 Bark 订阅可以配置最多 3 个监测地点
- 可按灾种、信息来源、预计烈度、震级、严重度和距离设置通知条件
- 地震通知显示监测点预计烈度、距离以及 P 波和 S 波到达时间
- 地震预警会按监测点的实际 S 波剩余时间每秒更新，直到震波到达
- 不同灾种使用独立的 Bark 标题和正文排版，不显示内部渠道、事件 ID 等开发字段
- 通知可打开详情页查看灾害信息和本次命中的订阅条件
- 服务重启后会继续处理尚未完成的订阅确认和通知

本仓库的二进制只提供 JSON API 与后台任务。启动服务用 `cargo run`（清单里只有 `disaster-alert`）。进程和 Docker 镜像都不会再返回网页；订阅页和通知详情页见 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web)。若 Bark 详情仍使用原来的站点根地址，由站点反代将 `/` 与 `/incidents` 指到前端服务、将 `/api` 与 `/health` 指到本服务。

地震波到达时间由起震时间、距离、深度和配置的波速估算。震级不改变传播时间，但会影响监测点的预计烈度；预计烈度未命中订阅规则时不会发送通知。

## 部署

本仓库推荐用 Docker Compose 跑 API。镜像编译和运行都是 Debian（`rust:1.97-bookworm` / `debian:bookworm-slim`）。GitHub Actions 在 PR 上只构建镜像、不上传；合并进 `main` 后把镜像上传到 GitHub Container Registry（`ghcr.io`，不是 Docker Hub），再 SSH 到运行环境（本项目实例是阿里云 ECS）拉取并重启容器。

网页不在本镜像里，见 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web)。

### Docker Compose（推荐）

克隆仓库并准备配置：

```bash
git clone https://github.com/luyi2008/disaster-alert.git
cd disaster-alert
cp .env.example .env
```

生成签名私钥：

```bash
openssl rand 32 | base64 | tr '+/' '-_' | tr -d '=\n'
```

编辑 `.env`，填写通知详情页访问地址和上一步生成的私钥：

```dotenv
ALERT_DETAIL_BASE_URL=https://alerts.example.com
ALERT_SIGNING_KEY=生成的私钥
```

阅读[使用与部署责任](#使用与部署责任)后，如确认接受实例运营责任，再设置：

```dotenv
INSTANCE_TERMS_ACCEPTED=true
```

启动服务：

```bash
docker compose up -d --build
```

检查状态和日志：

```bash
docker compose ps
docker compose logs -f disaster-alert
```

默认日志级别是 `info`（含 warn/error）。过滤器为环境变量 `RUST_LOG`；未设置时等价于 `disaster_alert=info,tower_http=info`。入站请求由 `TraceLayer` 记录，出站 HTTP（高德、Nominatim、Bark、Huania）记录为 `outbound.http`。需要更详细或更安静时：

```dotenv
# RUST_LOG=disaster_alert=debug,tower_http=info
# RUST_LOG=disaster_alert=warn,tower_http=warn
```

本地 `cargo run` 时日志在进程终端。完整约定见 [CONTRIBUTING.md](CONTRIBUTING.md)。

可选应用配置见[配置](#配置)。

生产环境也可以不在主机上构建，改为拉取 CI 推送的镜像：

```bash
export DISASTER_ALERT_IMAGE=ghcr.io/luyi2008/disaster-alert:latest
docker compose pull
docker compose up -d --no-build
```

未设置 `DISASTER_ALERT_IMAGE` 时，Compose 默认使用上述 `latest` 标签；本地开发仍可用 `docker compose up -d --build` 从当前源码构建。

### 合并到 main 后的自动部署

`main` 上的 [container workflow](.github/workflows/container.yml) 会：

1. 构建镜像并上传到 `ghcr.io/<owner>/<repo>:latest`（同时打提交 SHA 标签；不是 Docker Hub）
2. 把当前提交的 `compose.yaml` 拷到主机上的部署目录
3. 把 GitHub Secret `DEPLOY_ENV_FILE` 写成该目录下的 `.env`（覆盖已有文件）。若同时配置了 `AMAP_KEY` / `AMAP_REGEO_URL`，部署会把这两项覆盖进 `.env`（不会打印 Key）
4. 用本次 job 的 `GITHUB_TOKEN` 登录 `ghcr.io`、拉取镜像，并以 `--no-build` 重启容器

`DEPLOY_ENV_FILE` 不是仓库里的文件。它是一个 Actions Secret 的**名字**；**值**是生产环境整份 `.env` 文本（与 [.env.example](.env.example) 同结构，填真实配置）。单独的 `AMAP_KEY` Secret **不会**自动变成容器环境变量，除非走上面的部署覆盖。Compose 仍通过 `env_file: .env` 读主机上的文件；应用进程看不到这些 Secret 名。

需要在本仓库 Settings → Secrets and variables → Actions 配置：

| Secret | 用途 |
| --- | --- |
| `DEPLOY_HOST` | ECS 公网 IP 或 SSH 主机名 |
| `DEPLOY_USER` | SSH 用户 |
| `DEPLOY_SSH_KEY` | 该用户的私钥（仅用于部署） |
| `DEPLOY_PATH` | 主机上放置 `compose.yaml` 与 `.env` 的目录，例如 `/opt/disaster-alert` |
| `DEPLOY_ENV_FILE` | 整份生产 `.env` 文本。第一次把 ECS 上现有 `.env` 贴进去，避免首次部署写成空文件 |
| `AMAP_KEY` | 可选。高德 Web 服务 Key；部署时写入主机 `.env` |
| `AMAP_REGEO_URL` | 可选。高德逆地理编码 URL；部署时写入主机 `.env` |

日常改业务配置：在 GitHub 里编辑 `DEPLOY_ENV_FILE`（或上述高德 Secret），然后手动运行该 workflow（`workflow_dispatch`）或等下次合进 `main`。每次部署都会覆盖主机上的 `.env`，不要在 ECS 上改完还指望能留下。私钥不进 git、不进镜像。

首次在 ECS 上准备一次即可：安装 Docker 与 Compose 插件、把部署公钥写入 `authorized_keys`、创建可写的 `DEPLOY_PATH`。安全组放行 SSH（建议限制来源），应用端口继续只绑 `127.0.0.1`，对外 HTTPS 由主机上的反向代理处理（配置不在本仓库）。数据库在 Docker 命名卷里，换镜像不会删除。

PR 不会部署、也不会写 `.env`。未配置上述 secrets 时，合并后的 deploy job 会失败；镜像若已上传仍会留在 `ghcr.io`。

### 手动部署

不使用 Docker 时，需要 Rust `1.97` 或更高版本。先准备配置：

```bash
cp .env.example .env
```

在 `.env` 中填写 `ALERT_DETAIL_BASE_URL`、`ALERT_SIGNING_KEY` 和其他需要的配置，然后构建并启动：

```bash
cargo build --release
./target/release/disaster-alert
```

生产环境建议监听 `127.0.0.1`，再通过反向代理提供 HTTPS。

## 维护

### 更新 Docker Compose 部署

本项目实例在 `main` 更新后由 Actions 自动拉取镜像。其他自建环境可以：

```bash
git pull --ff-only
docker compose up -d --build
```

或只更新镜像、不在主机编译：

```bash
docker compose pull
docker compose up -d --no-build
```

数据库保存在 Docker 命名卷中。`docker compose down` 不会删除数据库；`docker compose down -v` 会永久删除数据库。

数据库目录只能由一个应用实例使用，不要增加 `disaster-alert` 服务的副本数。Compose 会等待服务优雅退出并完成数据库刷盘。

### 日志

容器日志即进程 stdout：

```bash
docker compose logs -f disaster-alert
docker compose logs -f disaster-alert 2>&1 | grep outbound.http
```

`RUST_LOG` 控制 `tracing` 过滤器（进程环境优先于 `.env`）。生产未设置时默认 `info`：生命周期、订阅变更、入站 HTTP、出站 `outbound.http`、warn/error。心跳和确认完成等 `debug` 事件默认不输出。

## 配置

应用会读取当前工作目录下的 `.env`。进程环境变量优先于 `.env`；完整示例见 [.env.example](.env.example)。

### 应用服务

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `INSTANCE_TERMS_ACCEPTED` | `false` | 为 `false` 时拒绝新增和覆盖订阅，已有任务与取消订阅不受影响。设为 `true` 前须阅读“使用与部署责任” |
| `SERVER_HOST` | `0.0.0.0` | 监听地址 |
| `SERVER_PORT` | `30010` | 服务端口 |
| `SERVER_PUBLISH_HOST` | `127.0.0.1` | Docker Compose 发布端口时使用的宿主机地址；不使用 Compose 时忽略 |
| `ALLOWED_ORIGINS` | 空 | 允许访问 API 的前端 Origin，多个值用逗号分隔 |
| `DB_PATH` | `./data/disaster-alert.fjall` | 数据库目录；同一目录只能由一个应用实例使用 |
| `SHUTDOWN_TIMEOUT_SECONDS` | `15` | 服务关闭时的最长等待时间，范围 `1..=300` 秒 |
| `RUST_LOG` | `disaster_alert=info,tower_http=info` | `tracing` 日志过滤器。未设置时使用该默认值 |

### Bark

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `BARK_URL_ALLOWLIST` | `https://api.day.app` | 网页端可以选择的 Bark 服务地址，多个值用逗号分隔 |
| `BARK_SOUND` | 空 | Bark 铃声名称，空表示使用默认铃声 |
| `BARK_VOLUME` | `10` | 通知音量，范围 `0..=10` |
| `BARK_GROUP` | `灾害预警` | Bark 通知分组名 |
| `BARK_CALL` | `true` | 是否为非静默灾害通知启用 Bark 通话级提醒 |
| `ALERT_DETAIL_BASE_URL` | 必填 | Bark 客户端能够访问的通知详情页根地址，部署时使用 HTTPS |
| `ALERT_SIGNING_KEY` | 必填 | 32 字节、无填充的 URL-safe Base64 私钥 |

`BARK_URL_ALLOWLIST` 支持域名、IP、端口和反向代理子路径，例如：

```dotenv
BARK_URL_ALLOWLIST=https://api.day.app,http://192.168.1.10:8080,https://example.com/bark
```

### 灾害数据

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `RECONNECT_MIN_SECONDS` | `1` | 数据源断开后的最小重连间隔 |
| `RECONNECT_MAX_SECONDS` | `30` | 数据源断开后的最大重连间隔 |
| `PUSH_UPDATES` | `false` | 是否推送同一事件的后续报告 |
| `UPDATE_MIN_REPORT_GAP` | `1` | 后续报告至少间隔多少个报告编号才再次推送 |
| `IGNORE_TRAINING` | `true` | 是否忽略演练信息 |
| `IGNORE_CANCEL` | `false` | 是否忽略取消或解除信息，通常应保持 `false` |
| `STALE_ORIGIN_SECONDS` | `600` | 忽略起震时间超过该秒数的地震预警 |
| `P_WAVE_KM_S` | `6.0` | P 波估算速度，单位 km/s |
| `S_WAVE_KM_S` | `3.5` | S 波估算速度，单位 km/s |

### 反向地理编码

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `REVERSE_GEOCODING_ENABLED` | `true` | 是否启用 `/api/reverse-geocode` |
| `REVERSE_GEOCODING_URL` | `https://nominatim.openstreetmap.org/reverse` | Nominatim 备用接口。阿里云 ECS 等访问境外超时的环境应配置 `AMAP_KEY` |
| `AMAP_KEY` | 空 | 可选。高德开放平台 **Web 服务** Key；设置后优先走高德逆地理编码，失败再回退 Nominatim |
| `AMAP_REGEO_URL` | `https://restapi.amap.com/v3/geocode/regeo` | 高德逆地理编码地址，仅在设置了 `AMAP_KEY` 时使用 |

其余环境变量用于数据保留和 Bark 并发，默认值见 [.env.example](.env.example)。

## 安全与隐私

服务会保存 Bark Key、监测地点和通知规则。通知详情 URL 包含访问凭据，反向代理、CDN、WAF、APM 和分析系统不得记录 `/incidents/` 路径的完整 URL。

- 不要提交真实 `.env`、数据库、Bark Key、高德 Key 或签名私钥
- 不要在日志、截图、Issue 或测试数据中使用真实 Bark Key、用户位置或通知详情 URL
- 修改 `ALERT_SIGNING_KEY` 后，之前发送的详情链接会失效
- 统计接口只返回聚合数量

## 使用与部署责任

本仓库提供可独立部署的软件源代码。项目维护者不运营、控制或认可第三方使用本项目搭建的实时灾害信息、订阅、通知或预警服务。

将 `INSTANCE_TERMS_ACCEPTED=true` 写入部署环境，表示实例运营者明确确认：

- 启用实时数据或向他人提供服务前，应自行核查部署地、服务对象所在地及数据来源所在地适用的法律法规，并取得主管部门和数据提供方要求的许可或授权
- 实例运营者对数据接入、内容展示、通知发送、个人信息处理、数据保存和服务对象范围承担责任；自部署不等于获准向社会发布预警
- 本项目及其处理的信息可能延迟、缺失或误报，不属于官方预警，也不应作为唯一的灾害预警、安全决策或应急行动依据

该环境变量只记录部署者的明确确认，不能替代法律评估、行政许可、数据授权或个人信息处理依据，也不能证明某项部署当然合法。若部署者不能确认上述事项，应保持默认值 `false`，并停止对外提供实时功能。

## API

机器可读的接口规范见 [OpenAPI 3.1](docs/openapi.yaml)。网页由 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web) 提供，不包含在本二进制中。

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `POST` | `/api/subscribe` | 创建或覆盖订阅 |
| `DELETE` | `/api/unsubscribe` | 删除订阅 |
| `GET` | `/api/bark-urls` | 获取可用的 Bark 服务地址 |
| `GET` | `/api/subscription-options` | 获取灾种、来源和默认规则 |
| `GET` | `/api/reverse-geocode` | 根据坐标查询行政区 |
| `GET` | `/api/incidents/{incident_id}/notifications/{token}` | 获取通知详情（需通知链接中的 token） |
| `GET` | `/api/status` | 获取订阅总数、数据源、后台任务状态，以及实例是否已确认责任声明 |
| `GET` | `/health` | 健康检查 |

## 开发

本仓启动 API（需先 `cp .env.example .env` 并填写必填项）：

```bash
cargo run
```

默认监听 `http://127.0.0.1:30010` 上的 JSON API，没有网页。

本地看订阅界面时，再在 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web) 里执行 `npm run dev`，浏览器打开 Vite 地址。前端开发约定见该仓库 README。

提交前：

```bash
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
```

更多开发约定见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 致谢

- 数据源：[wolfx.jp](https://ws-api.wolfx.jp)
- 数据源：[FAN Studio](https://api.fanstudio.tech/doc/ws-api/#home)
- 数据源：[成都高新减灾研究所](http://www.365icl.com/) / [成都市美幻科技有限公司](http://www.huania.com/)
- 推送服务：[Bark](https://github.com/Finb/Bark)
