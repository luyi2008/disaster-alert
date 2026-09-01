# disaster-alert-bff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在独立仓库 `disaster-alert-bff` 落地 Hono + Better Auth 网关：手机 OTP、微信扫码（含 mock）、Bark token 设备资产、以及把订阅读写代理到现有 Rust API。

**Architecture:** 一个 Node 进程。Better Auth 挂在 `/api/auth/*`。设备表与订阅代理走 Hono 路由，先读 session cookie，再查 `user_id → token`，最后用 HTTP JSON 调 `disaster-alert`。短信、Bark `GET /register/{token}`、Rust 客户端都是可替换端口。本计划不改 Rust 写接口鉴权；代理按**现状**调用（`POST /api/subscribe` 带 `destination`，`GET /api/subscriptions` 用 `Authorization: Bearer <bark token>`）。服务凭证头先带上，留给下一份 Rust 计划启用。

**Tech Stack:** Node 22、TypeScript、Hono、`@hono/node-server`、better-auth、better-sqlite3、Vitest、oxlint。

## Global Constraints

- 独立仓库路径：`/Users/jon/Mangguo/disaster-alert-bff`（与 `disaster-alert`、`disaster-alert-web` 同级）。
- 监听端口 `30012`。
- 登录态：HttpOnly、Secure（生产）、SameSite=Lax session cookie；浏览器不把 Bark token 当身份。
- 手机号：中国大陆 11 位数字；内部规范成 `+86` + 11 位再交给 Better Auth。
- OTP：发送间隔 ≥ 60 秒；每小时 ≤ 5 次；连续校验失败 5 次作废；`AUTH_MOCK=true` 时固定码 `000000`。
- 微信：未配置 AppID 且 `AUTH_MOCK=true` 才提供模拟确认；生产 `AUTH_MOCK=false`。
- Bark token：22 位字母或数字；只接受主动输入，不解析测试链接。
- `GET {BARK_BASE_URL}/register/{token}` 仅 200 才写入设备。
- 设备表无 `base_url`；BFF 用环境变量 `BARK_BASE_URL` 填 Rust `destination.base_url`。
- token 密文 AES-256-GCM；`token_hash` HMAC-SHA256 做全局唯一。
- 同一 token 已属本账号则幂等；已属他人则拒绝且不泄露对方。
- 解绑：先 Rust 退订成功，再删资产。
- 补绑手机/微信：已被另一账号占用则不合并。
- 日志不得出现验证码、Bark token、微信 `code`。
- 飞书、运营 `/api/admin/*`、网页改版不在本计划。

## File map

本仓新建（相对 `disaster-alert-bff/`）：

| 文件 | 职责 |
| --- | --- |
| `package.json` | 脚本与依赖 |
| `tsconfig.json` | ESM、strict |
| `vitest.config.ts` | Node 环境测试 |
| `.env.example` | 配置样例，无密钥 |
| `.gitignore` | `node_modules`、`.env`、`data/` |
| `src/config.ts` | 环境变量 |
| `src/app.ts` | Hono 应用 |
| `src/server.ts` | listen |
| `src/auth/index.ts` | Better Auth 实例 |
| `src/auth/phone.ts` | 11 位校验与 `+86` 规范化 |
| `src/sms/mock.ts` | mock 发送（记录最后一次，测试用） |
| `src/sms/rate-limit.ts` | 60s / 每小时 5 次 |
| `src/crypto/device-token.ts` | AES-GCM + HMAC |
| `src/bark/register.ts` | `GET /register/{token}` |
| `src/devices/store.ts` | SQLite 设备表 |
| `src/devices/routes.ts` | `/api/devices` |
| `src/rust/client.ts` | 调 disaster-alert |
| `src/subscribe-proxy/routes.ts` | `/api/devices/:id/subscribe` 等 |
| `src/wechat/mock.ts` | mock 票据 |
| `src/settings/routes.ts` | 补绑 |
| `src/app.test.ts` 及各模块 `*.test.ts` | 测试 |

---

### Task 1: 脚手架与健康检查

