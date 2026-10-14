# API 参考

两套接口共用一个端口：

- **注册表协议**（`/-/…` 与包路径）— npm CLI 消费，字段遵循 npm registry / CouchDB 约定
- **管理 API**（`/api/…`）— 管理界面消费，纯 JSON

所有错误响应统一为：

```json
{ "error": "人类可读的说明", "reason": "同上" }
```

认证方式见 [架构说明 · 认证模型](ARCHITECTURE.md#4-认证模型)。

---

## 一、注册表协议

### 包数据

#### `GET /:package`

返回完整 packument。

```bash
curl http://localhost:4873/hello-npmreg \
  -H "Authorization: Bearer $TOKEN"
```

```json
{
  "_id": "hello-npmreg",
  "_rev": "1-0",
  "name": "hello-npmreg",
  "description": "…",
  "dist-tags": { "latest": "1.0.0" },
  "versions": {
    "1.0.0": {
      "name": "hello-npmreg",
      "version": "1.0.0",
      "dist": {
        "tarball": "http://localhost:4873/hello-npmreg/-/hello-npmreg-1.0.0.tgz",
        "shasum": "faf5a3…",
        "integrity": "sha512-q87pFT…"
      }
    }
  },
  "time": { "created": "…", "modified": "…", "1.0.0": "…" },
  "maintainers": [{ "name": "admin", "email": "admin@example.com" }],
  "_attachments": {}
}
```

`dist.tarball` 在响应时才组装，因此修改 `REGISTRY_BASE_URL` 后无需刷库。

#### `GET /:package/:version`

单个版本 manifest（结构同上，不含 `dist-tags` / `time`）。

#### `GET /:package/-/:filename`

下载 tarball。

| 响应头 | 说明 |
| --- | --- |
| `Content-Type` | `application/octet-stream` |
| `ETag` | tarball 的 sha1，即 `shasum` |
| `Cache-Control` | `public, max-age=31536000, immutable` |

支持 `HEAD`（返回同样的响应头，空 body）。每次成功下载会累加包的与版本的下载计数。

### 发布与撤回

#### `PUT /:package`

发布。请求体为带 `_attachments` 的 packument（npm 自身发出的格式）：

```json
{
  "name": "hello-npmreg",
  "dist-tags": { "latest": "1.0.0" },
  "versions": { "1.0.0": { "name": "…", "version": "1.0.0", "dist": { "tarball": "…" } } },
  "_attachments": {
    "hello-npmreg-1.0.0.tgz": {
      "content_type": "application/octet-stream",
      "data": "<base64>",
      "length": 309
    }
  }
}
```

成功返回 `201`：

```json
{ "ok": true, "id": "hello-npmreg", "rev": "1-0" }
```

行为要点：

- 没有 `_attachments` 时视为**元数据更新**（`npm deprecate` / 老版 `dist-tag` 走这条路径），此时版本必须已存在。
- 版本号必须满足 semver；包名必须符合 npm 命名规则。
- 已存在的版本会被覆盖。
- 只有 owner / maintainer / 管理员可以发布到已存在的包。

#### `PUT /:package/-rev/:rev`

与 `PUT /:package` 等价。`:rev` 被忽略（仓库不做乐观锁）。

#### `DELETE /:package/-rev/:rev`

撤回。

```bash
# 撤回单个版本（body 里列出要删的版本）
curl -X DELETE http://localhost:4873/hello-npmreg/-rev/1-0 \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"versions":{"1.0.0":{}}}'

# 撤回整包（body 为空或省略 versions）
curl -X DELETE http://localhost:4873/hello-npmreg/-rev/1-0 \
  -H "Authorization: Bearer $TOKEN"
```

- body 含 `versions` 且非空 → 只删列出的版本；删空后整包一并移除。
- 否则整包删除（数据库行 + 磁盘目录）。
- 删除后会自动把 `latest` 重指向剩余的最高 semver。

### dist-tags

npm ≥ 7 使用这组独立端点，而不是回写整个 packument。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/-/package/:name/dist-tags` | 返回 `{"latest":"1.0.0","beta":"1.1.0-rc.1"}` |
| `GET` | `/-/package/:name/dist-tags/:tag` | 返回该 tag 的版本字符串 |
| `PUT` | `/-/package/:name/dist-tags/:tag` | body 为 JSON 字符串 `"1.0.0"` |
| `DELETE` | `/-/package/:name/dist-tags/:tag` | 删除该 tag |

tag 名不得是合法 semver（否则 `pkg@1.2.3` 会有歧义），不得含空白字符。

### 认证

#### `PUT /-/user/org.couchdb.user::name`

传统登录 / 注册（`npm login --auth-type=legacy`、`npm adduser`）。

```bash
curl -X PUT http://localhost:4873/-/user/org.couchdb.user:alice \
  -H 'Content-Type: application/json' \
  -d '{"name":"alice","password":"…","email":"alice@example.com","type":"user"}'
```

```json
{ "ok": true, "token": "npmp_…" }
```

- 用户不存在且 `ALLOW_REGISTRATION=true` → 创建用户，返回 `201`。
- 用户不存在且未开放注册 → `403`。
- 密码错误 → `401`。

#### `POST /-/v1/login`

浏览器授权流的第一步（npm 默认登录方式）。

```json
{ "loginUrl": "http://localhost:4873/login?cli=<session-id>",
  "doneUrl":  "http://localhost:4873/-/v1/login/<session-id>" }
```

#### `GET /-/v1/login/:id`

CLI 轮询的地址。

| 状态 | 响应 |
| --- | --- |
| 等待用户授权 | `202` `{}` |
| 已授权 | `200` `{"token":"npmp_…"}`，随后该 session 立即失效 |
| 不存在 / 已过期（10 分钟） | `404` |

#### `GET /-/whoami`

```json
{ "username": "alice" }
```

未认证返回 `401`。

### 令牌管理（npm CLI）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/-/npm/v1/tokens` | `npm token list` |
| `POST` | `/-/npm/v1/tokens` | `npm token create`，body 可带 `password` 做二次确认 |
| `DELETE` | `/-/npm/v1/tokens/token/:key` | `npm token revoke`，`:key` 即令牌 id |

### 其他

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/-/ping` | 存活探测，返回 `{}` |
| `GET` | `/-/v1/search?text=&size=&from=` | 搜索，返回 npm 的 `{objects,total,time}` 结构 |
| `POST` | `/-/npm/v1/security/audits/quick` | 审计占位实现，返回 0 漏洞 |
| `POST` | `/-/npm/v1/security/advisories/bulk` | 返回 `{}` |

> 本仓库不代理公共漏洞库。审计端点返回结构合法的空报告，让 `npm install` / `npm audit` 不至于在私有仓库上报错。

---

## 二、管理 API

除标注外均需认证。认证方式：会话 Cookie（浏览器）或 `Authorization: Bearer <token>`。

### 元信息

| 方法 | 路径 | 认证 | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/health` | 否 | `{status, name, version}` |
| `GET` | `/api/config` | 否 | `{registry_base_url, allow_registration, version}` |
| `GET` | `/api/stats` | 管理员 | `{users, packages, versions, downloads}` |

### 会话

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/api/auth/login` | body `{username,password}`，成功后下发 HttpOnly 会话 Cookie |
| `POST` | `/api/auth/logout` | 清空 Cookie |
| `POST` | `/api/auth/register` | 仅在 `ALLOW_REGISTRATION=true` 时可用 |
| `GET` | `/api/me` | 当前用户 |
| `POST` | `/api/me/password` | body `{current_password,new_password}` |

### 访问令牌

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/me/tokens` | 列出本人令牌（只显示前缀） |
| `POST` | `/api/me/tokens` | body `{name,ttl_days?}`；**响应中的 `token` 是唯一一次明文** |
| `DELETE` | `/api/me/tokens/:id` | 吊销 |

```bash
curl -X POST http://localhost:4873/api/me/tokens \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"ci-runner","ttl_days":90}'
```

```json
{ "id": 7, "name": "ci-runner", "token": "npmp_…", "expires_at": "2027-01-12T07:00:00Z" }
```

### 包

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/packages?q=&limit=&offset=` | 列表，`q` 匹配包名与描述 |
| `GET` | `/api/packages/:name` | 详情，含全部版本与 tarball 校验值 |
| `DELETE` | `/api/packages/:name` | 删除整包 |
| `DELETE` | `/api/packages/:name?version=1.0.0` | 删除指定版本 |

`:name` 需 URL 编码（`@acme/widget` → `%40acme%2Fwidget`）。

列表项：

```json
{ "name": "hello-npmreg", "description": "…", "latest": "1.0.0",
  "versions": 3, "downloads": 42, "owner": "admin",
  "license": "MIT",
  "created_at": "…", "updated_at": "…" }
```

> 列表里只有本仓库发布的包。上游代理不落库，所以这里不会出现公共包。

### 用户（管理员）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/users` | 列表 |
| `POST` | `/api/users` | body `{username,email?,password,is_admin?}` |
| `DELETE` | `/api/users/:id` | 删除；不能删自己，也不能删掉最后一个管理员 |
| `POST` | `/api/users/:id/password` | 重置密码，同时吊销该用户的所有令牌 |

### 命令行授权

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/api/cli/authorize` | body `{session_id}`；界面在用户点击「授权」时调用 |

---

## 三、界面资源

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 管理界面 |
| `GET` | `/login` | 同上（供 CLI 授权链接直达） |
| `GET` | `/app.js` | 界面脚本 |
| `GET` | `/style.css` | 界面样式 |
| `GET` | `/favicon.ico` | `204` |

---

## 四、状态码约定

| 码 | 何时 |
| --- | --- |
| `200` | 读取成功、更新成功 |
| `201` | 发布成功、创建用户 / 令牌成功 |
| `202` | CLI 登录轮询，尚未授权 |
| `400` | 包名非法、semver 非法、base64 损坏、缺少必填字段 |
| `401` | 未认证、令牌过期、密码错误 |
| `403` | 无权限、账号被禁用、未开放注册 |
| `404` | 包 / 版本 / tag / 会话不存在 |
| `409` | 用户名冲突、登录会话已被处理 |
| `413` | 请求体超过 `MAX_PUBLISH_BODY_MIB` |
| `502` | 所有上游都不可达，或上游 tarball 校验失败（见下） |
| `500` | 内部错误（响应体不含任何内部细节） |

---

## 五、上游代理

本地没有的包会按 `UPSTREAM_REGISTRIES` 的顺序向上游查询。对客户端而言这是透明的：
`GET /:package` 与 `GET /:package/:version` 的行为不变，只是内容可能由上游供货。

判定规则：

| 情况 | 响应 |
| --- | --- |
| 本地有 | 本地内容（本地永远优先） |
| 某上游命中 | 该上游的响应，**原样透传** |
| 所有上游都明确回 404 | `404` |
| 所有上游都失败（网络 / 5xx / 401…） | `502` |

**为什么上游全挂不是 404**：那会让 `npm install` 以「包不存在」收场，
把一个可恢复的故障伪装成事实错误。`502` 的响应体里会列出每个上游各自的失败原因，便于定位。

上游响应**不落库、不解析、不缓存**，字节流式转发。因此：

- 公共包的 `dist.tarball` 仍指向上游 CDN，**tarball 字节不经过本仓库**
- 重复查询同一公共包会再次打到上游（npm 客户端自身有缓存，通常感知不到）
- 该端点**没有** `/-/:filename` 代理：公共包的 tarball 不会由本仓库提供
