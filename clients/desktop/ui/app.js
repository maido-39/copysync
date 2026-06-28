const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);

// ---- tabs
document.querySelectorAll("nav#tabs button").forEach((b) => {
  b.addEventListener("click", () => {
    document.querySelectorAll("nav#tabs button").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    b.classList.add("active");
    $("#" + b.dataset.tab).classList.add("active");
    dbg("탭 전환 → " + b.dataset.tab);
    if (b.dataset.tab === "history") loadHistory();
  });
});

// ---- debug log (event recorder for the 디버깅 tab)
// Detailed debug mode: enable via the "이벤트 기록" checkbox (#dbg-record) on the
// 디버깅 tab. When ON, every relevant UI event (status/clip/roster/reconnect/theme/
// invoke calls) is recorded verbosely. Failures, reconnects and connection changes
// are ALWAYS recorded (dbgForce) regardless of the toggle, so a problem is captured
// even if recording wasn't enabled beforehand. The log lives in-memory (ring buffer,
// 800 lines) and is shown in #dbg-log; "복사" copies it to the clipboard and the
// browser/WebView devtools console mirrors every line for external capture.
let dbgRecording = false;
const dbgLines = [];
// Always recorded — failures, reconnects, connection changes — so they're in
// the Debug tab even if the user never turned "이벤트 기록" on before the problem.
function dbgForce(msg) {
  const t = new Date().toLocaleTimeString();
  const line = "[" + t + "] " + msg;
  dbgLines.push(line);
  if (dbgLines.length > 800) dbgLines.shift();
  const el = $("#dbg-log");
  if (el) el.textContent = dbgLines.join("\n");
  // Mirror to the console so it's also captured by external WebView log sinks.
  try { console.log("copysync.dbg", line); } catch (e) {}
}
// Verbose per-event lines — only when recording is enabled.
function dbg(msg) {
  if (!dbgRecording) return;
  dbgForce(msg);
}
// Structured error logger — every catch routes here so no error is swallowed.
// Records the operation that failed, the stringified error, and a JS stack trace
// where available. Always recorded (dbgForce), and mirrored to console.error.
function dbgErr(op, err, extra) {
  let detail = "";
  try { detail = err instanceof Error ? (err.message || String(err)) : String(err); }
  catch (e) { detail = "<unstringifiable error>"; }
  const ctx = extra ? " | " + extra : "";
  dbgForce("⚠️ [" + op + "] " + detail + ctx);
  // Stack trace where the language/runtime supports it.
  let stack = err && err.stack ? err.stack : null;
  if (!stack) { try { stack = new Error().stack; } catch (e) {} }
  if (stack) dbgForce("    ↳ " + String(stack).replace(/\n/g, "\n    "));
  try { console.error("copysync.err [" + op + "]", err, extra || ""); } catch (e) {}
}
$("#dbg-record").addEventListener("change", (e) => {
  dbgRecording = e.target.checked;
  dbgForce("기록 " + (dbgRecording ? "시작 (상세 디버그 ON)" : "중지"));
});
$("#dbg-copy").addEventListener("click", () =>
  navigator.clipboard.writeText(dbgLines.join("\n")).catch((err) => dbgErr("dbg-copy", err)));
$("#dbg-clear").addEventListener("click", () => { dbgLines.length = 0; const el = $("#dbg-log"); if (el) el.textContent = ""; });

// Catch-all: uncaught exceptions and unhandled promise rejections anywhere in the
// UI are recorded (with stack) instead of vanishing into the WebView console only.
window.addEventListener("error", (ev) => {
  dbgErr("window.error", ev.error || ev.message, ev.filename ? ev.filename + ":" + ev.lineno + ":" + ev.colno : "");
});
window.addEventListener("unhandledrejection", (ev) => { dbgErr("unhandledrejection", ev.reason); });
dbgForce("▶ UI 시작 (상세 디버그는 디버깅 탭의 '이벤트 기록'으로 켜기)");

