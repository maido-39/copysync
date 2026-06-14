(function () {
  "use strict";

  var root = document.getElementById("root");
  var toastBox = document.getElementById("toasts");
  var CSRF = { "X-CopySync-CSRF": "1" };
  var state = { me: null, section: "overview", nav: {}, main: null };

  // ---- helpers ---------------------------------------------------------------
  async function api(method, path, body) {
    var opts = { method: method, credentials: "same-origin", headers: {} };
    if (body !== undefined) {
      opts.headers["Content-Type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    if (method !== "GET" && method !== "HEAD") Object.assign(opts.headers, CSRF);
    var res = await fetch(path, opts);
    var text = await res.text();
    var data = null;
    if (text) { try { data = JSON.parse(text); } catch (e) { data = text; } }
    if (!res.ok) {
      var err = new Error((data && data.message) || res.statusText);
      err.status = res.status; err.code = data && data.error;
      throw err;
    }
    return data;
  }

  function el(tag, attrs) {
    var e = document.createElement(tag);
    if (attrs) for (var k in attrs) {
      if (k === "class") e.className = attrs[k];
      else if (k === "html") e.innerHTML = attrs[k];
      else if (k === "text") e.textContent = attrs[k];
      else if (k.slice(0, 2) === "on") e.addEventListener(k.slice(2), attrs[k]);
      else if (attrs[k] != null) e.setAttribute(k, attrs[k]);
    }
    for (var i = 2; i < arguments.length; i++) {
      var c = arguments[i];
      if (c == null) continue;
      if (Array.isArray(c)) c.forEach(function (x) { if (x != null) e.appendChild(typeof x === "string" ? document.createTextNode(x) : x); });
      else e.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    }
    return e;
  }
  function clear(n) { while (n.firstChild) n.removeChild(n.firstChild); }

  function fmtBytes(n) {
    n = Number(n) || 0;
    if (n >= 1 << 30) return (n / (1 << 30)).toFixed(1) + " GiB";
    if (n >= 1 << 20) return (n / (1 << 20)).toFixed(1) + " MiB";
    if (n >= 1 << 10) return (n / (1 << 10)).toFixed(1) + " KiB";
    return n + " B";
  }
  function fmtTime(s) { return s ? new Date(s).toLocaleString() : "—"; }
  function relTime(s) {
    if (!s) return "—";
    var d = Date.now() - new Date(s).getTime();
    if (d < 60000) return "방금";
    if (d < 3600000) return Math.floor(d / 60000) + "분 전";
    if (d < 86400000) return Math.floor(d / 3600000) + "시간 전";
    return Math.floor(d / 86400000) + "일 전";
  }

  function toast(msg, kind) {
    var t = el("div", { class: "toast " + (kind || "") }, msg);
    toastBox.appendChild(t);
    setTimeout(function () { t.style.opacity = "0"; setTimeout(function () { t.remove(); }, 200); }, 3200);
  }

  function modal(opts) {
    var bg = el("div", { class: "modal-bg", onclick: function (e) { if (e.target === bg) close(); } });
    function close() { bg.remove(); }
    var confirmBtn = el("button", { class: "btn " + (opts.danger ? "danger" : ""), onclick: function () { close(); opts.onConfirm && opts.onConfirm(); } }, opts.confirmText || "확인");
    var m = el("div", { class: "modal" },
      el("h3", null, opts.title),
      opts.body ? el("p", null, opts.body) : null,
      el("div", { class: "row" },
        el("button", { class: "btn ghost", onclick: close }, "취소"),
        confirmBtn));
    bg.appendChild(m);
    document.body.appendChild(bg);
  }

  // ---- entry -----------------------------------------------------------------
  async function start() {
    try {
      var me = await api("GET", "/admin/me");
      state.me = me;
      if (me.mustChangePw) renderForcedChangePw();
      else renderApp();
    } catch (e) {
      renderLogin();
    }
  }

  // ---- auth screens ----------------------------------------------------------
  function authShell(title, sub, formNode) {
    clear(root);
    root.appendChild(el("div", { class: "auth-wrap" },
      el("div", { class: "auth" },
        el("div", { class: "logo" },
          el("img", { class: "logo-img", src: "/mascot.png", alt: "CopySync" }),
          el("h1", null, "CopySync"),
          el("p", null, sub || "self-hosted LAN clipboard sync")),
        el("div", { class: "card" }, el("h2", null, title), formNode))));
  }

  function renderLogin() {
    var user = el("input", { type: "text", value: "admin", autocomplete: "username" });
    var pass = el("input", { type: "password", autocomplete: "current-password" });
    var msg = el("p", { class: "msg" });
    var form = el("form", { onsubmit: async function (ev) {
      ev.preventDefault();
      msg.className = "msg"; msg.textContent = "";
      try { await api("POST", "/admin/login", { username: user.value, password: pass.value }); start(); }
      catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
    }},
      el("label", null, "사용자 이름"), user,
      el("label", null, "비밀번호"), pass,
      el("div", { class: "row", style: "margin-top:16px" }, el("button", { class: "btn", type: "submit" }, "로그인")),
      msg);
    authShell("로그인", null, form);
  }

  function renderForcedChangePw() {
    var cur = el("input", { type: "password", value: "", autocomplete: "current-password" });
    var nw = el("input", { type: "password", autocomplete: "new-password" });
    var nw2 = el("input", { type: "password", autocomplete: "new-password" });
    var msg = el("p", { class: "msg" });
    var form = el("form", { onsubmit: async function (ev) {
      ev.preventDefault();
      msg.className = "msg"; msg.textContent = "";
      if (nw.value !== nw2.value) { msg.className = "msg err"; msg.textContent = "비밀번호가 일치하지 않습니다."; return; }
      try { await api("POST", "/admin/password", { current: cur.value, new: nw.value }); start(); }
      catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
    }},
      el("p", { class: "help" }, "계속하려면 기본 비밀번호를 변경해야 합니다."),
      el("label", null, "현재 비밀번호"), cur,
      el("label", null, "새 비밀번호 (8자 이상)"), nw,
      el("label", null, "새 비밀번호 확인"), nw2,
      el("div", { class: "row", style: "margin-top:16px" }, el("button", { class: "btn", type: "submit" }, "비밀번호 저장")),
      msg);
    authShell("새 비밀번호 설정", "first-run security", form);
  }

  // ---- app shell -------------------------------------------------------------
  var SECTIONS = [
    { id: "overview", label: "개요", ic: "📊" },
    { id: "devices", label: "기기", ic: "💻" },
    { id: "pairing", label: "페어링", ic: "🔗" },
    { id: "settings", label: "설정", ic: "⚙️" },
    { id: "downloads", label: "다운로드", ic: "📥" },
    { id: "monitor", label: "모니터링", ic: "📡" },
    { id: "account", label: "계정", ic: "👤" },
  ];

  function renderApp() {
    clear(root);
    var nav = el("nav", { class: "nav" });
    state.nav = {};
    SECTIONS.forEach(function (s) {
      var b = el("button", { onclick: function () { go(s.id); } },
        el("span", { class: "ic" }, s.ic), el("span", null, s.label));
      state.nav[s.id] = b;
      nav.appendChild(b);
    });
    var sidebar = el("aside", { class: "sidebar" },
      el("div", { class: "brand" },
        el("img", { class: "brand-img", src: "/mascot.png", alt: "" }),
        el("div", null, el("b", null, "CopySync"), el("div", { class: "srv" }, state.me.serverName || ""))),
      nav,
      el("div", { class: "foot" },
        el("div", { class: "who" }, state.me.username || "admin"),
        el("button", { class: "btn ghost", onclick: logout }, "로그아웃")));
    state.main = el("main", { class: "main" });
    root.appendChild(el("div", { class: "layout" }, sidebar, state.main));
    go(state.section);
  }

  async function logout() { try { await api("POST", "/admin/logout"); } catch (e) {} start(); }

  function go(id) {
    state.section = id;
    if (state.monitorES) { state.monitorES.close(); state.monitorES = null; }
    for (var k in state.nav) state.nav[k].classList.toggle("active", k === id);
    clear(state.main);
    if (id === "overview") sectionOverview();
    else if (id === "devices") sectionDevices();
    else if (id === "pairing") sectionPairing();
    else if (id === "settings") sectionSettings();
    else if (id === "downloads") sectionDownloads();
    else if (id === "monitor") sectionMonitor();
    else sectionAccount();
  }

  function pageHead(title, sub, action) {
    return el("div", { class: "page-head" },
      el("div", null, el("h1", null, title), sub ? el("p", null, sub) : null),
      action || null);
  }

  // ---- overview --------------------------------------------------------------
  async function sectionOverview() {
    var m = state.main;
    m.appendChild(pageHead("개요", "서버 상태 요약"));
    var tiles = el("div", { class: "tiles" }, el("div", { class: "tile" }, el("div", { class: "k" }, "불러오는 중…")));
    m.appendChild(tiles);
    var recent = el("div", { class: "card" }, el("h2", null, "최근 기기"), el("p", { class: "muted" }, "불러오는 중…"));
    m.appendChild(recent);
    try {
      var d = await api("GET", "/admin/devices");
      var s = await api("GET", "/admin/settings").catch(function () { return {}; });
      var devs = (d && d.devices) || [];
      var online = devs.filter(function (x) { return x.online; }).length;
      clear(tiles);
      tiles.appendChild(tile("기기", String(devs.length)));
      tiles.appendChild(tile("온라인", String(online), online > 0));
      tiles.appendChild(tile("E2E 암호화", s.e2eEnabled ? "켜짐" : "꺼짐"));
      tiles.appendChild(tile("온디맨드 임계값", s.onDemandThresholdBytes ? fmtBytes(s.onDemandThresholdBytes) : "—"));
      clear(recent);
      recent.appendChild(el("div", { class: "card-head" }, el("h2", null, "최근 기기"),
        el("button", { class: "btn ghost sm", onclick: function () { go("pairing"); } }, "기기 페어링")));
      if (!devs.length) recent.appendChild(el("p", { class: "empty" }, "아직 페어링된 기기가 없습니다."));
      else {
        var sorted = devs.slice().sort(function (a, b) { return (b.online ? 1 : 0) - (a.online ? 1 : 0); }).slice(0, 5);
        sorted.forEach(function (x) {
          recent.appendChild(el("div", { class: "switch-row" },
            el("div", { class: "lbl" }, el("b", null, x.name || "(이름 없음)"), el("span", null, (x.platform || "—") + " · " + relTime(x.lastSeenAt))),
            el("span", { class: "badge " + (x.online ? "on" : "off") }, x.online ? "온라인" : "오프라인")));
        });
      }
    } catch (e) { clear(tiles); tiles.appendChild(el("p", { class: "msg err" }, e.message)); }
  }
  function tile(k, v, dot) {
    return el("div", { class: "tile" }, el("div", { class: "k" }, k), el("div", { class: "v" + (dot ? " dot" : "") }, v));
  }

  // ---- devices ---------------------------------------------------------------
  async function sectionDevices() {
    var m = state.main;
    var refresh = el("button", { class: "btn ghost sm", onclick: load }, "새로고침");
    m.appendChild(pageHead("기기", "페어링된 클라이언트", refresh));
    var card = el("div", { class: "card" }, el("p", { class: "muted" }, "불러오는 중…"));
    m.appendChild(card);

    async function load() {
      clear(card);
      try {
        var data = await api("GET", "/admin/devices");
        var devs = (data && data.devices) || [];
        if (!devs.length) { card.appendChild(el("p", { class: "empty" }, "아직 페어링된 기기가 없습니다. 페어링 탭에서 코드를 생성하세요.")); return; }
        var tbody = el("tbody");
        devs.forEach(function (dv) {
          tbody.appendChild(el("tr", null,
            el("td", null, el("strong", null, dv.name || "(이름 없음)"), el("div", { class: "muted small mono" }, dv.id)),
            el("td", null, dv.platform || "—"),
            el("td", null, el("span", { class: "badge " + (dv.online ? "on" : "off") }, dv.online ? "온라인" : "오프라인")),
            el("td", { class: "muted small" }, relTime(dv.lastSeenAt)),
            el("td", { style: "text-align:right" }, el("button", { class: "btn danger sm", onclick: function () { revoke(dv); } }, "해제"))));
        });
        card.appendChild(el("div", { class: "table-wrap" }, el("table", null,
          el("thead", null, el("tr", null,
            el("th", null, "이름"), el("th", null, "플랫폼"), el("th", null, "상태"), el("th", null, "마지막 접속"), el("th", null, ""))),
          tbody)));
      } catch (e) { card.appendChild(el("p", { class: "msg err" }, e.message)); }
    }
    function revoke(dv) {
      modal({
        title: "기기 해제", danger: true, confirmText: "해제",
        body: "“" + (dv.name || dv.id) + "” 의 페어링을 해제합니다. 다시 사용하려면 재페어링이 필요합니다.",
        onConfirm: async function () {
          try { await api("DELETE", "/admin/devices/" + encodeURIComponent(dv.id)); toast("해제됨", "ok"); load(); }
          catch (e) { toast(e.message, "err"); }
        },
      });
    }
    load();
  }

  // ---- pairing ---------------------------------------------------------------
  function sectionPairing() {
    var m = state.main;
    m.appendChild(pageHead("페어링", "일회용 코드로 새 기기 연결"));
    var card = el("div", { class: "card" });
    var out = el("div", null);
    card.appendChild(el("p", { class: "sub" }, "코드를 생성한 뒤 클라이언트에서 QR을 스캔하거나 값을 직접 입력하세요. 코드는 1회용이며 곧 만료됩니다."));
    card.appendChild(el("div", { class: "row" }, el("button", { class: "btn", onclick: gen }, "페어링 코드 생성")));
    card.appendChild(out);
    m.appendChild(card);

    async function gen() {
      clear(out);
      out.appendChild(el("p", { class: "muted", style: "margin-top:16px" }, "생성 중…"));
      try {
        var r = await api("POST", "/admin/pairing");
        clear(out);
        var exp = el("p", { class: "help" });
        function tick() {
          if (!document.body.contains(exp)) return;
          var left = Math.max(0, Math.round((new Date(r.expiresAt).getTime() - Date.now()) / 1000));
          exp.textContent = "만료: " + fmtTime(r.expiresAt) + " (" + left + "초 남음)";
          if (left > 0) setTimeout(tick, 1000);
        }
        var left = el("div", null,
          el("label", null, "일회용 코드"),
          el("div", { class: "otp mono" }, r.otp),
          exp,
          el("div", { class: "row", style: "margin-top:12px" },
            el("button", { class: "btn ghost sm", onclick: function () { copy(r.otp, "코드 복사됨"); } }, "코드 복사"),
            el("button", { class: "btn ghost sm", onclick: function () { copy(JSON.stringify(r.payload), "페어링 정보 복사됨"); } }, "페어링 정보 복사")),
          el("label", { style: "margin-top:16px" }, "수동 페어링 정보"),
          el("pre", { class: "payload" }, JSON.stringify(r.payload, null, 2)));
        var right = r.qr ? el("div", { class: "qr" }, el("img", { src: r.qr, alt: "pairing QR" }))
                         : el("p", { class: "muted" }, "(QR 없음)");
        out.appendChild(el("div", { class: "pair-grid", style: "margin-top:18px" }, left, right));
        tick();
      } catch (e) { clear(out); out.appendChild(el("p", { class: "msg err" }, e.message)); }
    }
    function copy(t, ok) { if (navigator.clipboard) navigator.clipboard.writeText(t).then(function () { toast(ok, "ok"); }); }
  }

  // ---- settings --------------------------------------------------------------
  var SETTINGS_GROUPS = [
    { title: "제한", fields: [
      ["maxMessageBytes", "최대 WS 메시지 (KB)", "kb", "이보다 큰 인라인 텍스트는 블롭 채널을 사용합니다."],
      ["blobMaxBytes", "최대 블롭 크기 (KB)", "kb", "블롭 채널 업로드 파일당 상한."],
      ["onDemandThresholdBytes", "온디맨드 임계값 (KB)", "kb", "이 크기 이하 파일은 즉시 업로드, 초과 시 요청할 때만 전송."],
      ["blobStoreCapBytes", "블롭 저장소 상한 (KB)", "kb", "총 디스크 예산 초과 시 LRU로 삭제."],
    ] },
    { title: "공유 풀", fields: [
      ["pools", "풀 목록 (쉼표 구분)", "list", "기기는 이 풀들 중 하나에 속하며 같은 풀끼리만 동기화됩니다. 'default'는 항상 포함됩니다."],
    ] },
    { title: "보존", fields: [
      ["queueDepthPerDevice", "오프라인 큐 깊이", "int", "오프라인 기기당 최대 보관 클립 수."],
      ["queueItemTtlSeconds", "큐 항목 TTL(초)", "int", "이보다 오래된 큐 항목은 폐기."],
      ["blobTtlSeconds", "블롭 TTL(초)", "int", "참조 없는 블롭을 이 시간 후 삭제."],
    ] },
    { title: "보안 · 세션", fields: [
      ["e2eEnabled", "종단간 암호화(E2E)", "bool", "켜면 서버가 클립 내용을 못 봅니다 (Stage 3)."],
      ["allowServerBroadcast", "서버 브로드캐스트 허용", "bool", "E2E가 켜지면 자동으로 꺼집니다."],
      ["sessionTtlSeconds", "관리자 세션 TTL(초)", "int", ""],
      ["pairingCodeTtlSeconds", "페어링 코드 TTL(초)", "int", ""],
      ["tokenRotateDays", "토큰 회전 주기(일)", "int", "기기 토큰이 이보다 오래되면 자동 재발급합니다. 0 = 비활성. 구버전 클라이언트는 영향 없이 그대로 동작합니다."],
    ] },
  ];

  async function sectionSettings() {
    var m = state.main;
    m.appendChild(pageHead("설정", "런타임 설정 (즉시 적용)"));
    var holder = el("div", null, el("p", { class: "muted" }, "불러오는 중…"));
    m.appendChild(holder);
    try {
      var s = await api("GET", "/admin/settings");
      clear(holder);
      var inputs = {};
      SETTINGS_GROUPS.forEach(function (g) {
        var card = el("div", { class: "card" }, el("h2", null, g.title));
        var fg = el("div", { class: "field-grid" });
        var boolBox = el("div", null);
        g.fields.forEach(function (f) {
          var key = f[0], label = f[1], type = f[2], help = f[3];
          if (type === "bool") {
            var cb = el("input", { type: "checkbox" }); cb.checked = !!s[key];
            inputs[key] = { el: cb, type: type };
            boolBox.appendChild(el("div", { class: "switch-row" },
              el("div", { class: "lbl" }, el("b", null, label), el("span", null, help)),
              el("label", { class: "toggle" }, cb, el("span", { class: "track" }))));
          } else if (type === "list") {
            var lhint = el("div", { class: "help" }); lhint.textContent = help;
            var linp = el("input", { type: "text", value: (s[key] || []).join(", ") });
            inputs[key] = { el: linp, type: type };
            fg.appendChild(el("div", null, el("label", null, label), linp, lhint));
          } else {
            var hint = el("div", { class: "help" });
            var initVal = type === "kb" ? Math.round((s[key] || 0) / 1024) : s[key];
            var inp = el("input", { type: "number", min: "0", value: String(initVal),
              oninput: function () {
                if (type === "bytes") hint.textContent = fmtBytes(inp.value) + (help ? " — " + help : "");
                else if (type === "kb") hint.textContent = fmtBytes(inp.value * 1024) + (help ? " — " + help : "");
              } });
            inputs[key] = { el: inp, type: type };
            hint.textContent = (type === "bytes" || type === "kb") ? fmtBytes(s[key] || 0) + (help ? " — " + help : "") : help;
            fg.appendChild(el("div", null, el("label", null, label), inp, hint));
          }
        });
        if (fg.childNodes.length) card.appendChild(fg);
        if (boolBox.childNodes.length) card.appendChild(boolBox);
        holder.appendChild(card);
      });
      var save = el("button", { class: "btn", onclick: async function () {
        var payload = {};
        for (var k in inputs) {
          var it = inputs[k];
          payload[k] = it.type === "bool" ? it.el.checked
            : it.type === "kb" ? Math.round(Number(it.el.value) * 1024)
            : it.type === "list" ? it.el.value.split(",").map(function (x) { return x.trim(); }).filter(Boolean)
            : Number(it.el.value);
        }
        save.disabled = true;
        try { await api("PUT", "/admin/settings", payload); toast("설정 저장됨", "ok"); go("settings"); }
        catch (e) { toast(e.message, "err"); save.disabled = false; }
      }}, "설정 저장");
      holder.appendChild(el("div", { class: "row" }, save));
    } catch (e) { clear(holder); holder.appendChild(el("p", { class: "msg err" }, e.message)); }
  }

  // ---- downloads (file hosting) ----------------------------------------------
  async function sectionDownloads() {
    var m = state.main;
    m.appendChild(pageHead("다운로드", "파일을 올려 같은 LAN의 기기에서 내려받게 합니다."));
    var holder = el("div", null, el("p", { class: "muted" }, "불러오는 중…"));
    m.appendChild(holder);
    async function load() {
      clear(holder);
      var s = await api("GET", "/admin/settings");
      var on = !!s.downloadsEnabled;
      var cb = el("input", { type: "checkbox" }); cb.checked = on;
      cb.addEventListener("change", async function () {
        try {
          await api("PUT", "/admin/settings", { downloadsEnabled: cb.checked });
          toast(cb.checked ? "호스팅 켜짐" : "호스팅 꺼짐", "ok"); load();
        } catch (e) { toast(e.message, "err"); cb.checked = !cb.checked; }
      });
      holder.appendChild(el("div", { class: "card" }, el("h2", null, "파일 호스팅"),
        el("div", { class: "switch-row" },
          el("div", { class: "lbl" }, el("b", null, "다운로드 호스팅 켜기"),
            el("span", null, "켜면 같은 LAN의 누구나 인증 없이 내려받습니다.")),
          el("label", { class: "toggle" }, cb, el("span", { class: "track" }))),
        on ? el("p", { class: "help" }, "공개 주소: ",
          el("a", { href: "/downloads/", target: "_blank" }, location.origin + "/downloads/")) : null));

      var fileInput = el("input", { type: "file" });
      var up = el("button", { class: "btn" }, "업로드");
      up.addEventListener("click", async function () {
        if (!fileInput.files.length) { toast("파일을 선택하세요", "err"); return; }
        var fd = new FormData(); fd.append("file", fileInput.files[0]);
        up.disabled = true;
        try {
          var res = await fetch("/admin/downloads", { method: "POST", credentials: "same-origin",
            headers: { "X-CopySync-CSRF": "1" }, body: fd });
          if (!res.ok) throw new Error("업로드 실패 (" + res.status + ")");
          toast("업로드됨", "ok"); fileInput.value = ""; load();
        } catch (e) { toast(e.message, "err"); }
        up.disabled = false;
      });
      holder.appendChild(el("div", { class: "card" }, el("h2", null, "파일 추가"),
        el("div", { class: "row" }, fileInput, up),
        el("p", { class: "help" }, "또는 서버의 ./data/downloads/ 폴더에 직접 넣어도 됩니다.")));

      var listCard = el("div", { class: "card" }, el("h2", null, "호스팅 중인 파일"));
      var d = await api("GET", "/admin/downloads");
      if (!d.files || !d.files.length) listCard.appendChild(el("p", { class: "muted" }, "없음"));
      else d.files.forEach(function (f) {
        var del = el("button", { class: "btn ghost" }, "삭제");
        del.addEventListener("click", async function () {
          try {
            await api("DELETE", "/admin/downloads/" + encodeURIComponent(f.name));
            toast("삭제됨", "ok"); load();
          } catch (e) { toast(e.message, "err"); }
        });
        listCard.appendChild(el("div", { class: "switch-row" },
          el("a", { href: "/downloads/" + encodeURIComponent(f.name), target: "_blank" },
            f.name + " (" + fmtBytes(f.size) + ")"),
          del));
      });
      holder.appendChild(listCard);
    }
    load().catch(function (e) { clear(holder); holder.appendChild(el("p", { class: "msg err" }, e.message)); });
  }

  // ---- monitor (live) --------------------------------------------------------
  function sectionMonitor() {
    var m = state.main;
    m.appendChild(pageHead("모니터링", "들어오는 클립 실시간 — E2E 클립은 내용이 보이지 않습니다."));
    var heat = el("div", { class: "card" }, el("h2", null, "복사 잔디 (최근 17주)"));
    m.appendChild(heat);
    api("GET", "/admin/monitor/activity").then(function (a) {
      var grid = el("div", { class: "heat" });
      (a.days || []).forEach(function (d) {
        var fc = a.maxCount ? d.count / a.maxCount : 0; // frequency
        var fb = a.maxBytes ? d.bytes / a.maxBytes : 0; // volume
        var cell = el("div", { class: "heat-cell", title: d.date + " · " + d.count + "회 · " + fmtBytes(d.bytes) });
        cell.style.background = d.count
          ? "rgb(" + Math.round(18 + fb * 40) + "," + Math.round(55 + fc * 175) + "," + Math.round(55 + fb * 175) + ")"
          : "var(--border)";
        grid.appendChild(cell);
      });
      heat.appendChild(grid);
      heat.appendChild(el("p", { class: "muted small" }, "🟩 초록 = 빈도 · 🟦 파랑 = 용량 (블렌드)"));
    }).catch(function () {});
    var card = el("div", { class: "card" });
    var listEl = el("div", { class: "mon-list" }, el("p", { class: "muted" }, "대기 중… 클립이 들어오면 여기에 표시됩니다."));
    card.appendChild(listEl);
    m.appendChild(card);
    var first = true;
    var es = new EventSource("/admin/monitor/stream");
    state.monitorES = es;
    es.onmessage = function (e) {
      var ev;
      try { ev = JSON.parse(e.data); } catch (_) { return; }
      if (first) { clear(listEl); first = false; }
      var prevText = ev.preview || ("(" + (ev.mime || ev.kind) + " · " + fmtBytes(ev.size) + ")");
      var body = el("div", { class: "mon-body" });
      if (ev.kind === "image" && ev.blobId) {
        body.appendChild(el("img", {
          class: "mon-thumb", alt: "", src: "/admin/monitor/blob/" + encodeURIComponent(ev.blobId),
          onerror: function (e2) { e2.target.style.display = "none"; },
          onclick: function (e2) { e2.target.classList.toggle("big"); },
        }));
      }
      var textEl = el("span", { class: "mon-prev" }, prevText);
      body.appendChild(textEl);
      var actions = el("span", { class: "mon-actions" });
      if (prevText.length > 60) {
        var more = el("button", { class: "mon-btn", title: "전문 보기 / 접기" }, "⋯");
        more.onclick = function () { more.textContent = textEl.classList.toggle("expanded") ? "▲" : "⋯"; };
        actions.appendChild(more);
      }
      var copyBtn = el("button", { class: "mon-btn", title: "복사" }, "복사");
      copyBtn.onclick = function () {
        navigator.clipboard.writeText(prevText).then(function () {
          copyBtn.textContent = "✓"; setTimeout(function () { copyBtn.textContent = "복사"; }, 1200);
        }).catch(function () {});
      };
      actions.appendChild(copyBtn);
      var row = el("div", { class: "mon-row" },
        el("span", { class: "mon-chip" }, ev.pool || "default"),
        el("span", { class: "mon-kind" }, ev.kind || ""),
        body,
        el("span", { class: "mon-meta" }, (ev.origin || "") + " · " + (ev.ts || "")),
        actions);
      listEl.insertBefore(row, listEl.firstChild);
      while (listEl.childNodes.length > 200) listEl.removeChild(listEl.lastChild);
    };
  }

  // ---- account ---------------------------------------------------------------
  function sectionAccount() {
    var m = state.main;
    m.appendChild(pageHead("계정", "관리자 비밀번호"));
    var cur = el("input", { type: "password", autocomplete: "current-password" });
    var nw = el("input", { type: "password", autocomplete: "new-password" });
    var nw2 = el("input", { type: "password", autocomplete: "new-password" });
    var msg = el("p", { class: "msg" });
    var card = el("div", { class: "card" },
      el("h2", null, "비밀번호 변경"),
      el("div", { class: "field-grid" },
        el("div", null, el("label", null, "현재 비밀번호"), cur),
        el("div", null,
          el("label", null, "새 비밀번호 (8자 이상)"), nw,
          el("label", null, "새 비밀번호 확인"), nw2)),
      el("div", { class: "row", style: "margin-top:14px" },
        el("button", { class: "btn", onclick: async function () {
          msg.className = "msg"; msg.textContent = "";
          if (nw.value !== nw2.value) { msg.className = "msg err"; msg.textContent = "비밀번호가 일치하지 않습니다."; return; }
          try { await api("POST", "/admin/password", { current: cur.value, new: nw.value }); toast("비밀번호 변경됨", "ok"); cur.value = nw.value = nw2.value = ""; }
          catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
        }}, "비밀번호 저장")),
      msg);
    m.appendChild(card);
  }

  start();
})();
