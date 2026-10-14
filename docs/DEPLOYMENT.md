# 部署指南

面向生产环境：反向代理、HTTPS、备份、升级、排障。

---

## 1. 最小生产部署

三件事决定了一个可用的私有仓库：

1. **`JWT_SECRET`** — 换成随机值，且不要提交进版本库
2. **`ADMIN_PASSWORD`** — 强密码，仅用于首次启动创建管理员
3. **`REGISTRY_BASE_URL`** — npm 客户端实际访问的地址（含协议与端口，不带尾斜杠）

```bash
openssl rand -hex 32           # JWT_SECRET
```

`REGISTRY_BASE_URL` 写错是最常见的坑：packument 里的 `dist.tarball` 由它生成，写错会导致「登录正常、`npm publish` 正常，但 `npm install` 404」。

---

## 2. Docker

### 构建

```bash
docker build -t npm-registry-rust:0.1.0 .
```

多阶段构建，最终镜像基于 `debian:bookworm-slim`，只包含二进制与 CA 证书。

### 运行

```bash
docker run -d \
  --name npm-registry \
  --restart unless-stopped \
  -p 127.0.0.1:4873:4873 \
  -e HOST=0.0.0.0 \
  -e PORT=4873 \
  -e REGISTRY_BASE_URL=https://npm.example.com \
  -e JWT_SECRET="$(openssl rand -hex 32)" \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD='<强密码>' \
  -e ALLOW_REGISTRATION=false \
  -e LOG_FORMAT=json \
  -v /srv/npm-registry/storage:/app/storage \
  -v /srv/npm-registry/data:/app/data \
  npm-registry-rust:0.1.0
```

容器内以非 root 用户 `registry` 运行，`/app/storage` 与 `/app/data` 需要可写。

### compose

```bash
cp .env.example .env    # 修改后再启动
docker compose up -d
docker compose logs -f
```

`docker-compose.yml` 里默认只把端口绑到 `127.0.0.1`，由反向代理对外暴露。

---

## 3. 反向代理

### Caddy（自动 HTTPS，最省事）

```caddyfile
npm.example.com {
    reverse_proxy 127.0.0.1:4873
    request_body {
        max_size 64MB          # 与 MAX_PUBLISH_BODY_MIB 对齐
    }
    encode gzip
}
```

### Nginx

```nginx
server {
    listen 443 ssl http2;
    server_name npm.example.com;

    ssl_certificate     /etc/letsencrypt/live/npm.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/npm.example.com/privkey.pem;

    client_max_body_size 64m;          # 必须 >= MAX_PUBLISH_BODY_MIB

    location / {
        proxy_pass         http://127.0.0.1:4873;
        proxy_http_version 1.1;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Real-IP         $remote_addr;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;

        # 大包发布时不要提前掐断
        proxy_read_timeout    300s;
        proxy_send_timeout    300s;
        proxy_request_buffering off;
    }
}
```

两个关键点：

- `client_max_body_size` / `max_size` 必须 ≥ `MAX_PUBLISH_BODY_MIB`，否则会在应用层之前被拒。
- 必须透传 `X-Forwarded-Proto`，否则若将来支持 http→https 跳转会判断错误。

---

## 4. 数据与备份

两个目录构成全部状态：

| 路径 | 内容 | 丢了会怎样 |
| --- | --- | --- |
| `./data/registry.db`（含 `-wal` / `-shm`） | 用户、令牌、包与版本元数据 | 全部元数据丢失 |
| `./storage/` | tarball 原文 | 版本无法下载 |

两者必须一起备份，且要保证一致性。推荐停机或使用 SQLite 在线备份：

```bash
# 方式一：应用在线时（推荐）
sqlite3 data/registry.db ".backup '/backup/registry-$(date +%F).db'"
tar czf /backup/storage-$(date +%F).tar.gz storage/

# 方式二：短暂停机
docker compose stop
tar czf /backup/npm-registry-$(date +%F).tar.gz data storage
docker compose start
```

> WAL 模式下**不要**直接 `cp registry.db`——那样会丢掉未 checkpoint 的已提交数据，务必用 `.backup`。

### 恢复

```bash
docker compose stop
rm -rf data storage
tar xzf /backup/npm-registry-2026-10-14.tar.gz
docker compose start
```

### 上游代理与磁盘

上游代理**不落库、不落盘**：packument 原样透传，tarball 直连上游 CDN。
所以开启公共包代理不会让 `storage/` 或数据库长大，这对磁盘紧张的机器是好事。