// ---- status
function renderStatus(s) {
  $("#s-server").textContent = s.server_name || "—";
  $("#s-device").textContent = s.device_name || "—";
  $("#s-conn").textContent = s.connected ? "연결됨" : s.paired ? "재연결 중…" : "페어링 필요";
  $("#s-e2e").textContent = s.e2e ? "켜짐" : "꺼짐";
  $("#set-sid").textContent = s.server_id || "—";
  const c = $("#conn");
  c.textContent = s.connected ? "연결됨" : s.paired ? "재연결 중…" : "연결 끊김";
  c.className = "pill " + (s.connected ? "on" : s.paired ? "warn" : "off");
  const sel = $("#pool");
  if (sel) {
    const pools = s.pools && s.pools.length ? s.pools : ["default"];
    const cur = s.pool || "default";
    sel.innerHTML = pools.map((p) => `<option value="${esc(p)}"${p === cur ? " selected" : ""}>${esc(p)}</option>`).join("");
  }
}
async function refreshStatus() {
  try { renderStatus(await invoke("get_status")); } catch (e) { dbgErr("get_status", e); }
}

// ---- history
function esc(t) { const d = document.createElement("div"); d.textContent = t; return d.innerHTML; }
function fmtSize(n) {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB"]; let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(i ? 1 : 0)} ${u[i]}`;
}
let lastRows = [];
// Files/images keep their saved local path in `preview` (inbound: the downloaded
// file; outbound images: a locally cached PNG) — that's what we thumbnail/snippet.
// Text clips put the text itself there, so only treat path-looking values as files.
function itemPath(e) {
  const p = e.preview || "";
  const looksPath = p.includes("/") || p.includes("\\");
  if (e.kind === "image") return looksPath ? p : null;
  if (e.kind === "file" && e.direction === "in") return looksPath ? p : null;
  return null;
}
function itemHtml(e, i) {
  const label = e.kind === "image" ? "이미지" : e.kind === "file" ? "파일" : "텍스트";
  const head = e.kind === "text" ? esc(e.preview) : esc(e.name || e.preview);
  const out = e.direction === "out";
  // Direction coded by the word itself (보냄 strong / 받음 muted), not a colored stripe.
  const dirHtml = `<span class="${out ? "sent" : "recv"}">${out ? "보냄" : "받음"}</span>`;
  const rest = [e.origin && e.origin !== "me" ? e.origin : null, fmtSize(e.size), e.ts]
    .filter(Boolean).map(esc).join(" · ");
  const meta = dirHtml + (rest ? " · " + rest : "");
  const path = itemPath(e);
  let media = `<span class="ic">${e.kind === "image" ? "🖼️" : e.kind === "file" ? "📄" : "🔤"}</span>`;
  if (e.kind === "image" && path) media = `<img class="thumb" data-i="${i}" alt=""/>`;
  else if (e.kind === "file" && path && (e.mime || "").startsWith("text/")) media = `<pre class="snip" data-i="${i}">…</pre>`;
  return `<div class="item">
    <span class="tag ${e.kind}">${label}</span>
    ${media}
    <div class="body"><div class="preview">${head}</div><div class="meta">${meta}</div></div>
  </div>`;
}
function hydrate() {
  document.querySelectorAll("#hist-list [data-i]").forEach((el) => {
    const e = lastRows[+el.dataset.i];
    if (!e) return;
    const path = itemPath(e);
    if (!path) return;
    if (el.tagName === "IMG") invoke("thumbnail", { path }).then((d) => { if (d) el.src = d; }).catch((err) => dbgErr("thumbnail", err, "path=" + path));
    else invoke("text_preview", { path }).then((t) => { if (t) el.textContent = t; }).catch((err) => dbgErr("text_preview", err, "path=" + path));
  });
}
async function loadHistory() {
  const q = $("#search").value;
  try {
    lastRows = await invoke("get_history", { query: q || null });
    dbg("기록 로드 " + lastRows.length + "건 (검색=" + (q || "·") + ")");
    $("#hist-list").innerHTML = lastRows.length
      ? lastRows.map((e, i) => itemHtml(e, i)).join("")
      : `<div class="empty">기록이 없습니다.</div>`;
    hydrate();
  } catch (e) {
    dbgErr("get_history", e, "query=" + (q || ""));
    $("#hist-list").innerHTML = `<div class="empty">${esc(String(e))}</div>`;
  }
}
$("#search").addEventListener("input", () => loadHistory());
$("#refresh").addEventListener("click", () => loadHistory());

