# npm-registry-rust

> 一个用 Rust 写的轻量、自托管的私有 npm registry。单二进制交付，自带管理界面。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![Docker](https://img.shields.io/badge/docker-ready-2496ED.svg)](docker-compose.yml)

`npm-registry-rust` 目标是做「私有 npm 仓库」这件事的最小完备实现：**协议兼容 npm CLI**、**开箱即用的认证**、**一个不折腾的管理界面**、**零外部依赖的部署形态**。不追求做 Verdaccio 的全功能替代，只把私有仓库真正会用到的那部分做扎实。

> 英文简介见文末 [English summary](#english-summary)。

---

## 特性

| 能力 | 说明 |
| --- | --- |
| **npm 协议兼容** | `npm publish` / `install` / `dist-tag` / `unpublish` / `search` / `whoami` 全流程 |
| **上游代理回退** | 本地没有的包按序向上游查询（淘宝源 → 官方源），404 才继续往下找；上游响应流式透传，内存占用与包大小无关 |
| **两种登录方式** | 浏览器授权流（`npm login` 默认）+ 传统 CouchDB 流（`--auth-type=legacy`） |
| **访问令牌** | `npmp_` 前缀、服务端仅存 SHA-256 哈希、可设有效期、可随时吊销 |
| **私有化认证** | 未登录不可读写；首个发布者成为包 owner，支持维护者与管理员 |
| **上传下载** | tarball 落本地磁盘，写入时计算 `shasum` / `integrity`，下载计数 |
| **管理界面** | 内嵌单页：包列表与详情、版本删除、令牌管理、用户管理、接入指引 |
| **单二进制** | UI 用 `include_str!` 打进可执行文件，部署 = 一个文件 + 一个目录 |
| **SQLite 存储** | 元数据入库，WAL 模式，迁移自动执行，无需单独装数据库 |
| **可观测** | 结构化 JSON 日志、请求 ID 贯穿、健康检查端点、优雅停机 |

---

## 快速开始

### 方式一：本地运行

```bash
git clone https://github.com/your-org/npm-registry-rust.git
cd npm-registry-rust

cp .env.example .env
# 至少改两处：JWT_SECRET 与 ADMIN_PASSWORD
openssl rand -hex 32   # 用它填 JWT_SECRET

cargo run --release
```

启动后打开 <http://localhost:4873>，用 `.env` 里的 `ADMIN_USERNAME` / `ADMIN_PASSWORD` 登录。

### 方式二：Docker Compose

```bash
cp .env.example .env
# 编辑 .env，然后：
docker compose up -d
```

数据落在 `./storage`（tarball）与 `./data`（SQLite），两个目录都已在 `.gitignore` 中。

### 方式三：Docker

```bash
docker build -t npm-registry-rust .
docker run -d --name npm-registry \
  -p 4873:4873 \
  -e JWT_SECRET="$(openssl rand -hex 32)" \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD='change-me-please' \
  -e REGISTRY_BASE_URL=http://localhost:4873 \
  -v "$PWD/storage:/app/storage" \
  -v "$PWD/data:/app/data" \
  npm-registry-rust
```

---

## 客户端使用

```bash
# 1. 指向私有仓库
npm config set registry http://localhost:4873

# 2. 登录（会打开浏览器授权；CI 场景请用令牌）
npm login --registry http://localhost:4873

# 3. 发布
cd your-package && npm publish

# 4. 安装
npm install your-package --registry http://localhost:4873

# 其他常用命令
npm dist-tag add your-package@1.2.3 beta
npm unpublish your-package@1.0.0
npm search your-package --registry http://localhost:4873
npm whoami --registry http://localhost:4873
```

### CI / 无人值守环境

在管理界面「访问令牌」页生成一个令牌，写进 `.npmrc`：

```
//your-registry.example.com/:_authToken=npmp_xxxxxxxxxxxxxxxx
```

然后 `npm publish` 直接可用，无需交互式登录。

### 只让某个 scope 走私有仓库

```bash
npm config set @your-scope:registry http://localhost:4873
```

---

## 上游代理

本仓库没有的包会自动向上游查询，所以**客户端只需要配一个 registry**——私有包和公共包走同一个地址。

回退规则（这是刻意的设计）：

| 上游返回 | 行为 |
| --- | --- |
| `200` | 命中，停止查找 |
| `404` | 该上游确实没有 → 试下一个 |
| 其他（网络错误 / 5xx / 401…） | 该上游故障 → 试下一个，但记下它失败了 |

- 只要**有一个上游明确回 404** 且没有任何上游命中 → 返回 `404`
- 如果**所有上游都失败** → 返回 `502`，而不是 `404`

第二点是关键：上游全挂时回 404 会让 `npm install` 以「这个包不存在」收场，把一个运维故障伪装成事实错误。宁可给出一个明确的网关错误。

上游响应是**原样透传**的，不落库、不解析。因此公共包的 `dist.tarball` 仍然指向上游 CDN——**tarball 字节不经过本机**。

这样做的原因不是省事，是内存：把完整 packument 解析成 `serde_json::Value` 并缓存，成本约为文档体积的 10 倍（`vue` 2.2MB 的 packument 要吃掉 34MB 常驻内存，`typescript` 15MB 的接近 150MB）。在一台 2 核 1.7GB 的机器上，一次正常安装就能把进程顶到内存上限。透传则与包大小无关。

代价是本地没有元数据缓存。实际影响很小——npm 自己有内容寻址缓存，同一个人重复安装根本不会再打到 registry。

**关闭代理**（纯私有仓库，未知包一律 404）：

```bash
UPSTREAM_REGISTRIES=
```

> 优先级：**本地永远优先**。名字只要在本仓库存在，就完全由本地定义，不会与上游内容合并。

---

## 配置

全部通过环境变量配置，启动时集中校验、快速失败。完整清单见 [`.env.example`](.env.example)。

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | 监听地址 |
| `PORT` | `4873` | 监听端口（npm 私有仓库惯例端口） |
| `REGISTRY_BASE_URL` | `http://localhost:$PORT` | 对客户端可见的公开地址，用于生成 tarball 链接 |
| `STORAGE_DIR` | `./storage` | tarball 存放目录 |
| `DATABASE_URL` | `sqlite://./data/registry.db?mode=rwc` | SQLite 连接串 |
| `JWT_SECRET` | **必填** | 会话 JWT 签名密钥，≥32 字节 |
| `SESSION_TTL_MINUTES` | `720` | 网页会话有效期 |
| `ADMIN_USERNAME` | — | 首次启动创建的管理员账号 |
| `ADMIN_PASSWORD` | — | 首次启动创建的管理员密码 |
| `ADMIN_EMAIL` | `admin@example.com` | 管理员邮箱 |
| `ALLOW_REGISTRATION` | `false` | 是否允许自助注册 |
| `MAX_PUBLISH_BODY_MIB` | `64` | 单次发布请求体上限 |
| `UPSTREAM_REGISTRIES` | 淘宝源,官方源 | 上游仓库链，**按序回退**；置空则关闭代理 |
| `UPSTREAM_TIMEOUT_SECONDS` | `30` | 上游请求超时 |
| `CORS_ALLOWED_ORIGINS` | 空 | 允许的跨域来源，逗号分隔；生产环境禁用 `*` |
| `RUST_LOG` | `info` | 日志级别 |
| `LOG_FORMAT` | `text` | `text` 或 `json` |

---

## 项目结构

```
src/
├── main.rs              # 唯一的装配点：配置 → 日志 → 数据库 → HTTP 服务
├── config.rs            # 环境变量加载与校验
├── error.rs             # 类型化错误 + 全局错误处理
├── state.rs             # 共享状态（配置 / 连接池 / 存储）
├── models.rs            # 数据库行模型与对外投影
├── auth.rs              # 密码哈希、JWT 会话、API 令牌、鉴权提取器
├── storage.rs           # 文件系统 tarball 存储（含路径穿越防护）
├── db.rs                # 连接池与迁移
├── service/             # 业务逻辑：packages / users / tokens
└── routes/
    ├── npm.rs           # npm 注册表协议
    ├── api.rs           # 管理 JSON API
    └── ui.rs            # 内嵌管理界面
static/                  # 管理界面源码（会被编译进二进制）
migrations/              # SQL 迁移，启动时自动执行
docs/                    # 架构 / API / 部署文档
```

分层约定：**路由层只做 HTTP 翻译，服务层承载业务规则，数据访问集中在 `service` 与 `db`**。路由里不写 SQL，服务层不碰 `Request` / `Response`。

---

## 文档

- [架构说明](docs/ARCHITECTURE.md) — 分层、数据模型、认证模型、关键决策
- [API 参考](docs/API.md) — 完整的端点点表（npm 协议 + 管理 API）
- [部署指南](docs/DEPLOYMENT.md) — 反向代理、HTTPS、备份、升级、运维

> 部署配置（域名、服务器地址、密钥路径）属于实例信息，不放在仓库里。
> 本仓库只提供通用指引；具体实例由外部参数化工具渲染 systemd 单元与 nginx 站点。

---

## 安全说明

- 密码使用 **Argon2id** 哈希存储，永不落明文。
- API 令牌只存 **SHA-256 哈希**，明文仅在创建时展示一次。
- 网页会话使用 **HttpOnly + SameSite=Lax** Cookie 承载 JWT，不写 `localStorage`，不放 URL。
- tarball 路径做白名单校验 + 规范化检查，杜绝目录穿越。
- 错误响应统一为 `{"error": "..."}`，不泄露堆栈、SQL 或内部路径。
- 默认关闭自助注册。生产环境请务必替换 `JWT_SECRET`、设置强 `ADMIN_PASSWORD`、置于 HTTPS 之后。

漏洞报告请走 [SECURITY.md](SECURITY.md)，不要开公开 issue。

---

## 路线图

- [ ] PostgreSQL 后端（sqlx 已有 feature，只差适配层）
- [ ] 包名与 scope 级权限（ACL）
- [ ] Webhook / 发布通知
- [ ] S3 / 对象存储后端
- [ ] 审计日志查询界面

欢迎提 PR，也欢迎先开 issue 讨论方向。

---

## 贡献

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org/)。

---

## English summary

A lightweight, self-hosted private npm registry written in Rust.

- **Protocol-compatible** with the npm CLI: publish, install, dist-tag, unpublish, search, whoami.
- **Authentication out of the box**: browser-based `npm login`, legacy CouchDB login, and revocable `npmp_` API tokens (stored as SHA-256 hashes).
- **Private by default**: nothing is readable or writable without credentials; first publisher owns the package.
- **Single binary**: the dashboard is embedded with `include_str!`; SQLite means no external services.
- **Storage**: tarballs on disk with `shasum` / `integrity` computed on write.

```bash
cp .env.example .env   # set JWT_SECRET and ADMIN_PASSWORD
cargo run --release    # -> http://localhost:4873
```

MIT licensed.
