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
    if (b.dataset.tab === "history") loadHistory();
  });
});

// ---- debug log (event recorder for the 디버깅 tab)
let dbgRecording = false;
const dbgLines = [];
// Always recorded — failures, reconnects, connection changes — so they're in
// the Debug tab even if the user never turned "이벤트 기록" on before the problem.
function dbgForce(msg) {
  const t = new Date().toLocaleTimeString();
  dbgLines.push("[" + t + "] " + msg);
  if (dbgLines.length > 800) dbgLines.shift();
  const el = $("#dbg-log");
  if (el) el.textContent = dbgLines.join("\n");
}
// Verbose per-event lines — only when recording is enabled.
function dbg(msg) {
  if (!dbgRecording) return;
  dbgForce(msg);
}
$("#dbg-record").addEventListener("change", (e) => {
  dbgRecording = e.target.checked;
  dbg("기록 " + (dbgRecording ? "시작" : "중지"));
});
$("#dbg-copy").addEventListener("click", () => navigator.clipboard.writeText(dbgLines.join("\n")).catch(() => {}));
$("#dbg-clear").addEventListener("click", () => { dbgLines.length = 0; const el = $("#dbg-log"); if (el) el.textContent = ""; });

// ---- status
function renderStatus(s) {
  $("#s-server").textContent = s.server_name || "—";
  $("#s-device").textContent = s.device_name || "—";
  $("#s-conn").textContent = s.connected ? "연결됨" : s.paired ? "재연결 중…" : "페어링 필요";
  $("#s-e2e").textContent = s.e2e ? "켜짐" : "꺼짐";
  $("#set-sid").textContent = s.server_id || "—";
  const c = $("#conn");
  c.textContent = s.connected ? "연결됨" : "연결 끊김";
  c.className = "pill " + (s.connected ? "on" : "off");
  const sel = $("#pool");
  if (sel) {
    const pools = s.pools && s.pools.length ? s.pools : ["default"];
    const cur = s.pool || "default";
    sel.innerHTML = pools.map((p) => `<option value="${esc(p)}"${p === cur ? " selected" : ""}>${esc(p)}</option>`).join("");
  }
}
async function refreshStatus() {
  try { renderStatus(await invoke("get_status")); } catch (e) {}
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
  const dir = e.direction === "out" ? "보냄" : "받음";
  const meta = [dir, e.origin && e.origin !== "me" ? e.origin : null, fmtSize(e.size), e.ts]
    .filter(Boolean).join(" · ");
  const path = itemPath(e);
  let media = `<span class="ic">${e.kind === "image" ? "🖼️" : e.kind === "file" ? "📄" : "🔤"}</span>`;
  if (e.kind === "image" && path) media = `<img class="thumb" data-i="${i}" alt=""/>`;
  else if (e.kind === "file" && path && (e.mime || "").startsWith("text/")) media = `<pre class="snip" data-i="${i}">…</pre>`;
  return `<div class="item dir-${e.direction}">
    <span class="tag ${e.kind}">${label}</span>
    ${media}
    <div class="body"><div class="preview">${head}</div><div class="meta">${esc(meta)}</div></div>
  </div>`;
}
function hydrate() {
  document.querySelectorAll("#hist-list [data-i]").forEach((el) => {
    const e = lastRows[+el.dataset.i];
    if (!e) return;
    const path = itemPath(e);
    if (!path) return;
    if (el.tagName === "IMG") invoke("thumbnail", { path }).then((d) => { if (d) el.src = d; }).catch(() => {});
    else invoke("text_preview", { path }).then((t) => { if (t) el.textContent = t; }).catch(() => {});
  });
}
async function loadHistory() {
  const q = $("#search").value;
  try {
    lastRows = await invoke("get_history", { query: q || null });
    $("#hist-list").innerHTML = lastRows.length
      ? lastRows.map((e, i) => itemHtml(e, i)).join("")
      : `<div class="empty">기록이 없습니다.</div>`;
    hydrate();
  } catch (e) {
    $("#hist-list").innerHTML = `<div class="empty">${esc(String(e))}</div>`;
  }
}
$("#search").addEventListener("input", () => loadHistory());
$("#refresh").addEventListener("click", () => loadHistory());