// ---- send text
$("#send-btn").addEventListener("click", async () => {
  const t = $("#send-text").value;
  if (!t) return;
  try { await invoke("send_text", { text: t }); $("#send-text").value = ""; dbg("텍스트 전송 " + t.length + "자"); }
  catch (e) { dbgErr("send_text", e, "len=" + t.length); alert("보내기 실패: " + e); }
});

// ---- send file (native dialog → send_file command)
$("#file-btn").addEventListener("click", async () => {
  try {
    const dlg = window.__TAURI__.dialog;
    if (!dlg) { dbgForce("⚠️ [send_file] dialog 플러그인 없음"); alert("파일 대화상자를 사용할 수 없습니다"); return; }
    const path = await dlg.open({ multiple: false, directory: false });
    if (path) { await invoke("send_file", { path }); dbg("파일 전송 " + path); }
  } catch (e) { dbgErr("send_file", e); alert("파일 보내기 실패: " + e); }
});

// ---- routing (per-device targets)
let roster = [];
function curMode() { return document.querySelector('input[name=route]:checked').value; }
function renderDevices() {
  const box = $("#dev-list");
  box.hidden = curMode() !== "some";
  if (box.hidden) return;
  box.innerHTML = roster.length
    ? roster.map((d) =>
        `<label class="dev"><input type="checkbox" value="${esc(d.id)}"/>` +
        `<span class="dot ${d.online ? "on" : "off"}"></span>${esc(d.name)}</label>`).join("")
    : `<div class="hint">알려진 기기가 없습니다.</div>`;
  box.querySelectorAll("input[type=checkbox]").forEach((cb) => cb.addEventListener("change", applyTargets));
}
function applyTargets() {
  let ids = [];
  if (curMode() === "some") ids = [...$("#dev-list").querySelectorAll("input:checked")].map((c) => c.value);
  dbg("타겟 설정 mode=" + curMode() + " ids=[" + ids.join(",") + "]");
  invoke("set_targets", { ids }).catch((err) => dbgErr("set_targets", err, "ids=[" + ids.join(",") + "]"));
}
document.querySelectorAll('input[name=route]').forEach((r) =>
  r.addEventListener("change", () => { renderDevices(); applyTargets(); }));
async function loadRoster() {
  try { roster = await invoke("get_roster"); dbg("로스터 로드 " + roster.length + "대"); renderDevices(); } catch (e) { dbgErr("get_roster", e); }
}
listen("roster", (ev) => { roster = ev.payload || []; dbg("로스터 " + roster.length + "대"); renderDevices(); });
loadRoster();

// ---- pair
$("#pair-btn").addEventListener("click", async () => {
  const msg = $("#pair-msg");
  msg.className = "msg"; msg.textContent = "페어링 중…";
  try {
    const s = await invoke("pair", {
      server: $("#p-server").value.trim(),
      otp: $("#p-otp").value.trim(),
      name: $("#p-name").value.trim() || "desktop",
      pin: $("#p-pin").value.trim(),
      e2ePass: $("#p-e2e").value,
    });
    msg.className = "msg ok"; msg.textContent = "페어링 완료!";
    dbgForce("🔗 페어링 완료 server=" + $("#p-server").value.trim());
    renderStatus(s);
  } catch (e) {
    dbgErr("pair", e, "server=" + $("#p-server").value.trim());
    msg.className = "msg err"; msg.textContent = "실패: " + e;
  }
});

