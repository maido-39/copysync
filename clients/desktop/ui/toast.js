// A lightweight incoming-clip toast. Reliable on portable Windows where WinRT
// toast notifications need an installed app / registered AppUserModelID.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const fromEl = document.getElementById("from");
const bodyEl = document.getElementById("body");
const thumbEl = document.getElementById("thumb");
let timer = null;

listen("toast-show", (e) => {
  const p = e.payload || {};
  fromEl.textContent = p.title || "다른 기기";
  bodyEl.textContent = p.body || "";
  if (p.image) {
    thumbEl.src = p.image;
    thumbEl.style.display = "block";
  } else {
    thumbEl.style.display = "none";
    thumbEl.removeAttribute("src");
  }
  if (timer) clearTimeout(timer);
  timer = setTimeout(() => invoke("hide_toast").catch(() => {}), 5000);
});

// Click to dismiss immediately.
document.body.addEventListener("click", () => invoke("hide_toast").catch(() => {}));
