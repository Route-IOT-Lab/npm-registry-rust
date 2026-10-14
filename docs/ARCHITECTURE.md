# 架构说明

本文说明 `npm-registry-rust` 的分层、数据模型、认证模型，以及几个关键取舍。

---

## 1. 整体形态

一个进程、一个二进制、两个目录：

```
                 ┌──────────────────────────────────────┐
   npm CLI ─────▶│                                      │
                 │            axum Router               │
   浏览器  ─────▶│  ┌────────┬──────────┬────────────┐  │
                 │  │  ui    │   api    │    npm     │  │
                 │  └───┬────┴────┬─────┴─────┬──────┘  │
                 │      │         │           │         │
                 │      ▼         ▼           ▼         │
                 │  ┌────────────────────────────────┐  │
                 │  │          service 层            │  │  ← 业务规则
                 │  │  packages / users / tokens     │  │
                 │  └───────┬────────────────┬───────┘  │
                 │          │                │          │
                 │          ▼                ▼          │
                 │    SQLite (sqlx)    本地文件系统      │
                 │    ./data/*.db      ./storage/**.tgz │
                 └──────────────────────────────────────┘
```

没有外部依赖：不需要单独的数据库、对象存储或缓存服务。这让「私有仓库」这件事的运维成本降到最低。

---

## 2. 分层约定

| 层 | 目录 | 职责 | 禁止 |
| --- | --- | --- | --- |
| 路由 | `routes/` | 解析 HTTP、调用服务、组织响应 | 写 SQL、写业务分支 |
| 服务 | `service/` | 业务规则、编排、事务边界 | 引用 `Request` / `Response` |
| 数据 | `db.rs` / 内联查询 | 连接池、迁移、查询 | 承载业务判断 |

这条边界是刻意的：`service::packages::upsert_packument` 不知道调用它的是 npm CLI 还是管理界面，因此两个入口共享同一套规则（权限、完整性校验、dist-tag 默认值），不会出现「网页上删得掉、CLI 删不掉」这类偏差。

`main.rs` 是唯一的装配点——它读配置、初始化日志与数据库、拼装 Router、装中间件，然后 `serve`。除此之外不写逻辑。

---

## 3. 数据模型

```
users ─┬─< tokens            访问令牌（只存哈希）
       ├─< packages          包（owner_id 指向首个发布者）
       └─< maintainers >──┐
                          │
packages ─┬─< versions   版本（含完整 manifest 原文）
          ├─< dist_tags  标签 → 版本
          └─< maintainers
```

几处值得说的设计：

**版本 manifest 原样存 JSON。** `versions.manifest` 保存 npm 提交的版本对象原文，读取时再动态改写 `dist.tarball` / `dist.shasum` / `dist.integrity`。好处是：域名变了不用刷库，而 npm 提交的那些字段（`_npmVersion`、`_nodeVersion`、`engines`、`bin`、`dependencies`……）能一字不差地回给客户端。

**tarball 元数据独立成列。** `shasum` / `integrity` / `size` 单独存，而不是只放在 manifest JSON 里。这样列表页统计体积、校验下载、以及 `npm install` 的一致性校验都不需要解析 JSON。

**dist_tags 独立成表。** 而不是塞进 packages 的一个 JSON 列。因为 `latest` 的重算（删版本后）、`npm dist-tag` 的增删都要按 tag 精确操作。

**时间统一为 RFC3339 UTC 字符串。** SQLite 没有原生时间类型，统一格式比用整数时间戳更好排查问题。

**库里只有「本仓库发布」的包。** 上游代理不落库（原因见第 8 节），所以 `packages` 表天然就是
这个仓库实际提供的内容，没有任何「这份数据是别人家的」这种模糊状态。

---

## 4. 认证模型

### 三条凭据通道

请求进来时，`AuthUser` / `AdminUser` / `MaybeAuthUser` 三个提取器按固定优先级解析身份：