// ---- live events
let lastConn = null;
listen("status", (ev) => {
  const s = ev.payload || {};
  if (s.connected !== lastConn) { lastConn = s.connected; dbgForce("🔌 연결 " + (s.connected ? "됨" : "끊김")); }
  renderStatus(s);
});
// Pink translucent toast shown when the privacy filter blocks an outbound clip.
const REASON_KO = {
  "password-like": "비밀번호로 추정",
  "payment card": "카드번호로 추정",
  "private key": "개인 키",
  "OTP secret": "OTP/2FA 비밀",
  "custom pattern": "사용자 패턴",
};
function showBlockedToast(reasonLabel, content) {
  const wrap = $("#toasts");
  if (!wrap) return;
  const el = document.createElement("div");
  el.className = "toast";
  const ico = document.createElement("span");
  ico.className = "ico"; ico.textContent = "🔒";
  const bodyEl = document.createElement("div"); bodyEl.className = "body";
  const title = document.createElement("div"); title.className = "title";
  title.textContent = "동기화 차단됨";
  const reason = document.createElement("span");
  reason.className = "reason";
  reason.textContent = REASON_KO[reasonLabel] || reasonLabel || "민감 정보";
  title.appendChild(reason);
  const preview = document.createElement("div"); preview.className = "preview";
  preview.textContent = (content || "").slice(0, 90); // textContent → safe vs clipboard HTML
  bodyEl.appendChild(title); bodyEl.appendChild(preview);
  const x = document.createElement("span"); x.className = "x"; x.textContent = "✕";
  el.appendChild(ico); el.appendChild(bodyEl); el.appendChild(x);
  wrap.appendChild(el);
  const kill = () => { el.classList.add("out"); setTimeout(() => el.remove(), 300); };
  x.addEventListener("click", kill);
  setTimeout(kill, 5200);
}
listen("clip", (ev) => {
  const p = ev.payload || {};
  const arrow = p.direction === "out" ? "↑보냄" : "↓받음";
  const body = p.text ? p.text.slice(0, 60) : (p.name || "");
  dbg("클립 " + arrow + " " + (p.kind || "text") + " " + (p.sensitive ? "🔒 " : "") + body);
  if (p.sensitive) showBlockedToast(p.sensitive, p.text || p.name);
  if ($("#history").classList.contains("active")) loadHistory();
});
listen("error", (ev) => { dbgForce("⚠️ [backend] " + ev.payload); try { console.warn("copysync:", ev.payload); } catch (e) {} });
// Clipboard-watcher diagnostics (e.g. RDP/virtual file copies that aren't CF_HDROP).
listen("cliplog", (ev) => dbg("📋 " + ev.payload));
// Reconnect attempts (exponential backoff) — show the countdown + log it.
listen("reconnect", (ev) => {
  dbgForce("🔄 재연결 " + ev.payload);
  const c = $("#s-conn"); if (c) c.textContent = "재연결 중 · " + ev.payload;
});

// ---- autostart
async function loadAutostart() {
  try { $("#autostart").checked = await invoke("get_autostart"); } catch (e) { dbgErr("get_autostart", e); }
}
$("#autostart").addEventListener("change", async (e) => {
  try {
    await invoke("set_autostart", { enabled: e.target.checked });
    dbg("자동 시작 " + (e.target.checked ? "켜짐" : "꺼짐"));
  } catch (err) {
    dbgErr("set_autostart", err, "enabled=" + e.target.checked);
    alert("자동 시작 설정 실패: " + err);
    e.target.checked = !e.target.checked;
  }
});
loadAutostart();

// ---- privacy filter (toggle: don't sync sensitive clips)
async function loadPrivacyFilter() {
  try { $("#privacy-filter").checked = await invoke("get_privacy_filter"); } catch (e) { dbgErr("get_privacy_filter", e); }
}
$("#privacy-filter").addEventListener("change", async (e) => {
  try {
    await invoke("set_privacy_filter", { enabled: e.target.checked });
    dbg("개인정보 필터 " + (e.target.checked ? "켜짐" : "꺼짐"));
  } catch (err) {
    dbgErr("set_privacy_filter", err, "enabled=" + e.target.checked);
    alert("필터 설정 실패: " + err);
    e.target.checked = !e.target.checked;
  }
});
loadPrivacyFilter();