**Files:**
- Create: `/Users/jon/Mangguo/disaster-alert-bff/package.json`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/tsconfig.json`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/vitest.config.ts`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/.gitignore`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/src/app.ts`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/src/server.ts`
- Create: `/Users/jon/Mangguo/disaster-alert-bff/src/app.test.ts`

**Interfaces:**
- Consumes: 无
- Produces: `createApp(): Hono`；`GET /health` → `{ "ok": true }`；`npm test`、`npm run dev`

- [ ] **Step 1: 建仓目录并 git init**

```bash
mkdir -p /Users/jon/Mangguo/disaster-alert-bff
cd /Users/jon/Mangguo/disaster-alert-bff
git init -b main
```

若要用 GitHub（与另外两仓一致）：

```bash
gh repo create luyi2008/disaster-alert-bff --private --source=. --remote=origin
```

先不要 push，等有可运行的健康检查再推。

- [ ] **Step 2: 写 package.json / tsconfig / vitest / gitignore**

`package.json`：

```json
{
  "name": "disaster-alert-bff",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "scripts": {
    "dev": "node --watch --import tsx src/server.ts",
    "build": "tsc -p tsconfig.json",
    "start": "node dist/server.js",
    "test": "vitest run",
    "lint": "oxlint src"
  },
  "engines": {
    "node": ">=22"
  }
}
```

安装：

```bash
cd /Users/jon/Mangguo/disaster-alert-bff
npm install hono @hono/node-server better-auth better-sqlite3
npm install -D typescript tsx vitest oxlint @types/node @types/better-sqlite3
```

`tsconfig.json`：

```json
{
  "compilerOptions": {
    "target": "ES2023",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "strict": true,
    "outDir": "dist",
    "rootDir": "src",
    "skipLibCheck": true,
    "esModuleInterop": true,
    "verbatimModuleSyntax": true
  },
  "include": ["src"]
}
```

`vitest.config.ts`：

```ts
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    globals: false,
  },
});
```

`.gitignore`：`node_modules/`、`dist/`、`.env`、`data/`。

- [ ] **Step 3: 写失败测试**

`src/app.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import { createApp } from "./app.ts";

describe("health", () => {
  it("returns ok", async () => {
    const app = createApp();
    const res = await app.request("/health");
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ ok: true });
  });
});
```

- [ ] **Step 4: 跑测试确认失败**

```bash
cd /Users/jon/Mangguo/disaster-alert-bff
npm test
```

Expected: FAIL，`Cannot find module './app.ts'` 或 `createApp is not a function`。

- [ ] **Step 5: 最小实现**

`src/app.ts`：

```ts
import { Hono } from "hono";

export function createApp(): Hono {
  const app = new Hono();
  app.get("/health", (c) => c.json({ ok: true }));
  return app;
}
```

`src/server.ts`：

```ts
import { serve } from "@hono/node-server";
import { createApp } from "./app.ts";

const port = Number(process.env.PORT ?? "30012");
serve({ fetch: createApp().fetch, port }, () => {
  console.log(`disaster-alert-bff listening on ${port}`);
});
```

- [ ] **Step 6: 跑测试确认通过**

```bash
npm test
```

Expected: PASS。

- [ ] **Step 7: Commit**

```bash
git add package.json package-lock.json tsconfig.json vitest.config.ts .gitignore src
git commit -m "$(cat <<'EOF'
Add Hono app with a health endpoint.

EOF
)"
```

---

### Task 2: 配置加载

**Files:**
- Create: `src/config.ts`
- Create: `src/config.test.ts`
- Create: `.env.example`

**Interfaces:**
- Consumes: `process.env`
- Produces: `loadConfig(env: NodeJS.ProcessEnv): AppConfig`

```ts
export type AppConfig = {
  port: number;
  authMock: boolean;
  betterAuthSecret: string;
  betterAuthUrl: string;
  trustedOrigins: string[];
  sqlitePath: string;
  barkBaseUrl: string;
  wechatAppId: string | null;
  wechatAppSecret: string | null;
  disasterAlertBaseUrl: string;
  bffServiceToken: string;
  deviceTokenEncryptionKey: string;
};
```

- `AUTH_MOCK` 仅当值为 `true` 时为 true。
- `WECHAT_APP_ID` / `WECHAT_APP_SECRET` 都非空才视为已配置。
- `DEVICE_TOKEN_ENCRYPTION_KEY` 必须是 32 字节经 base64 解码后的密钥（32 bytes）；测试里用固定 32 字节再 base64。
- `BARK_BASE_URL`、`DISASTER_ALERT_BASE_URL` 去尾斜杠。
- `trustedOrigins` 来自 `TRUSTED_ORIGINS` 逗号分隔，默认 `http://127.0.0.1:5173`。