1. `Authorization: Bearer npmp_…` — npm CLI 使用的**不透明令牌**
2. `Authorization: Basic <base64(user:pass)>` — 兼容 curl / 老工具
3. `Cookie: npm_registry_session=<jwt>` — 网页会话

令牌**不透明**而非 JWT，是有意为之：npm 客户端不会刷新令牌，所以令牌必须长期有效且能立刻吊销。JWT 做不到即时吊销（除非维护黑名单，那还不如直接查库）。代价是每个带令牌的请求多一次索引查询——对私有仓库的流量规模来说可以忽略。

不透明令牌存储的是 **SHA-256 哈希**：拖库拿不到可用凭据，`.npmrc` 泄露也能在界面上精确吊销某一个。

网页会话则用 JWT + HttpOnly Cookie：无需服务端会话表，短时效，退出即清空。

### 权限规则

| 操作 | 谁可以 |
| --- | --- |
| 读取（packument / tarball） | 任意已认证用户 |
| 新建包 | 任意已认证用户（成为 owner） |
| 向已存在的包发布 | owner、`maintainers` 表中的用户、管理员 |
| 改 dist-tag / 删版本 / 删包 | owner、管理员 |
| 用户管理、`/api/stats` | 管理员 |
| 吊销自己的令牌 | 本人 |

「首个发布者成为 owner」是刻意选择的默认值，省掉了预建 ACL 的运维负担。目前没有提供转移 owner 的界面，需要时直接改 `packages.owner_id`。

---

## 5. 关键取舍

**为什么用 SQLite 而不是 Postgres？**
私有 npm 仓库的写入量极小（一次 `npm publish` 一次写），读取是点查。SQLite 的 WAL 模式完全够用，而「不需要额外服务」对私有化交付是巨大的优势。`sqlx` 的抽象层保留了换 Postgres 的空间——主要工作是替换连接池构造与少量 SQLite 方言（`INSERT OR IGNORE`、`ON CONFLICT` 语法基本通用）。

**为什么 UI 用 `include_str!` 内嵌？**
部署形态从「二进制 + 一堆静态文件」变成「一个二进制」。代价是改一行 CSS 要重新编译——对后端项目来说这是可以接受的交易。

**为什么包路径不打成路由？**
scoped 包名 `@scope/name` 自带斜杠，`/{package}` 与 `/{package}/-/{filename}` 在同一个路由树里必然产生歧义。所以 `/-/` 前缀的端点显式注册，其余全部交给 `dispatch` 兜底，用与 registry 规范一致的规则手工切分路径。这样 `@scope/name@1.0.0`、`/@scope%2fname`（npm 有时会编码斜杠）两种情况都能处理。

**为什么 tarball 允许覆盖写？**
`npm publish` 重发同一版本时，覆盖写比报 409 更符合直觉（内容不同则更新，相同则幂等）。版本号相同内容不同属于用户失误，仓库层面不做拦截。

---

## 6. 请求生命周期

以 `npm publish` 为例：

```
1. PUT /hello-npmreg
2. TraceLayer        → 生成请求 ID，记录耗时
3. RequestBodyLimit  → 超过 MAX_PUBLISH_BODY_MIB 直接 413
4. fallback dispatch → 解析路径 → PackagePath::Packument("hello-npmreg")
5. MaybeAuthUser     → Bearer → 查 tokens → 载入 user
6. service::upsert_packument
   ├─ 校验包名 / semver
   ├─ 查包是否存在 → ensure_can_publish（权限）
   ├─ 解码 _attachments（base64）
   ├─ 逐版本计算 shasum(sha1) / integrity(sha512)、写盘
   ├─ BEGIN TRANSACTION
   │   ├─ upsert packages
   │   ├─ upsert versions × N
   │   └─ upsert dist_tags（缺失时补 latest = 最高 semver）
   └─ COMMIT
7. 201 {"ok":true}
```

