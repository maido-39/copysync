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
listen("roster", (ev) => { roster = ev.payload || []; renderDevices(); });
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
listen("status", (ev) => renderStatus(ev.payload));
listen("clip", () => { if ($("#history").classList.contains("active")) loadHistory(); });
listen("error", (ev) => console.warn("copysync:", ev.payload));

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

$("#pool").addEventListener("change", (e) => {
  invoke("set_pool", { pool: e.target.value }).catch((err) => alert("풀 변경 실패: " + err));
});

refreshStatus();
setInterval(refreshStatus, 4000);
