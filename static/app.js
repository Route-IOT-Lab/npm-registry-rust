/* npm-registry-rust dashboard — dependency-free single page app. */
(() => {
  "use strict";

  const appEl = document.getElementById("app");
  const toastEl = document.getElementById("toast");

  const state = {
    me: null,
    config: { registry_base_url: "", allow_registration: false },
    tab: "packages",
    search: "",
    packages: [],
    packagesTotal: 0,
    detail: null,
    tokens: [],
    users: [],
    cliSession: new URLSearchParams(location.search).get("cli"),
    error: null,
    revealedToken: null,
    busy: false,
  };

  // ---------------------------------------------------------------- utilities

  function esc(value) {
    return String(value === null || value === undefined ? "" : value).replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
    );
  }

  function toast(message, kind = "") {
    toastEl.textContent = message;
    toastEl.className = "toast " + kind;
    toastEl.hidden = false;
    clearTimeout(toast._t);
    toast._t = setTimeout(() => (toastEl.hidden = true), 3200);
  }

  async function api(path, options = {}) {
    const res = await fetch(path, {
      credentials: "include",
      headers: { "Content-Type": "application/json", ...(options.headers || {}) },
      ...options,
    });
    let payload = null;
    const text = await res.text();
    if (text) {
      try {
        payload = JSON.parse(text);
      } catch {
        payload = { error: text };
      }
    }
    if (!res.ok) {
      const message = (payload && (payload.error || payload.reason)) || `HTTP ${res.status}`;
      const err = new Error(message);
      err.status = res.status;
      throw err;
    }
    return payload;
  }

  function fmtDate(value) {
    if (!value) return "—";
    const d = new Date(value);
    if (Number.isNaN(d.getTime())) return esc(value);
    return d.toLocaleString("zh-CN", { hour12: false });
  }

  // ------------------------------------------------------------------- boot

  async function boot() {
    try {
      state.config = await api("/api/config");
    } catch {
      /* non fatal */
    }
    try {
      state.me = await api("/api/me");
    } catch {
      state.me = null;
    }
    render();
  }

  // ------------------------------------------------------------------ render

  function render() {
    if (!state.me) {
      appEl.innerHTML = viewLogin();
      return;
    }
    appEl.innerHTML = viewShell();
    if (state.cliSession) renderCliBanner();
  }

  function viewLogin() {
    return `
      <div class="login-wrap">
        <form class="login-card" data-form="login">
          <div class="logo">n</div>
          <h1>私有 npm 仓库</h1>
          <p class="sub">${esc(state.config.registry_base_url || "npm-registry-rust")}</p>
          ${state.error ? `<div class="form-error">${esc(state.error)}</div>` : ""}
          <div class="field">
            <label for="u">用户名</label>
            <input id="u" name="username" autocomplete="username" required />
          </div>
          <div class="field">
            <label for="p">密码</label>
            <input id="p" name="password" type="password" autocomplete="current-password" required />
          </div>
          ${
            state.config.allow_registration
              ? `<div class="between"><span class="muted" style="font-size:12px">开放注册</span>
                   <button type="button" class="btn ghost sm" data-action="switch-register">注册新账号</button></div>`
              : ""
          }
          <button class="btn primary" style="width:100%;margin-top:8px" ${
            state.busy ? "disabled" : ""
          }>登 录</button>
        </form>
      </div>`;
  }

  function viewRegister() {
    return `
      <div class="login-wrap">
        <form class="login-card" data-form="register">
          <div class="logo">n</div>
          <h1>注册账号</h1>
          <p class="sub">用户名 ≥2 位，密码 ≥8 位</p>
          ${state.error ? `<div class="form-error">${esc(state.error)}</div>` : ""}
          <div class="field"><label>用户名</label><input name="username" required /></div>
          <div class="field"><label>邮箱（可选）</label><input name="email" type="email" /></div>
          <div class="field"><label>密码</label><input name="password" type="password" required /></div>
          <div class="between">
            <button type="button" class="btn ghost sm" data-action="switch-login">返回登录</button>
            <button class="btn primary" ${state.busy ? "disabled" : ""}>创建并登录</button>
          </div>
        </form>
      </div>`;
  }

  function viewShell() {
    const tabs = [
      ["packages", "包"],
      ["tokens", "访问令牌"],
      ["guide", "接入指引"],
    ];
    if (state.me.is_admin) tabs.push(["users", "用户"]);

    return `
      <header class="topbar">
        <div class="brand"><span class="dot">n</span> npm registry <small>私有仓库</small></div>
        <nav class="tabs">
          ${tabs
            .map(
              ([id, label]) =>
                `<button class="tab ${state.tab === id ? "active" : ""}" data-action="tab" data-tab="${id}">${label}</button>`
            )
            .join("")}
        </nav>
        <div class="spacer"></div>
        <span class="who"><b>${esc(state.me.username)}</b>${state.me.is_admin ? " · 管理员" : ""}</span>
        <button class="btn ghost sm" data-action="logout">退出</button>
      </header>
      <main class="main">${viewTab()}</main>
      ${state.revealedToken ? viewRevealModal() : ""}
    `;
  }

  function viewTab() {
    switch (state.tab) {
      case "tokens":
        return viewTokens();
      case "users":
        return viewUsers();
      case "guide":
        return viewGuide();
      default:
        return viewPackages();
    }
  }

  // --------------------------------------------------------------- packages

  function viewPackages() {
    const rows = state.packages
      .map(
        (p) => `
        <tr>
          <td class="mono">${esc(p.name)}</td>
          <td>${p.latest ? `<span class="pill accent">${esc(p.latest)}</span>` : "—"}</td>
          <td class="muted">${esc(p.description || "—")}</td>
          <td class="mono muted">${p.versions}</td>
          <td class="mono muted">${p.downloads}</td>
          <td class="muted">${esc(p.owner || "—")}</td>
          <td class="muted">${fmtDate(p.updated_at)}</td>
          <td class="actions">
            <button class="btn sm" data-action="detail" data-name="${esc(p.name)}">详情</button>
          </td>
        </tr>`
      )
      .join("");

    return `
      <div class="stack">
        ${
          state.detail
            ? `<div class="panel" id="detail-panel">
                 <div class="panel-head">
                   <h2 class="mono">${esc(state.detail.name)}</h2>
                   <span class="spacer"></span>
                   <button class="btn sm ghost" data-action="close-detail">关闭</button>
                 </div>
                 <div class="panel-body">
                   <div class="stack">
                     <div class="between">
                       <div>
                         <div class="muted" style="font-size:12px">最新版本</div>
                         <div class="mono">${esc(state.detail.latest || "—")}</div>
                       </div>
                       <div>
                         <div class="muted" style="font-size:12px">归属</div>
                         <div class="mono">${esc(state.detail.owner || "—")}</div>
                       </div>
                       <div>
                         <div class="muted" style="font-size:12px">下载量</div>
                         <div class="mono">${state.detail.downloads}</div>
                       </div>
                       <button class="btn sm danger" data-action="delete-package" data-name="${esc(
                               state.detail.name
                             )}">删除整包</button>
                     </div>
                     <table>
                       <thead><tr><th>版本</th><th>大小</th><th>下载</th><th>发布时间</th><th></th></tr></thead>
                       <tbody>
                         ${state.detail.versions
                           .map(
                             (v) => `<tr>
                               <td class="mono">${esc(v.version)}</td>
                               <td class="mono muted">${humanSize(v.size)}</td>
                               <td class="mono muted">${v.downloads}</td>
                               <td class="muted">${fmtDate(v.created_at)}</td>
                               <td class="actions">
                                 <button class="btn sm danger" data-action="delete-version"
                                          data-name="${esc(state.detail.name)}" data-version="${esc(
                                         v.version
                                       )}">删除</button>
                               </td>
                             </tr>`
                           )
                           .join("")}
                       </tbody>
                     </table>
                     <div>
                       <div class="muted" style="font-size:12px;margin-bottom:6px">安装命令</div>
                       <div class="code">npm i ${esc(
                         state.detail.name
                       )} --registry=${esc(state.config.registry_base_url)}</div>
                     </div>
                   </div>
                 </div>
               </div>`
            : ""
        }
        <div class="panel">
          <div class="panel-head">
            <h2>包</h2>
            <span class="pill">${state.packagesTotal}</span>
            <span class="spacer"></span>
            <input class="search" data-input="search" placeholder="搜索包名 / 描述…" value="${esc(
              state.search
            )}" />
            <button class="btn sm" data-action="refresh-packages">刷新</button>
          </div>
          <div class="panel-body flush">
            ${
              state.packages.length
                ? `<table>
                     <thead><tr><th>包名</th><th>最新</th><th>描述</th><th>版本</th><th>下载</th><th>归属</th><th>更新时间</th><th></th></tr></thead>
                     <tbody>${rows}</tbody>
                   </table>`
                : `<div class="empty">还没有包。用 npm publish 推一个上来试试。</div>`
            }
          </div>
        </div>
      </div>`;
  }

  // ----------------------------------------------------------------- tokens

  function viewTokens() {
    return `
      <div class="stack">
        <div class="panel">
          <div class="panel-head"><h2>新建访问令牌</h2></div>
          <div class="panel-body">
            <form class="row" data-form="create-token">
              <div>
                <label>名称</label>
                <input name="name" placeholder="例如：ci-runner" required />
              </div>
              <div>
                <label>有效期</label>
                <select name="ttl_days">
                  <option value="">永久</option>
                  <option value="30">30 天</option>
                  <option value="90">90 天</option>
                  <option value="365">1 年</option>
                </select>
              </div>
              <div class="fixed"><button class="btn primary">生成</button></div>
            </form>
            <p class="muted" style="font-size:12.5px;margin:12px 0 0">
              令牌明文只显示一次。把它写进 <span class="mono">~/.npmrc</span>：
              <span class="mono">//your-registry/:_authToken=&lt;token&gt;</span>
            </p>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>我的令牌</h2><span class="pill">${state.tokens.length}</span></div>
          <div class="panel-body flush">
            ${
              state.tokens.length
                ? `<table>
                     <thead><tr><th>名称</th><th>令牌</th><th>来源</th><th>创建</th><th>最后使用</th><th>过期</th><th></th></tr></thead>
                     <tbody>${state.tokens
                       .map(
                         (t) => `<tr>
                           <td>${esc(t.name)}</td>
                           <td class="mono">${esc(t.token)}</td>
                           <td><span class="pill">${esc(t.source)}</span></td>
                           <td class="muted">${fmtDate(t.created_at)}</td>
                           <td class="muted">${fmtDate(t.last_used_at)}</td>
                           <td class="muted">${t.expires_at ? fmtDate(t.expires_at) : "永久"}</td>
                           <td class="actions"><button class="btn sm danger" data-action="revoke-token" data-id="${
                             t.id
                           }">吊销</button></td>
                         </tr>`
                       )
                       .join("")}</tbody>
                   </table>`
                : `<div class="empty">还没有令牌。</div>`
            }
          </div>
        </div>
      </div>`;
  }

  // ------------------------------------------------------------------ users

  function viewUsers() {
    return `
      <div class="stack">
        <div class="panel">
          <div class="panel-head"><h2>新建用户</h2></div>
          <div class="panel-body">
            <form class="row" data-form="create-user">
              <div><label>用户名</label><input name="username" required /></div>
              <div><label>邮箱</label><input name="email" type="email" /></div>
              <div><label>密码</label><input name="password" type="password" required /></div>
              <div class="fixed"><label>管理员</label>
                <select name="is_admin"><option value="false">否</option><option value="true">是</option></select>
              </div>
              <div class="fixed"><button class="btn primary">创建</button></div>
            </form>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>用户列表</h2><span class="pill">${state.users.length}</span></div>
          <div class="panel-body flush">
            <table>
              <thead><tr><th>用户名</th><th>邮箱</th><th>角色</th><th>创建时间</th><th></th></tr></thead>
              <tbody>${state.users
                .map(
                  (u) => `<tr>
                    <td class="mono">${esc(u.username)}</td>
                    <td class="muted">${esc(u.email || "—")}</td>
                    <td>${
                      u.is_admin ? `<span class="pill warn">管理员</span>` : `<span class="pill">成员</span>`
                    }</td>
                    <td class="muted">${fmtDate(u.created_at)}</td>
                    <td class="actions">
                      <button class="btn sm" data-action="reset-password" data-id="${u.id}" data-name="${esc(
                    u.username
                  )}">改密码</button>
                      ${
                        u.id === state.me.id
                          ? ""
                          : `<button class="btn sm danger" data-action="delete-user" data-id="${u.id}" data-name="${esc(
                              u.username
                            )}">删除</button>`
                      }
                    </td>
                  </tr>`
                )
                .join("")}</tbody>
            </table>
          </div>
        </div>
      </div>`;
  }

  // ------------------------------------------------------------------ guide

  function viewGuide() {
    const base = state.config.registry_base_url || "http://localhost:4873";
    const host = base.replace(/^https?:\/\//, "");
    return `
      <div class="stack">
        <div class="panel">
          <div class="panel-head"><h2>1 · 配置 registry</h2></div>
          <div class="panel-body">
            <div class="code">npm config set registry ${esc(base)}</div>
            <p class="muted" style="font-size:12.5px;margin:12px 0 0">
              只想给某个 scope 用私有仓库，可以只覆盖该 scope：
              <span class="mono">@your-scope:registry=${esc(base)}</span>
            </p>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>2 · 登录</h2></div>
          <div class="panel-body">
            <div class="code">npm login --registry=${esc(base)}</div>
            <p class="muted" style="font-size:12.5px;margin:12px 0 0">
              浏览器登录流程不可用时，可用传统方式：
              <span class="mono">npm login --auth-type=legacy --registry=${esc(base)}</span>
            </p>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>3 · 发布与安装</h2></div>
          <div class="panel-body">
            <div class="code">npm publish                        # 在当前包目录执行
npm i &lt;package&gt; --registry=${esc(base)}</div>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>4 · 公共包自动回退上游</h2></div>
          <div class="panel-body">
            <p class="muted" style="margin:0">
              本仓库找不到的包，会按配置顺序向各上游依次查询：先淘宝源，再官方源。
              某一层明确返回「无此包」才继续往下找，全都找不到才回 404；全部上游都故障则回 502。
              上游响应是<b>原样透传</b>的，因此公共包的 tarball 直接来自上游 CDN，
              不会经过本机。所以<b>不需要</b>为公共包单独切换 registry。
            </p>
          </div>
        </div>
        <div class="panel">
          <div class="panel-head"><h2>5 · 直接用令牌（CI 场景）</h2></div>
          <div class="panel-body">
            <div class="code"># ~/.npmrc
//${esc(host)}/:_authToken=&lt;你的令牌&gt;</div>
            <p class="muted" style="font-size:12.5px;margin:12px 0 0">
              在「访问令牌」页生成，避免在 CI 里跑交互式登录。
            </p>
          </div>
        </div>
      </div>`;
  }

  function viewRevealModal() {
    return `
      <div class="overlay" data-action="close-reveal">
        <div class="modal" data-stop="1">
          <h3>令牌已生成</h3>
          <p class="muted" style="font-size:13px">复制并妥善保存 — 关闭后无法再次查看。</p>
          <div class="code">${esc(state.revealedToken)}</div>
          <div class="modal-actions">
            <button class="btn" data-action="copy-token">复制</button>
            <button class="btn primary" data-action="close-reveal">我已保存</button>
          </div>
        </div>
      </div>`;
  }

  function renderCliBanner() {
    if (!state.me) return;
    const card = document.createElement("div");
    card.className = "panel";
    card.style.marginBottom = "18px";
    card.id = "cli-banner";
    card.innerHTML = `
      <div class="panel-head"><h2>命令行登录请求</h2></div>
      <div class="panel-body">
        <p class="muted" style="margin:0 0 14px">
          终端里的 <span class="mono">npm login</span> 正等待授权。确认后将为其签发一个访问令牌。
        </p>
        <div class="inline">
          <button class="btn primary" data-action="cli-authorize">授权</button>
          <button class="btn ghost" data-action="cli-deny">拒绝</button>
        </div>
      </div>`;
    appEl.querySelector(".main").prepend(card);
  }

  function humanSize(bytes) {
    if (!bytes) return "0 B";
    const units = ["B", "KB", "MB", "GB"];
    let i = 0;
    let n = bytes;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i++;
    }
    return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
  }

  // ------------------------------------------------------------------ loaders

  async function loadPackages() {
    const params = new URLSearchParams();
    if (state.search) params.set("q", state.search);
    const data = await api(`/api/packages?${params.toString()}`);
    state.packages = data.packages;
    state.packagesTotal = data.total;
  }

  async function loadDetail(name) {
    state.detail = await api(`/api/packages/${encodeURIComponent(name)}`);
  }

  async function loadTokens() {
    state.tokens = (await api("/api/me/tokens")).tokens;
  }

  async function loadUsers() {
    state.users = (await api("/api/users")).users;
  }

  async function refreshTab() {
    try {
      if (state.tab === "packages") await loadPackages();
      if (state.tab === "tokens") await loadTokens();
      if (state.tab === "users") await loadUsers();
    } catch (e) {
      toast(e.message, "err");
    }
    render();
  }

  // ------------------------------------------------------------------ events

  appEl.addEventListener("submit", async (event) => {
    const form = event.target.closest("form");
    if (!form) return;
    event.preventDefault();
    const kind = form.dataset.form;
    const data = Object.fromEntries(new FormData(form));

    try {
      state.busy = true;
      state.error = null;

      if (kind === "login") {
        await api("/api/auth/login", { method: "POST", body: JSON.stringify(data) });
        state.me = await api("/api/me");
        await refreshTab();
      } else if (kind === "register") {
        await api("/api/auth/register", { method: "POST", body: JSON.stringify(data) });
        state.me = await api("/api/me");
        await refreshTab();
      } else if (kind === "create-token") {
        const payload = { name: data.name, ttl_days: data.ttl_days ? Number(data.ttl_days) : null };
        const created = await api("/api/me/tokens", { method: "POST", body: JSON.stringify(payload) });
        state.revealedToken = created.token;
        await loadTokens();
        toast("令牌已生成", "ok");
        render();
      } else if (kind === "create-user") {
        await api("/api/users", {
          method: "POST",
          body: JSON.stringify({ ...data, is_admin: data.is_admin === "true" }),
        });
        form.reset();
        await loadUsers();
        toast("用户已创建", "ok");
        render();
      }
    } catch (e) {
      state.error = e.message;
      toast(e.message, "err");
      render();
    } finally {
      state.busy = false;
    }
  });

  let searchTimer = null;
  appEl.addEventListener("input", (event) => {
    const field = event.target.dataset.input;


    if (field !== "search") return;
    state.search = event.target.value;
    clearTimeout(searchTimer);
    searchTimer = setTimeout(async () => {
      await loadPackages();
      render();
      const input = appEl.querySelector('[data-input="search"]');
      if (input) {
        input.focus();
        input.setSelectionRange(input.value.length, input.value.length);
      }
    }, 250);
  });

  appEl.addEventListener("click", async (event) => {
    const target = event.target.closest("[data-action]");
    if (!target) return;
    const action = target.dataset.action;

    if (action === "close-reveal" && event.target.closest("[data-stop]") && target === event.currentTarget) return;

    try {
      switch (action) {
        case "tab": {
          state.tab = target.dataset.tab;
          state.detail = null;
          state.error = null;
          await refreshTab();
          break;
        }
        case "logout": {
          await api("/api/auth/logout", { method: "POST" });
          state.me = null;
          state.packages = [];
          state.detail = null;
          render();
          break;
        }
        case "switch-register": {
          appEl.innerHTML = viewRegister();
          break;
        }
        case "switch-login": {
          state.error = null;
          render();
          break;
        }
        case "refresh-packages": {
          await loadPackages();
          render();
          break;
        }
        case "detail": {
          await loadDetail(target.dataset.name);
          render();
          appEl.querySelector("#detail-panel")?.scrollIntoView({ behavior: "smooth" });
          break;
        }
        case "close-detail": {
          state.detail = null;
          render();
          break;
        }
        case "delete-version": {
          if (!confirm(`删除 ${target.dataset.name}@${target.dataset.version}？此操作不可撤销。`)) return;
          await api(
            `/api/packages/${encodeURIComponent(target.dataset.name)}?version=${encodeURIComponent(
              target.dataset.version
            )}`,
            { method: "DELETE" }
          );
          await loadDetail(target.dataset.name);
          await loadPackages();
          toast("版本已删除", "ok");
          render();
          break;
        }
        case "delete-package": {
          if (!confirm(`删除整个包 ${target.dataset.name}？所有版本都会被移除。`)) return;
          await api(`/api/packages/${encodeURIComponent(target.dataset.name)}`, { method: "DELETE" });
          state.detail = null;
          await loadPackages();
          toast("包已删除", "ok");
          render();
          break;
        }
        case "revoke-token": {
          if (!confirm("吊销该令牌？使用它的客户端会立即失效。")) return;
          await api(`/api/me/tokens/${target.dataset.id}`, { method: "DELETE" });
          await loadTokens();
          toast("令牌已吊销", "ok");
          render();
          break;
        }
        case "delete-user": {
          if (!confirm(`删除用户 ${target.dataset.name}？其令牌会一并失效。`)) return;
          await api(`/api/users/${target.dataset.id}`, { method: "DELETE" });
          await loadUsers();
          toast("用户已删除", "ok");
          render();
          break;
        }
        case "reset-password": {
          const pw = prompt(`为 ${target.dataset.name} 设置新密码（≥8 位）`);
          if (!pw) return;
          await api(`/api/users/${target.dataset.id}/password`, {
            method: "POST",
            body: JSON.stringify({ password: pw }),
          });
          toast("密码已更新", "ok");
          break;
        }
        case "copy-token": {
          await navigator.clipboard.writeText(state.revealedToken);
          toast("已复制到剪贴板", "ok");
          break;
        }
        case "close-reveal": {
          if (event.target.closest("[data-stop]") && target.classList.contains("overlay")) return;
          state.revealedToken = null;
          render();
          break;
        }
        case "cli-authorize": {
          await api("/api/cli/authorize", {
            method: "POST",
            body: JSON.stringify({ session_id: state.cliSession }),
          });
          state.cliSession = null;
          history.replaceState(null, "", location.pathname);
          toast("已授权，终端可以继续了", "ok");
          render();
          break;
        }
        case "cli-deny": {
          state.cliSession = null;
          history.replaceState(null, "", location.pathname);
          render();
          break;
        }
        default:
          break;
      }
    } catch (e) {
      toast(e.message, "err");
    }
  });

  boot();
})();
