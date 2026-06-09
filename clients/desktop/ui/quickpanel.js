// Quick Panel: a compact, always-on-top history overlay. Toggled by the global
// hotkey (Ctrl/Cmd+Shift+V). Clicking a text item copies it back to the OS
// clipboard (which the watcher re-syncs) and hides the panel.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const listEl = document.getElementById("list");

async function refresh() {
  let items = [];
  try {
    items = await invoke("get_history", { query: null });
  } catch (e) {
    listEl.innerHTML = '<div class="empty">기록을 불러오지 못했습니다.</div>';
    return;
  }
  const texty = (items || []).filter((e) => e.kind === "text" && e.preview).slice(0, 20);
  listEl.innerHTML = "";
  if (!texty.length) {
    listEl.innerHTML = '<div class="empty">텍스트 기록이 아직 없습니다.</div>';
    return;
  }
  for (const e of texty) {
    const row = document.createElement("div");
    row.className = "it";
    const dir = document.createElement("span");
    dir.className = "dir";
    dir.textContent = e.direction === "out" ? "↑" : "↓";
    const txt = document.createElement("span");
    txt.className = "txt";
    txt.textContent = e.preview;
    row.appendChild(dir);
    row.appendChild(txt);
    row.title = e.preview;
    row.onclick = () => invoke("quickpanel_copy", { text: e.preview }).catch(() => {});
    listEl.appendChild(row);
  }
}

// Refresh whenever the panel is shown by the hotkey, and once at load.
listen("quickpanel-show", refresh);
refresh();