// ---- Quick Panel global hotkey (설정 → 단축키)
let recordingShortcut = false;
function prettyAccel(accel) {
  if (!accel) return "";
  return accel.split("+").map((t) => {
    const u = t.toUpperCase();
    if (u === "CONTROL" || u === "CTRL") return "Ctrl";
    if (u === "COMMANDORCONTROL" || u === "COMMANDORCTRL" || u === "CMDORCTRL" || u === "CMDORCONTROL") return "Ctrl";
    if (u === "SHIFT") return "Shift";
    if (u === "ALT" || u === "OPTION") return "Alt";
    if (u === "SUPER" || u === "COMMAND" || u === "CMD" || u === "META") return "Win";
    if (u.startsWith("KEY") && t.length > 3) return t.slice(3);
    if (u.startsWith("DIGIT")) return t.slice(5);
    if (u.startsWith("ARROW")) return t.slice(5);
    return t;
  }).join("+");
}
async function loadShortcut() {
  try {
    const a = await invoke("get_shortcut");
    const box = $("#shortcut");
    box.dataset.accel = a || "";
    box.value = prettyAccel(a) || "없음";
  } catch (e) { dbgErr("get_shortcut", e); }
}
$("#shortcut-record").addEventListener("click", () => {
  recordingShortcut = true;
  $("#shortcut").value = "키 조합을 누르세요…";
  $("#shortcut").focus();
});
$("#shortcut-clear").addEventListener("click", async () => {
  try { await invoke("set_shortcut", { accel: "" }); dbg("단축키 해제"); } catch (e) { dbgErr("set_shortcut(clear)", e); }
  await loadShortcut();
});
$("#shortcut").addEventListener("keydown", async (e) => {
  if (!recordingShortcut) return;
  e.preventDefault();
  e.stopPropagation();
  const code = e.code;
  const MODKEYS = ["ControlLeft", "ControlRight", "ShiftLeft", "ShiftRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight"];
  if (MODKEYS.includes(code)) return;            // wait for a non-modifier key
  if (code === "Escape") { recordingShortcut = false; loadShortcut(); return; }
  const parts = [];
  if (e.ctrlKey) parts.push("Control");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Super");
  if (!parts.length) { $("#shortcut").value = "Ctrl/Alt/Shift 를 포함하세요"; return; }
  parts.push(code);                              // e.code (KeyV, Digit1, F5…) is what the Rust parser accepts
  const accel = parts.join("+");
  recordingShortcut = false;
  try {
    await invoke("set_shortcut", { accel });
    await loadShortcut();
    dbg("단축키 변경 " + accel);
  } catch (err) {
    dbgErr("set_shortcut", err, "accel=" + accel);
    $("#shortcut").value = "사용 불가: " + err;
    setTimeout(loadShortcut, 1800);
  }
});
$("#shortcut").addEventListener("blur", () => {
  if (recordingShortcut) { recordingShortcut = false; loadShortcut(); }
});
loadShortcut();

// ---- mark received clips sensitive (exclude from OS clipboard history)
async function loadMarkSensitive() {
  try { $("#mark-sensitive").checked = await invoke("get_mark_sensitive"); } catch (e) { dbgErr("get_mark_sensitive", e); }
}
$("#mark-sensitive").addEventListener("change", async (e) => {
  try { await invoke("set_mark_sensitive", { enabled: e.target.checked }); dbg("민감 표시 " + (e.target.checked ? "켜짐" : "꺼짐")); }
  catch (err) { dbgErr("set_mark_sensitive", err, "enabled=" + e.target.checked); alert("설정 실패: " + err); e.target.checked = !e.target.checked; }
});
loadMarkSensitive();