- [ ] **Step 1: 写失败测试**（缺 `BETTER_AUTH_SECRET` 应抛错；`AUTH_MOCK=true` 读出 true）
- [ ] **Step 2: npm test，确认失败**
- [ ] **Step 3: 实现 `loadConfig`**
- [ ] **Step 4: npm test 通过**
- [ ] **Step 5: 写 `.env.example`（值为空或占位，不含真实密钥）并 commit**

`.env.example` 字段：`PORT=30012`、`AUTH_MOCK=true`、`BETTER_AUTH_SECRET=`、`BETTER_AUTH_URL=http://127.0.0.1:30012`、`TRUSTED_ORIGINS=http://127.0.0.1:5173`、`SQLITE_PATH=./data/auth.sqlite`、`BARK_BASE_URL=https://bark.mangguo.cloud`、`WECHAT_APP_ID=`、`WECHAT_APP_SECRET=`、`DISASTER_ALERT_BASE_URL=http://127.0.0.1:30010`、`BFF_SERVICE_TOKEN=`、`DEVICE_TOKEN_ENCRYPTION_KEY=`。

Commit message: `Load BFF config from the environment.`

---

### Task 3: 手机号规范化与 OTP 限流

**Files:**
- Create: `src/auth/phone.ts`
- Create: `src/auth/phone.test.ts`
- Create: `src/sms/rate-limit.ts`
- Create: `src/sms/rate-limit.test.ts`
- Create: `src/sms/mock.ts`

**Interfaces:**
- Consumes: 无
- Produces:

```ts
export function normalizeMainlandPhone(raw: string): string | null;
// 仅接受 11 位、以 1 开头；返回 "+86" + 11 位。带 +86 前缀也可以。非法返回 null。

export type SmsRateLimiter = {
  allowSend(phone: string, nowMs: number): { ok: true } | { ok: false; reason: "interval" | "hourly" };
};

export function createSmsRateLimiter(options?: {
  intervalMs?: number;
  hourlyLimit?: number;
}): SmsRateLimiter;
// 默认 intervalMs=60_000，hourlyLimit=5。按规范化后的号码计数。

export type MockSms = {
  sendOTP: (args: { phoneNumber: string; code: string }) => void;
  last: { phoneNumber: string; code: string } | null;
};
export function createMockSms(): MockSms;
```

- [ ] **Step 1: 测试**
  - `13812345678` → `+8613812345678`
  - `+8613812345678` → 同样
  - `138`、`12345678901`（非 1 开头）→ `null`
  - 限流：同一号 0ms 允许，1ms 拒绝 interval；6 次/小时第 6 次 hourly
  - mock：`sendOTP` 后 `last` 等于入参
- [ ] **Step 2: 跑测试失败**
- [ ] **Step 3: 实现**
- [ ] **Step 4: 测试通过**
- [ ] **Step 5: Commit** `Add phone normalization, OTP rate limits, and mock SMS.`

---

### Task 4: Better Auth + 手机 OTP

**Files:**
- Create: `src/auth/index.ts`
- Create: `src/auth/session.ts`
- Modify: `src/app.ts`
- Create: `src/auth/otp.test.ts`

**Interfaces:**
- Consumes: `AppConfig`、`createMockSms`、`createSmsRateLimiter`、`normalizeMainlandPhone`
- Produces: `createAuth(config, deps: { sendOTP: ...; sqlitePath: string }): Auth`；Hono 挂载 `app.on(["POST","GET"], "/api/auth/*", (c) => auth.handler(c.req.raw))`；`requireSession(c)` 无 cookie 返回 401 JSON `{ success: false, message: "未登录" }`

`phoneNumber` 插件：

- `otpLength: 6`
- `expiresIn: 300`
- `allowedAttempts: 5`
- `phoneNumberValidator`: `normalizeMainlandPhone(n) !== null`
- `signUpOnVerification.getTempEmail`: `(phone) => ${phone.replace("+","")}@users.disaster-alert.invalid`
- `sendOTP`：先 `normalize`；限流失败则抛可映射为 429 的错误；`config.authMock` 时若 code 将由 Better Auth 生成——**不要改 Better Auth 生成的码**。固定码 `000000` 用 `verifyOTP`：当 `authMock` 且用户提交 `000000` 时返回 true。真实发送走 `deps.sendOTP`（mock 写入 `last`，生产以后接阿里云）。
- `emailAndPassword.enabled: false`