注意第 6 步的顺序：**先写盘再开事务**。数据库事务回滚不了文件系统，反过来则可以接受——最坏情况是留下一个孤儿 tarball，不影响正确性，且下一版本的元数据会覆盖它。

---

## 7. 前端

`static/` 下三个文件，无构建步骤、无依赖：

- `index.html` — 外壳
- `style.css` — 深色主题
- `app.js` — 状态 + 渲染 + 事件委托

单页应用的所有状态在一个 `state` 对象里，`render()` 按状态重建 DOM，事件通过 `data-action` 统一委托到两个监听器（`submit` 与 `click`）。用户可控内容一律经 `esc()` 转义。

这是刻意保守的写法：没有框架版本升级的负担，任何人打开 `app.js` 就能改。

---

## 8. 上游代理

本地没有的包会按配置顺序向上游查询。这条回退链的语义是整个特性的核心：

| 上游返回 | 行为 |
| --- | --- |
| `200` | 命中，停止查找 |
| `404` | 明确「没有这个包」→ 试下一个 |
| 其他（网络错误 / 5xx / 401…） | 上游故障 → 试下一个，并记下失败 |

最终判定：

- 至少一个上游明确回 404，且没有上游命中 → `404`
- 所有上游都失败 → `502`

**为什么坚持区分这两种**：上游全挂时返回 `404`，`npm install` 会以「包不存在」收场。
把一个可恢复的运维故障伪装成事实错误，是最难排查的一类问题。所以这里用 `AppError::Upstream`
（对应 `502 Bad Gateway`），并在消息里带上每个上游各自的失败原因。

### 透传，而不是缓存

最早这一块是「抓取上游 packument → 解析成 `serde_json::Value` → 存库 → 之后从库里组装」。
这是最自然的写法，在普通机器上也没问题，但它不适合小机器，而且是可以量化的：

| 包 | packument JSON | 解析后的常驻成本 |
| --- | --- | --- |
| `vue` | 2.2 MB | +34 MB |
| `typescript` | 15 MB | ≈150 MB |

`serde_json::Value` 是重量级表示——每个字符串、每个对象都独立成节点——所以一次安装只要碰到几个大
packument，一台 2 核 1.7 GB 的机器就会被顶到内存上限。改用 npm 的「精简 packument」
（约小一半）也没有改变数量级。

于是改成透传：上游响应体直接流给客户端，全程不物化。代价是本地没有元数据缓存，
可接受——npm 自带内容寻址缓存，同一个人重复安装不会再打到 registry。

### 顺带的两个结果

**tarball 不经过本机。** packument 原样透传，`dist.tarball` 仍指向上游 CDN。
对一台出口带宽有限的机器来说，这比把字节也代理过来更合适。

**回退链保留在上游请求之前。** 只有确认某个上游返回 200，才把它的响应交给客户端；
404 则丢弃该响应继续找下一个。因此在开始推流之前，状态码就已经确定，
不会出现「已经把上游的 404 页面流给客户端了才发现要回退」。

### 与本地包的关系

**本地永远优先。** 名字只要在本仓库存在，就完全由本地数据定义，
不会与上游内容合并——一个 packument 里混着「上游的 X 和本地的 Y」会让
「这个仓库到底提供什么」变得无法预测。

---

## 9. 已知边界

- 只有 SQLite 后端（schema 已按可移植方式书写）
- 无 scope / 包级 ACL，权限模型是 owner + maintainers
- `npm owner add/rm` 会被接受但不生效（PUT 只更新元数据，不处理 maintainers 字段）
- 无 webhook、无审计日志查询界面
- 单实例部署：tarball 存储与 SQLite 都假定了单写者
- **上游代理不缓存也不代理 tarball**（见第 8 节）。公共包每次查询都会打到上游，
  且 tarball 直连上游 CDN。若某天需要单出口或离线能力，得另做一套不物化文档的方案
- 上游请求不带凭据，因此只适合公共上游；要代理需要鉴权的私有仓库还需另做