// ---- auto-clear the clipboard N seconds after a received clip
function paintAutoClear(secs) {
  document.querySelectorAll("#autoclear-row .ac").forEach((b) => {
    b.classList.toggle("primary", Number(b.dataset.secs) === Number(secs));
  });
}
async function loadAutoClear() {
  try { paintAutoClear(await invoke("get_auto_clear")); } catch (e) { dbgErr("get_auto_clear", e); }
}
document.querySelectorAll("#autoclear-row .ac").forEach((b) => {
  b.addEventListener("click", async () => {
    const secs = Number(b.dataset.secs);
    try { await invoke("set_auto_clear", { secs }); paintAutoClear(secs); dbg("자동 비우기 " + secs + "초"); }
    catch (err) { dbgErr("set_auto_clear", err, "secs=" + secs); alert("자동 비우기 설정 실패: " + err); }
  });
});
loadAutoClear();

// ---- mDNS server discovery (fills the server field on click)
$("#discover-btn").addEventListener("click", async () => {
  const box = $("#discover-list");
  box.innerHTML = `<p class="hint">검색 중…</p>`;
  try {
    const found = await invoke("discover_servers");
    dbg("mDNS 검색 결과 " + found.length + "대");
    if (!found.length) { box.innerHTML = `<p class="hint">서버를 찾지 못했습니다 (같은 LAN인지 확인).</p>`; return; }
    const st = "display:block;width:100%;text-align:left;margin-top:6px;padding:9px 11px;border-radius:8px;background:var(--panel2);color:var(--fg);border:1px solid var(--line);cursor:pointer";
    box.innerHTML = found.map((s) =>
      `<button type="button" class="found" data-url="${esc(s.url)}" style="${st}">${esc(s.name)} — ${esc(s.url)}</button>`
    ).join("");
    box.querySelectorAll(".found").forEach((b) =>
      b.addEventListener("click", () => { $("#p-server").value = b.dataset.url; }));
  } catch (e) { dbgErr("discover_servers", e); box.innerHTML = `<p class="hint">검색 실패: ${esc(String(e))}</p>`; }
});

$("#pool").addEventListener("change", (e) => {
  dbg("풀 변경 → " + e.target.value);
  invoke("set_pool", { pool: e.target.value }).catch((err) => { dbgErr("set_pool", err, "pool=" + e.target.value); alert("풀 변경 실패: " + err); });
});

$("#reconnect-btn").addEventListener("click", () => {
  dbgForce("🔄 수동 재연결 요청");
  invoke("reconnect").catch((err) => dbgErr("reconnect", err));
  const c = $("#s-conn"); if (c) c.textContent = "재연결 중…";
});

refreshStatus();
setInterval(refreshStatus, 4000);