SQLite 文件目录不存在时 `mkdirSync`。测试使用临时目录里的 sqlite 文件。

测试（`src/auth/otp.test.ts`）用 `createApp` + 真实 auth，内存/临时 sqlite：

1. `POST /api/auth/phone-number/send-otp` body `{ "phoneNumber": "13812345678" }` → 200；mock `last.phoneNumber` 为 `+8613812345678`。
2. 立刻再发 → 429，body 提示等待。
3. 错误验证码 → 不建 session。
4. `AUTH_MOCK` 下用 `000000` verify → Set-Cookie，随后 `GET /api/auth/get-session` 有 user。

CORS：`hono/cors`，`origin` 为 `config.trustedOrigins`，`credentials: true`，覆盖 `/api/*`。

Cookie：`advanced.defaultCookieAttributes`: `{ httpOnly: true, sameSite: "lax", secure: config.betterAuthUrl.startsWith("https:") }`。

- [ ] **Step 1–5:** 红、实现、绿、commit `Add Better Auth phone OTP with mock verification.`

若 `npx auth@latest migrate` 需要交互，改在 `createAuth` 首次启动时用 Better Auth 的 `getMigrations` / 官方 migrate API 自动建表；测试夹具调用同一函数。以当前 better-auth 文档的 sqlite `new Database(path)` 为准。

---

### Task 5: 设备 token 加密

**Files:**
- Create: `src/crypto/device-token.ts`
- Create: `src/crypto/device-token.test.ts`

**Interfaces:**

```ts
export function assertBarkTokenFormat(token: string): boolean;
// /^[A-Za-z0-9]{22}$/

export type DeviceTokenCrypto = {
  encrypt(plain: string): string; // base64(nonce|ciphertext|tag)
  decrypt(payload: string): string;
  hash(plain: string): string; // hex hmac-sha256
};

export function createDeviceTokenCrypto(key32: Buffer): DeviceTokenCrypto;
```

- [ ] 测试：22 位通过、21 位失败；同一明文两次 `encrypt` 密文不同，都能 `decrypt`；`hash` 稳定且不同明文不同。
- [ ] Commit `Encrypt Bark tokens and hash them for uniqueness.`

---

### Task 6: Bark register 客户端

**Files:**
- Create: `src/bark/register.ts`
- Create: `src/bark/register.test.ts`

**Interfaces:**

```ts
export type BarkRegisterResult =
  | { kind: "registered" }
  | { kind: "unregistered" }
  | { kind: "unavailable" };

export type BarkRegisterClient = {
  check(token: string): Promise<BarkRegisterResult>;
};

export function createBarkRegisterClient(options: {
  baseUrl: string;
  fetch?: typeof fetch;
}): BarkRegisterClient;
```

`check`：`GET ${baseUrl}/register/${encodeURIComponent(token)}`。200 → `registered`；4xx → `unregistered`；网络错或 5xx → `unavailable`。不要把 token 打进错误日志，只打 status。

测试用注入的 `fetch` mock，不要打真 Bark。

- [ ] Commit `Check Bark tokens via GET /register/:token.`

---

### Task 7: 设备表与绑定路由

**Files:**
- Create: `src/devices/store.ts`
- Create: `src/devices/store.test.ts`
- Create: `src/devices/routes.ts`
- Create: `src/devices/routes.test.ts`
- Modify: `src/app.ts`

**Interfaces:**

```ts
export type DeviceRecord = {
  id: string;
  userId: string;
  name: string;
  createdAt: number;
  updatedAt: number;
};

export type DeviceStore = {
  list(userId: string): DeviceRecord[];
  get(userId: string, id: string): DeviceRecord | null;
  getToken(userId: string, id: string): string | null; // 解密后的明文，仅内部给 Rust 用
  bind(input: { userId: string; token: string; name?: string }): 
    | { ok: true; device: DeviceRecord; alreadyBound: boolean }
    | { ok: false; reason: "taken" };
  rename(userId: string, id: string, name: string): DeviceRecord | null;
  remove(userId: string, id: string): boolean;
};

export function createDeviceStore(db: Database.Database, crypto: DeviceTokenCrypto): DeviceStore;
```

