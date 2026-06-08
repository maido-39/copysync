(function () {
  "use strict";

  var app = document.getElementById("app");
  var logoutBtn = document.getElementById("logout");
  var serverNameEl = document.getElementById("server-name");
  var CSRF = { "X-CopySync-CSRF": "1" };

  // ---- tiny helpers ----------------------------------------------------------
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
      else if (k === "onclick") e.addEventListener("click", attrs[k]);
      else if (k === "onsubmit") e.addEventListener("submit", attrs[k]);
      else if (k === "text") e.textContent = attrs[k];
      else e.setAttribute(k, attrs[k]);
    }
    for (var i = 2; i < arguments.length; i++) {
      var c = arguments[i];
      if (c == null) continue;
      e.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    }
    return e;
  }
  function clear(n) { while (n.firstChild) n.removeChild(n.firstChild); }
  function fmtBytes(n) {
    if (n >= 1 << 30) return (n / (1 << 30)).toFixed(1) + " GiB";
    if (n >= 1 << 20) return (n / (1 << 20)).toFixed(1) + " MiB";
    if (n >= 1 << 10) return (n / (1 << 10)).toFixed(1) + " KiB";
    return n + " B";
  }
  function fmtTime(s) { return s ? new Date(s).toLocaleString() : "—"; }

  // ---- router ----------------------------------------------------------------
  async function start() {
    try {
      var me = await api("GET", "/admin/me");
      serverNameEl.textContent = me.serverName ? "· " + me.serverName : "";
      logoutBtn.classList.remove("hidden");
      if (me.mustChangePw) renderChangePw(true);
      else renderDashboard(me);
    } catch (e) {
      logoutBtn.classList.add("hidden");
      renderLogin();
    }
  }

  logoutBtn.addEventListener("click", async function () {
    try { await api("POST", "/admin/logout"); } catch (e) {}
    start();
  });

  // ---- login -----------------------------------------------------------------
  function renderLogin() {
    clear(app);
    var user = el("input", { type: "text", value: "admin", autocomplete: "username" });
    var pass = el("input", { type: "password", autocomplete: "current-password" });
    var msg = el("p", { class: "msg" });
    var form = el("form", { class: "card", onsubmit: async function (ev) {
      ev.preventDefault();
      msg.className = "msg"; msg.textContent = "";
      try {
        await api("POST", "/admin/login", { username: user.value, password: pass.value });
        start();
      } catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
    }},
      el("h2", null, "Sign in"),
      el("label", null, "Username"), user,
      el("label", null, "Password"), pass,
      el("div", { class: "row", style: "margin-top:14px" }, el("button", { type: "submit" }, "Sign in")),
      msg
    );
    app.appendChild(form);
  }

  // ---- forced / manual password change --------------------------------------
  function renderChangePw(forced) {
    clear(app);
    var cur = el("input", { type: "password", autocomplete: "current-password" });
    var nw = el("input", { type: "password", autocomplete: "new-password" });
    var nw2 = el("input", { type: "password", autocomplete: "new-password" });
    var msg = el("p", { class: "msg" });
    var form = el("form", { class: "card", onsubmit: async function (ev) {
      ev.preventDefault();
      msg.className = "msg"; msg.textContent = "";
      if (nw.value !== nw2.value) { msg.className = "msg err"; msg.textContent = "Passwords do not match"; return; }
      try {
        await api("POST", "/admin/password", { current: cur.value, new: nw.value });
        start();
      } catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
    }},
      el("h2", null, forced ? "Set a new admin password" : "Change password"),
      forced ? el("p", { class: "help" }, "You must replace the default password before continuing.") : null,
      el("label", null, "Current password"), cur,
      el("label", null, "New password (min 8 chars)"), nw,
      el("label", null, "Confirm new password"), nw2,
      el("div", { class: "row", style: "margin-top:14px" }, el("button", { type: "submit" }, "Save password")),
      msg
    );
    app.appendChild(form);
  }

  // ---- dashboard -------------------------------------------------------------
  function renderDashboard(me) {
    clear(app);
    app.appendChild(devicesCard());
    app.appendChild(pairingCard());
    app.appendChild(settingsCard());
    app.appendChild(accountCard());
  }

  function devicesCard() {
    var body = el("div", null, el("p", { class: "muted" }, "Loading devices…"));
    var card = el("div", { class: "card" },
      el("div", { class: "row spread" }, el("h2", null, "Devices"),
        el("button", { class: "ghost", onclick: load }, "Refresh")),
      body);

    async function load() {
      clear(body);
      try {
        var data = await api("GET", "/admin/devices");
        var devs = (data && data.devices) || [];
        if (!devs.length) { body.appendChild(el("p", { class: "muted" }, "No paired devices yet. Use “Pair a device” below.")); return; }
        var rows = [el("tr", null,
          el("th", null, "Name"), el("th", null, "Platform"),
          el("th", null, "Status"), el("th", null, "Last seen"), el("th", null, ""))];
        devs.forEach(function (d) {
          rows.push(el("tr", null,
            el("td", null, el("strong", null, d.name || "(unnamed)"), el("div", { class: "muted small" }, d.id)),
            el("td", null, d.platform || "—"),
            el("td", null, el("span", { class: "badge " + (d.online ? "on" : "off") }, d.online ? "online" : "offline")),
            el("td", { class: "muted small" }, fmtTime(d.lastSeenAt)),
            el("td", null, el("button", { class: "danger", onclick: function () { revoke(d); } }, "Revoke"))));
        });
        var table = el("table", null);
        rows.forEach(function (r) { table.appendChild(r); });
        body.appendChild(table);
      } catch (e) { body.appendChild(el("p", { class: "msg err" }, e.message)); }
    }
    async function revoke(d) {
      if (!confirm("Revoke and unpair “" + (d.name || d.id) + "”? It will need to pair again.")) return;
      try { await api("DELETE", "/admin/devices/" + encodeURIComponent(d.id)); load(); }
      catch (e) { alert(e.message); }
    }
    load();
    return card;
  }

  function pairingCard() {
    var out = el("div", null);
    var card = el("div", { class: "card" },
      el("h2", null, "Pair a device"),
      el("p", { class: "help" }, "Generate a one-time code. On the client, scan the QR or enter the details manually. The code is single-use and expires soon."),
      el("div", { class: "row" }, el("button", { onclick: gen }, "Generate pairing code")),
      out);

    async function gen() {
      clear(out);
      out.appendChild(el("p", { class: "muted" }, "Generating…"));
      try {
        var r = await api("POST", "/admin/pairing");
        clear(out);
        var left = el("div", null,
          el("label", null, "One-time code"),
          el("div", { class: "otp" }, r.otp),
          el("p", { class: "help" }, "Expires " + fmtTime(r.expiresAt)),
          el("label", null, "Manual pairing payload"),
          el("pre", { class: "payload" }, JSON.stringify(r.payload, null, 2)),
          el("button", { class: "ghost", onclick: function () {
            navigator.clipboard && navigator.clipboard.writeText(JSON.stringify(r.payload));
          }}, "Copy payload"));
        var right = r.qr
          ? el("div", { class: "qr" }, el("img", { src: r.qr, alt: "pairing QR" }))
          : el("p", { class: "muted" }, "(QR unavailable)");
        out.appendChild(el("div", { class: "row", style: "align-items:flex-start;gap:24px" }, left, right));
      } catch (e) { clear(out); out.appendChild(el("p", { class: "msg err" }, e.message)); }
    }
    return card;
  }

  // settings field definitions: [key, label, type, help]
  var SETTINGS_FIELDS = [
    ["maxMessageBytes", "Max WS message (bytes)", "bytes", "Inline text larger than this must use the blob channel."],
    ["blobMaxBytes", "Max blob size (bytes)", "bytes", "Per-file upload cap on the blob channel."],
    ["blobStoreCapBytes", "Blob store cap (bytes)", "bytes", "Total on-disk blob budget before LRU eviction."],
    ["queueDepthPerDevice", "Offline queue depth", "int", "Max queued clips kept for an offline device."],
    ["queueItemTtlSeconds", "Queue item TTL (sec)", "int", "Queued clips older than this are dropped."],
    ["blobTtlSeconds", "Blob TTL (sec)", "int", "Unreferenced blobs are deleted after this."],
    ["sessionTtlSeconds", "Admin session TTL (sec)", "int", ""],
    ["pairingCodeTtlSeconds", "Pairing code TTL (sec)", "int", ""],
    ["e2eEnabled", "End-to-end encryption", "bool", "When on, the server never sees clipboard contents (Stage 3)."],
    ["allowServerBroadcast", "Allow server broadcast", "bool", "Forced off while E2E is enabled."]
  ];

  function settingsCard() {
    var body = el("div", null, el("p", { class: "muted" }, "Loading settings…"));
    var card = el("div", { class: "card" }, el("h2", null, "Settings"), body);
    var inputs = {};

    async function load() {
      clear(body);
      try {
        var s = await api("GET", "/admin/settings");
        var grid = el("div", { class: "grid2" });
        SETTINGS_FIELDS.forEach(function (f) {
          var key = f[0], label = f[1], type = f[2], help = f[3];
          if (type === "bool") {
            var cb = el("input", { type: "checkbox" });
            cb.checked = !!s[key];
            inputs[key] = { el: cb, type: type };
            var wrap = el("div", null, el("div", { class: "checkline" }, cb, el("span", null, label)));
            if (help) wrap.appendChild(el("div", { class: "help" }, help));
            grid.appendChild(wrap);
          } else {
            var inp = el("input", { type: "number", min: "0", value: String(s[key]) });
            inputs[key] = { el: inp, type: type };
            var w = el("div", null, el("label", null, label), inp);
            if (type === "bytes") w.appendChild(el("div", { class: "help" }, fmtBytes(s[key]) + (help ? " — " + help : "")));
            else if (help) w.appendChild(el("div", { class: "help" }, help));
            grid.appendChild(w);
          }
        });
        var msg = el("p", { class: "msg" });
        var save = el("button", { onclick: async function () {
          msg.className = "msg"; msg.textContent = "";
          var payload = {};
          for (var k in inputs) {
            payload[k] = inputs[k].type === "bool" ? inputs[k].el.checked : Number(inputs[k].el.value);
          }
          try { await api("PUT", "/admin/settings", payload); msg.className = "msg ok"; msg.textContent = "Saved."; load(); }
          catch (e) { msg.className = "msg err"; msg.textContent = e.message; }
        }}, "Save settings");
        body.appendChild(grid);
        body.appendChild(el("div", { class: "row", style: "margin-top:14px" }, save));
        body.appendChild(msg);
      } catch (e) { clear(body); body.appendChild(el("p", { class: "msg err" }, e.message)); }
    }
    load();
    return card;
  }

  function accountCard() {
    return el("div", { class: "card" },
      el("h2", null, "Admin account"),
      el("div", { class: "row" }, el("button", { class: "ghost", onclick: function () { renderChangePw(false); } }, "Change password")));
  }

  start();
})();
