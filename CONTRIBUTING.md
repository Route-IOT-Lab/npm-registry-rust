# 贡献指南

感谢愿意花时间。本文只写「怎么改」，设计意图见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 开发环境

- Rust 1.80+（`rustup` 安装即可）
- Node.js 20+（仅在跑端到端冒烟测试时需要）
- 不需要数据库：使用 SQLite 文件

```bash
git clone https://github.com/your-org/npm-registry-rust.git
cd npm-registry-rust
cp .env.example .env
# 至少设置 JWT_SECRET（openssl rand -hex 32）与 ADMIN_PASSWORD
cargo run
```

打开 <http://localhost:4873>。

---

## 提交前必须通过

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

CI 会依次跑这三步，外加一个端到端冒烟测试（真实 npm 客户端 `publish` + `install`）。四项全绿才会合并。

### 端到端自测

改动发布 / 下载 / 认证路径时，请至少手工验证一次：

```bash
cargo build --release
./target/release/npm-registry-rust &

TOKEN=$(curl -s -X PUT http://127.0.0.1:4873/-/user/org.couchdb.user:admin \
  -H 'Content-Type: application/json' \
  -d '{"name":"admin","password":"<你的 ADMIN_PASSWORD>"}' | jq -r .token)

mkdir -p /tmp/p && cd /tmp/p
echo '{"name":"t","version":"1.0.0","license":"MIT"}' > package.json
echo 'registry=http://127.0.0.1:4873/' > .npmrc
echo "//127.0.0.1:4873/:_authToken=$TOKEN" >> .npmrc
npm publish && npm dist-tag add t@1.0.0 beta && npm dist-tag ls t
```

---

## 代码规范

### 分层不许越界

| 层 | 可以 | 不可以 |
| --- | --- | --- |
| `routes/` | 解析 HTTP、调 `service`、组装响应 | 写 SQL、写业务分支 |
| `service/` | 业务规则、事务边界 | 引用 `Request` / `Response` / 状态码 |
| `models.rs` | 行模型与对外投影 | 业务逻辑 |

新增端点时问自己：这条规则如果从管理界面触发，是否也该生效？如果是，它属于 `service`。

### 强制要求

- **配置只从 `config.rs` 读**，不要在别处 `std::env::var`
- **错误统一返回 `AppError`**，不要 `unwrap()` 在请求路径上；内部错误对外一律降级为 `internal server error`
- **SQL 一律参数绑定**，不拼字符串
- **SQL 变更写进 `migrations/`**，不要改已有迁移文件（已部署的实例不会重跑）
- **前端用户可控内容必须 `esc()`**
- **公开项要有文档注释**，说明「为什么」而不是「做了什么」

### 前端

`static/` 下无构建步骤。改完直接重启进程即可（`include_str!` 在编译期读取）。
新增交互请挂在 `data-action` 上走事件委托，不要在 `innerHTML` 里内联 `onclick`。

---

## 提交信息

遵循 [Conventional Commits](https://www.conventionalcommits.org/)：

```
feat: 支持 PostgreSQL 后端
fix: 修复 /tmp 为软链时存储路径误判
docs: 补充反向代理配置示例
refactor: 抽出 dist-tag 校验
test: 覆盖 scoped 包路径解析
chore: 升级 sqlx
```

正文里请说明**为什么**改，以及**验证方式**（跑了什么命令、看到什么结果）。改动涉及行为变化时，请同步更新 `docs/`。

---

## Pull Request

1. 一个 PR 解决一件事。混在一起的改动会被要求拆分。
2. 描述里写清楚：动机、方案、验证方式、影响面。
3. 新增行为请补测试。修 bug 请补能复现该 bug 的回归测试。
4. 破坏性变更请先在 issue 里讨论。

---

## 报 Bug

请附上：

- 版本（`/api/health` 返回的 `version`）与部署方式
- 复现命令与完整输出（含 npm 的 `--loglevel verbose`）
- 相关应用日志（`LOG_FORMAT=json` 时请带上请求 ID）
- 期望行为 vs 实际行为

安全相关问题请走 [SECURITY.md](SECURITY.md)，不要开公开 issue。