建表 SQL：

```sql
CREATE TABLE IF NOT EXISTS devices (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  token_ciphertext TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_user_id ON devices(user_id);
```

`bind`：若 `token_hash` 已存在且 `user_id` 相同，返回该行 `alreadyBound: true`；其他 `user_id` 返回 `taken`。新行 `id = crypto.randomUUID()`，默认名 `设备${count+1}`。

HTTP（均需 session）：

- `GET /api/devices` → `{ success: true, data: { devices: DeviceRecord[] } }`（响应**不得**含 token）
- `POST /api/devices` body `{ "token": "..." }`  
  - 格式非法 400「Bark token 必须是 22 位字母或数字」  
  - bark `unregistered` 400「token 未在推送服务注册或无效」  
  - bark `unavailable` 503「稍后重试」  
  - `taken` 409，message 不含他人信息：「该 token 已绑定其他账号」  
  - 成功 200，含 `device`
- `PATCH /api/devices/:id` `{ "name": "..." }`
- `DELETE /api/devices/:id` 本任务先只删行；Task 9 再接 Rust 退订。他人 id → 404。

`routes.test.ts`：先走 OTP mock 登录拿到 Cookie，再绑定。Bark `fetch` 注入。

- [ ] Commit `Bind Bark tokens to the logged-in account.`

---

### Task 8: Rust 客户端与订阅代理

**Files:**
- Create: `src/rust/client.ts`
- Create: `src/rust/client.test.ts`
- Create: `src/subscribe-proxy/routes.ts`
- Create: `src/subscribe-proxy/routes.test.ts`
- Modify: `src/app.ts`

**Interfaces:**

```ts
export type RustClient = {
  subscribe(body: unknown): Promise<{ status: number; json: unknown }>;
  unsubscribe(destination: { type: "bark"; base_url: string; device_key: string }): Promise<{ status: number; json: unknown }>;
  getSubscriptions(deviceKey: string): Promise<{ status: number; json: unknown }>;
  getDeliveries(deviceKey: string): Promise<{ status: number; json: unknown }>;
  simulate(body: unknown): Promise<{ status: number; json: unknown }>;
};

export function createRustClient(options: {
  baseUrl: string;
  serviceToken: string;
  fetch?: typeof fetch;
}): RustClient;
```

调用现状（disaster-alert OpenAPI）：

- `POST ${baseUrl}/api/subscribe` JSON 原样（BFF 在代理层写入 `destination`）
- `DELETE ${baseUrl}/api/unsubscribe` JSON `{ destination }`
- `GET ${baseUrl}/api/subscriptions` header `Authorization: Bearer ${deviceKey}`
- `GET ${baseUrl}/api/deliveries` 同上
- `POST ${baseUrl}/api/simulate` JSON（BFF 填入该设备 destination / key，字段与现网 simulate 请求一致）

每个请求额外带 `X-BFF-Service-Token: ${serviceToken}`（Rust 本计划可忽略）。超时 15s → 代理返回 502「订阅服务暂时无法连接」。

浏览器路由（需 session；设备必须属于当前用户，否则 404）：

- `POST /api/devices/:id/subscribe` body 为现网 `SubscribeRequest` **去掉 destination**（`targets` + `alerts`）。BFF 补：

```ts
{
  destination: { type: "bark", base_url: config.barkBaseUrl, device_key: token },
  targets,
  alerts,
}
```

- `DELETE /api/devices/:id/subscribe`
- `GET /api/devices/:id/subscription`
- `GET /api/devices/:id/deliveries`
- `POST /api/devices/:id/simulate` body 为现网 simulate 去掉设备身份字段，由 BFF 填入

未登录 401。Rust 4xx/2xx JSON 原样转发（含 `success` / `message`）。Rust 挂 502。

`routes.test.ts`：mock `fetch` 断言发往 Rust 的 JSON 含固定 `base_url` 与解密出的 token；请求 Cookie 在、token 不出现在浏览器请求 URL 里。

- [ ] Commit `Proxy per-device subscription calls to disaster-alert.`

---