// ---- send text
$("#send-btn").addEventListener("click", async () => {
  const t = $("#send-text").value;
  if (!t) return;
  try { await invoke("send_text", { text: t }); $("#send-text").value = ""; }
  catch (e) { alert("보내기 실패: " + e); }
});

// ---- send file (native dialog → send_file command)
$("#file-btn").addEventListener("click", async () => {
  try {
    const dlg = window.__TAURI__.dialog;
    if (!dlg) { alert("파일 대화상자를 사용할 수 없습니다"); return; }
    const path = await dlg.open({ multiple: false, directory: false });
    if (path) await invoke("send_file", { path });
  } catch (e) { alert("파일 보내기 실패: " + e); }
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
  invoke("set_targets", { ids }).catch(() => {});
}
document.querySelectorAll('input[name=route]').forEach((r) =>
  r.addEventListener("change", () => { renderDevices(); applyTargets(); }));
async function loadRoster() {
  try { roster = await invoke("get_roster"); renderDevices(); } catch (e) {}
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
    renderStatus(s);
  } catch (e) {
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
listen("error", (ev) => { dbgForce("⚠️ " + ev.payload); console.warn("copysync:", ev.payload); });
// Clipboard-watcher diagnostics (e.g. RDP/virtual file copies that aren't CF_HDROP).
listen("cliplog", (ev) => dbg("📋 " + ev.payload));
// Reconnect attempts (exponential backoff) — show the countdown + log it.
listen("reconnect", (ev) => {
  dbgForce("🔄 재연결 " + ev.payload);
  const c = $("#s-conn"); if (c) c.textContent = "재연결 중 · " + ev.payload;
});

// ---- autostart
async function loadAutostart() {
  try { $("#autostart").checked = await invoke("get_autostart"); } catch (e) {}
}
$("#autostart").addEventListener("change", async (e) => {
  try {
    await invoke("set_autostart", { enabled: e.target.checked });
  } catch (err) {
    alert("자동 시작 설정 실패: " + err);
    e.target.checked = !e.target.checked;
  }
});
loadAutostart();

// ---- privacy filter (toggle: don't sync sensitive clips)
async function loadPrivacyFilter() {
  try { $("#privacy-filter").checked = await invoke("get_privacy_filter"); } catch (e) {}
}
$("#privacy-filter").addEventListener("change", async (e) => {
  try {
    await invoke("set_privacy_filter", { enabled: e.target.checked });
  } catch (err) {
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
  } catch (e) {}
}
$("#shortcut-record").addEventListener("click", () => {
  recordingShortcut = true;
  $("#shortcut").value = "키 조합을 누르세요…";
  $("#shortcut").focus();
});
$("#shortcut-clear").addEventListener("click", async () => {
  try { await invoke("set_shortcut", { accel: "" }); } catch (e) {}
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
  try { $("#mark-sensitive").checked = await invoke("get_mark_sensitive"); } catch (e) {}
}
$("#mark-sensitive").addEventListener("change", async (e) => {
  try { await invoke("set_mark_sensitive", { enabled: e.target.checked }); }
  catch (err) { alert("설정 실패: " + err); e.target.checked = !e.target.checked; }
});
loadMarkSensitive();

// ---- auto-clear the clipboard N seconds after a received clip
function paintAutoClear(secs) {
  document.querySelectorAll("#autoclear-row .ac").forEach((b) => {
    b.classList.toggle("primary", Number(b.dataset.secs) === Number(secs));
  });
}
async function loadAutoClear() {
  try { paintAutoClear(await invoke("get_auto_clear")); } catch (e) {}
}
document.querySelectorAll("#autoclear-row .ac").forEach((b) => {
  b.addEventListener("click", async () => {
    const secs = Number(b.dataset.secs);
    try { await invoke("set_auto_clear", { secs }); paintAutoClear(secs); }
    catch (err) { alert("자동 비우기 설정 실패: " + err); }
  });
});
loadAutoClear();

// ---- mDNS server discovery (fills the server field on click)
$("#discover-btn").addEventListener("click", async () => {
  const box = $("#discover-list");
  box.innerHTML = `<p class="hint">검색 중…</p>`;
  try {
    const found = await invoke("discover_servers");
    if (!found.length) { box.innerHTML = `<p class="hint">서버를 찾지 못했습니다 (같은 LAN인지 확인).</p>`; return; }
    const st = "display:block;width:100%;text-align:left;margin-top:6px;padding:9px 11px;border-radius:8px;background:var(--panel2);color:var(--fg);border:1px solid var(--line);cursor:pointer";
    box.innerHTML = found.map((s) =>
      `<button type="button" class="found" data-url="${esc(s.url)}" style="${st}">${esc(s.name)} — ${esc(s.url)}</button>`
    ).join("");
    box.querySelectorAll(".found").forEach((b) =>
      b.addEventListener("click", () => { $("#p-server").value = b.dataset.url; }));
  } catch (e) { box.innerHTML = `<p class="hint">검색 실패: ${esc(String(e))}</p>`; }
});

$("#pool").addEventListener("change", (e) => {
  invoke("set_pool", { pool: e.target.value }).catch((err) => alert("풀 변경 실패: " + err));
});

$("#reconnect-btn").addEventListener("click", () => {
  invoke("reconnect").catch(() => {});
  const c = $("#s-conn"); if (c) c.textContent = "재연결 중…";
});

refreshStatus();
setInterval(refreshStatus, 4000);

// ---- theme (dark/light + background image + box transparency) -------------
const THEME_KEY = "cs-theme";
const themeDefaults = { mode: "dark", img: "", x: 50, y: 50, zoom: 1, bright: 1, blur: 0, cardOp: 1 };
let theme = (() => {
  try { return Object.assign({}, themeDefaults, JSON.parse(localStorage.getItem(THEME_KEY) || "{}")); }
  catch (e) { return Object.assign({}, themeDefaults); }
})();
function saveTheme() { try { localStorage.setItem(THEME_KEY, JSON.stringify(theme)); } catch (e) {} }
const darkMql = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
function applyTheme() {
  const root = document.documentElement;
  const resolved = theme.mode === "system" ? (darkMql && !darkMql.matches ? "light" : "dark") : theme.mode;
  root.dataset.theme = resolved === "light" ? "light" : "dark";
  root.style.setProperty("--bg-img", theme.img ? `url("${theme.img}")` : "none");
  root.style.setProperty("--scrim", theme.img ? "0.62" : "0"); // readability wash over a wallpaper
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
      const max = 1600, sc = Math.min(1, max / Math.max(im.width, im.height));
      const c = document.createElement("canvas");
      c.width = Math.round(im.width * sc); c.height = Math.round(im.height * sc);
      c.getContext("2d").drawImage(im, 0, 0, c.width, c.height);
      theme.img = c.toDataURL("image/jpeg", 0.82);
      theme.x = 50; theme.y = 50; applyTheme(); syncThemeControls(); saveTheme();
    };
    im.src = rd.result;
  };
  rd.readAsDataURL(f);
});
$("#bg-clear").addEventListener("click", () => { theme.img = ""; applyTheme(); syncThemeControls(); saveTheme(); });
(() => {
  const box = $("#bg-crop"); if (!box) return;
  let dragging = false;
  const pan = (e) => {
    if (!dragging || !theme.img) return;
    const r = box.getBoundingClientRect();
    theme.x = Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100));
    theme.y = Math.max(0, Math.min(100, ((e.clientY - r.top) / r.height) * 100));
    applyTheme(); saveTheme();
  };
  box.addEventListener("mousedown", (e) => { if (theme.img) { dragging = true; pan(e); e.preventDefault(); } });
  window.addEventListener("mousemove", pan);
  window.addEventListener("mouseup", () => { dragging = false; });
})();
if (darkMql) darkMql.addEventListener("change", () => { if (theme.mode === "system") applyTheme(); });
applyTheme(); syncThemeControls();