代价是本地没有元数据缓存——见
[架构说明 · 上游代理](ARCHITECTURE.md#8-上游代理)。

### 回填孤儿 tarball

若某次发布在「写盘成功、入库失败」之间被打断，`storage/` 会比数据库多出文件。不影响正确性，可直接删除：

```bash
# 列出数据库不认识的文件（示意，按实际包名核实后再删）
find storage -name '*.tgz' -mtime +1
```

---

## 5. 升级

```bash
git pull
docker compose build
docker compose up -d
```

启动时会自动执行 `migrations/` 下尚未应用的迁移。迁移是只增不改的，升级不需要手工介入。

**回滚前请确认迁移是否可逆**——当前版本的所有迁移都是纯 `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX`，回滚旧版本二进制通常安全。

---

## 6. 客户端配置分发

私有仓库的推广成本主要在「让每个人配好 `.npmrc`」。两种做法：

**项目级**（推荐，随代码库走）：

```ini
# .npmrc，提交进仓库
registry=https://npm.example.com/
//npm.example.com/:_authToken=${NPM_TOKEN}
```

令牌从环境变量读，不落盘。

**只覆盖 scope**（公共包仍走 npmjs.org）：

```ini
@your-company:registry=https://npm.example.com/
//npm.example.com/:_authToken=${NPM_TOKEN}
```

**CI**（GitHub Actions 示例）：

```yaml
- uses: actions/setup-node@v4
  with:
    node-version: 22
    registry-url: https://npm.example.com
- run: npm ci
  env:
    NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
- run: npm publish
  env:
    NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
```

---

## 7. 运维

### 健康检查

```bash
curl -fsS http://127.0.0.1:4873/api/health
```

Docker 镜像已内置健康检查，无需额外配置。

### 日志

`LOG_FORMAT=json` 时每条日志是一行 JSON，并带请求 ID：

```json
{"timestamp":"2026-10-14T07:12:01Z","level":"INFO","fields":{"message":"finished processing request","latency":0,"status":200},"request_id":"…"}
```

用请求 ID 串起一次请求的全部日志：

```bash
docker compose logs | grep '<request-id>'
```

### 优雅停机

收到 `SIGTERM` / `Ctrl-C` 后停止接受新连接，等待在途请求结束再退出。为保证 `docker stop` 有足够时间，可调整：

```yaml
stop_grace_period: 30s
```

### 优雅重启（不中断 npm install）

先起新实例、健康检查通过后切流量，再停旧实例。由于 SQLite 单写者限制，**不要**同时运行两个写实例。

---

## 8. 排障

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `npm install` 404，但 `npm publish` 正常 | `REGISTRY_BASE_URL` 与实际访问地址不一致，packument 里的 tarball 指向了不可达地址 | 改对 `REGISTRY_BASE_URL` 后重启（无需刷库） |
| `E401` 频繁出现 | 令牌过期或被吊销 | 重新签发；检查令牌列表的「过期」列 |
| `npm login` 卡住不返回 | 浏览器未完成授权，或 `doneUrl` 不可达 | 10 分钟后重试；确认 4873 端口对浏览器可达 |
| `npm publish` 413 | 反向代理或 `MAX_PUBLISH_BODY_MIB` 限制 | 两者都放开 |
| 启动即退出并打印配置错误 | 某项环境变量缺失或非法 | 按提示修正；`JWT_SECRET` 至少要 32 字节 |
| 启动告警「no users exist」 | 首次启动未设置 `ADMIN_USERNAME` / `ADMIN_PASSWORD` | 设置后重启即可创建管理员 |
| 下载 500「tarball is missing from storage」 | `storage/` 与数据库不同步 | 从备份恢复 `storage/`，或删除该版本重新发布 |

### 排查手段

```bash
# 看应用日志
docker compose logs -f

# 直接查元数据
sqlite3 data/registry.db "select name, owner_id from packages;"
sqlite3 data/registry.db "select p.name, v.version, v.size from versions v join packages p on p.id=v.package_id;"

# 校验存储与数据库是否一致
find storage -name '*.tgz' | wc -l
sqlite3 data/registry.db "select count(*) from versions;"
```

---

## 9. 上线检查清单

- [ ] `JWT_SECRET` 已替换为随机值，且未进版本库
- [ ] `ADMIN_PASSWORD` 已设为强密码（或已确认管理员存在）
- [ ] `REGISTRY_BASE_URL` 与客户端实际访问地址一致
- [ ] 已置于 HTTPS 之后，会话 Cookie 自动带 `Secure`
- [ ] `ALLOW_REGISTRATION=false`
- [ ] `CORS_ALLOWED_ORIGINS` 为空或显式列出（绝不为 `*`）
- [ ] 反向代理的 body 上限 ≥ `MAX_PUBLISH_BODY_MIB`
- [ ] `data/` 与 `storage/` 已纳入备份，且演练过恢复
- [ ] `LOG_FORMAT=json`，日志已接入采集
- [ ] 健康检查已接入监控
- [ ] 已用真实 npm 客户端走过一遍 `login` → `publish` → `install`