### Task 9: 解绑前先退订

**Files:**
- Modify: `src/devices/routes.ts`
- Modify: `src/devices/routes.test.ts`

**Interfaces:** `DELETE /api/devices/:id` 改为：调用 `rust.unsubscribe`；若 status ≥ 500 或网络失败，不删资产，返回 502；2xx（含「订阅不存在」类业务成功/200 success:false）视为可解绑，再 `store.remove`。

- [ ] 测试：Rust 500 时 GET 设备仍在；Rust 200 后 GET 404。
- [ ] Commit `Unsubscribe in Rust before removing a device.`

---

### Task 10: 微信 mock 扫码

**Files:**
- Create: `src/wechat/mock.ts`
- Create: `src/wechat/mock.test.ts`
- Modify: `src/auth/index.ts`（配置了 AppID 时启用 `socialProviders.wechat`）
- Modify: `src/app.ts`

**Interfaces:**

仅 `config.authMock === true` 时挂载：

- `POST /api/auth/mock/wechat/ticket` → `{ success: true, data: { ticketId } }`，TTL 5 分钟
- `POST /api/auth/mock/wechat/confirm` body `{ ticketId }`：创建或找回 `account.providerId = "wechat"` 且 `accountId = "mock:" + ticket 对应的稳定 openid` 的用户（每次新 ticket 新 openid，用 `ticketId` 本身当 mock openid），然后 `auth.api` 创建 session 并 Set-Cookie
- 过期 ticket → 400，不建用户

真微信：`WECHAT_APP_ID` 与 secret 都有时：

```ts
socialProviders: {
  wechat: {
    clientId: config.wechatAppId,
    clientSecret: config.wechatAppSecret,
    lang: "cn",
  },
}
```

`AUTH_MOCK=false` 且无 AppID：不要挂 mock 路由；社交按钮由前端隐藏（本计划只保证接口 404）。

Session 创建用 `auth.$context` 的 `internalAdapter.createUser` / `createAccount` / `createSession`（以当前 better-auth 版本 API 为准；若有 `auth.api.signInSocial` 可测通的等价物则用之）。测试只覆盖 mock 路径。

- [ ] Commit `Add WeChat mock QR confirmation and optional WeChat OAuth.`

---

### Task 11: 设置里补绑手机号

**Files:**
- Create: `src/settings/routes.ts`
- Create: `src/settings/routes.test.ts`
- Modify: `src/app.ts`

**Interfaces:**

已登录：

- `POST /api/settings/phone/send-otp` `{ phoneNumber }` — 复用限流与 sendOTP
- `POST /api/settings/phone/verify` `{ phoneNumber, code }` — 调用 `auth.api.verifyPhoneNumber` 且 `updatePhoneNumber: true`

若该手机号已是另一用户：Better Auth 会报错；映射为 409「已在其他账号使用」，不合并。微信补绑：已登录用户走 Better Auth 标准 link social（真微信）；mock 下 `POST /api/settings/mock/wechat/confirm` 把 mock wechat account 链到**当前** user，若 mock openid 已被别人占用则 409。

- [ ] 测试：账号 A 占用 `13800000000` 后，账号 B 补绑同一号 409。
- [ ] Commit `Let signed-in users link a phone number without merging accounts.`

---

### Task 12: Docker 与 README

**Files:**
- Create: `Dockerfile`
- Create: `README.md`

Dockerfile：`node:22-bookworm-slim`，`npm ci && npm run build`，`CMD node dist/server.js`，`EXPOSE 30012`，数据目录 `/app/data`。README 用中文写清：职责、端口、与两个兄弟仓的关系、指向 `disaster-alert` 仓 spec `docs/superpowers/specs/2026-09-01-account-login-design.md`、本地 `AUTH_MOCK=true` 流程（OTP `000000`、模拟微信）。

- [ ] `docker build` 能通过（不要求本机跑通真微信）。
- [ ] Commit `Add Docker image and README for the BFF.`

---

## 本计划结束后仍属后续仓

- `disaster-alert`：写接口只认 `BFF_SERVICE_TOKEN`，拒绝浏览器 Bearer Bark token。
- `disaster-alert-web`：`/login`、`/devices`、Vite 拆代理。
- 主机反代把 `/api/auth` 与设备/订阅代理指到 30012。