// ---- theme (dark/light + background image + box transparency) -------------
const THEME_KEY = "cs-theme";
const themeDefaults = { mode: "dark", img: "", x: 50, y: 50, zoom: 1, bright: 1, blur: 0, cardOp: 1 };
let theme = (() => {
  try { return Object.assign({}, themeDefaults, JSON.parse(localStorage.getItem(THEME_KEY) || "{}")); }
  catch (e) { try { console.error("copysync.err [loadTheme]", e); } catch (_) {} return Object.assign({}, themeDefaults); }
})();
function saveTheme() { try { localStorage.setItem(THEME_KEY, JSON.stringify(theme)); } catch (e) { dbgErr("saveTheme", e); } }
const darkMql = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
function applyTheme() {
  const root = document.documentElement;
  const resolved = theme.mode === "system" ? (darkMql && !darkMql.matches ? "light" : "dark") : theme.mode;
  root.dataset.theme = resolved === "light" ? "light" : "dark";
  root.style.setProperty("--bg-img", theme.img ? `url("${theme.img}")` : "none");
  root.style.setProperty("--scrim", theme.img ? "0.62" : "0"); // readability wash over a wallpaper
  // Gate the costly backdrop-filter blur on having a wallpaper (CSS body.has-bg).
  document.body.classList.toggle("has-bg", !!theme.img);
  dbg("테마 적용 mode=" + theme.mode + " bg=" + (theme.img ? "있음" : "없음") +
      " op=" + theme.cardOp + " blur=" + theme.blur + " zoom=" + theme.zoom + " bright=" + theme.bright);
  root.style.setProperty("--bg-x", theme.x + "%");
  root.style.setProperty("--bg-y", theme.y + "%");
  root.style.setProperty("--bg-zoom", theme.zoom);
  root.style.setProperty("--bg-bright", theme.bright);
  root.style.setProperty("--bg-blur", theme.blur + "px");
  root.style.setProperty("--card-opacity", theme.cardOp);
}
function setRange(id, v, suf) {
  const i = $("#" + id); if (i) i.value = v;
  const s = $("#" + id + "-v"); if (s) s.textContent = (Math.round(v * 100) / 100) + (suf || "");
}
function syncThemeControls() {
  document.querySelectorAll("#theme-mode button").forEach((b) => b.classList.toggle("sel", b.dataset.mode === theme.mode));
  setRange("bg-zoom", theme.zoom, "×"); setRange("bg-bright", theme.bright, "");
  setRange("bg-blur", theme.blur, "px"); setRange("card-op", theme.cardOp, "");
  const box = $("#bg-crop"); if (box) box.style.display = theme.img ? "block" : "none";
}
document.querySelectorAll("#theme-mode button").forEach((b) => {
  b.addEventListener("click", () => { theme.mode = b.dataset.mode; applyTheme(); syncThemeControls(); saveTheme(); });
});
function wireSlider(id, key, suf) {
  const i = $("#" + id); if (!i) return;
  i.addEventListener("input", () => { theme[key] = Number(i.value); setRange(id, i.value, suf); applyTheme(); saveTheme(); });
}
wireSlider("bg-zoom", "zoom", "×"); wireSlider("bg-bright", "bright", "");
wireSlider("bg-blur", "blur", "px"); wireSlider("card-op", "cardOp", "");
$("#bg-pick").addEventListener("click", () => $("#bg-file").click());
$("#bg-file").addEventListener("change", (e) => {
  const f = e.target.files[0]; if (!f) return;
  const rd = new FileReader();
  rd.onload = () => {
    const im = new Image();
    im.onload = () => {
      try {
        const max = 1600, sc = Math.min(1, max / Math.max(im.width, im.height));
        const c = document.createElement("canvas");
        c.width = Math.round(im.width * sc); c.height = Math.round(im.height * sc);
        c.getContext("2d").drawImage(im, 0, 0, c.width, c.height);
        theme.img = c.toDataURL("image/jpeg", 0.82);
        theme.x = 50; theme.y = 50; applyTheme(); syncThemeControls(); saveTheme();
        dbg("배경 이미지 설정 " + c.width + "×" + c.height);
      } catch (err) { dbgErr("bg-image:encode", err, "name=" + f.name); alert("배경 이미지 처리 실패: " + err); }
    };
    im.onerror = (err) => dbgErr("bg-image:decode", err, "name=" + f.name);
    im.src = rd.result;
  };
  rd.onerror = () => dbgErr("bg-image:read", rd.error, "name=" + f.name);
  rd.readAsDataURL(f);
});
$("#bg-clear").addEventListener("click", () => { theme.img = ""; applyTheme(); syncThemeControls(); saveTheme(); });
(() => {
  const box = $("#bg-crop"); if (!box) return;
  let dragging = false;
  // Pan updates the position live (applyTheme each frame) but localStorage is
  // only written on mouseup — writing JSON every mousemove frame was needless churn.
  const pan = (e) => {
    if (!dragging || !theme.img) return;
    const r = box.getBoundingClientRect();
    theme.x = Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100));
    theme.y = Math.max(0, Math.min(100, ((e.clientY - r.top) / r.height) * 100));
    applyTheme();
  };
  box.addEventListener("mousedown", (e) => { if (theme.img) { dragging = true; pan(e); e.preventDefault(); } });
  window.addEventListener("mousemove", pan);
  window.addEventListener("mouseup", () => {
    if (dragging) { dragging = false; saveTheme(); dbg("배경 위치 저장 x=" + Math.round(theme.x) + " y=" + Math.round(theme.y)); }
  });
})();
if (darkMql) darkMql.addEventListener("change", () => { if (theme.mode === "system") applyTheme(); });
applyTheme(); syncThemeControls();
