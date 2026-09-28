// Wake: the phone page. No framework, no offline mode: every control call goes
// to the server, and the live status arrives over Server-Sent Events.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));
const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

const el = {
  body: document.body,
  clock: $("#clock"),
  tag: $("#status-tag"),
  tagText: $("#status-tag-text"),
  headline: $("#status-headline"),
  dek: $("#status-dek"),
  burst: $("#burst"),
  wire: $("#wire"),
  noticeSlot: $("#notice-slot"),
  connection: $("#connection"),
  slips: $$(".slip"),
  nextWord: $("#next-word"),
  nextNote: $("#next-note"),
  wakeBtn: $("#wake-btn"),
  wakeLabel: $("#wake-label"),
  lastBoot: $("#last-boot"),
  lastWake: $("#last-wake"),
  choiceExpiry: $("#choice-expiry"),
  announcer: $("#announcer"),
  sheet: $("#settings"),
  openSettings: $("#open-settings"),
  closeSettings: $("#close-settings"),
  access: $("#access"),
  unlockForm: $("#unlock-form"),
  tokenInput: $("#token-input"),
  tokenField: $("#token-field"),
  tokenError: $("#token-error"),
  unlocked: $("#unlocked"),
  lockBtn: $("#lock-btn"),
  choiceText: $("#choice-text"),
  resetBtn: $("#reset-btn"),
  events: $("#events"),
  about: $("#about"),
};

let S = null; // latest status from the server
let locked = el.body.hasAttribute("data-locked");
let busy = false;
let onlineAt = null; // when this page saw the PC come online
let notice = null; // { key, tone, title, text }

try {
  S = JSON.parse($("#initial").textContent);
} catch {
  S = null;
}

// ---------- formatting ----------

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "short" });
const dateFmt = new Intl.DateTimeFormat(undefined, { day: "numeric", month: "short" });
const clockFmt = new Intl.DateTimeFormat(undefined, { weekday: "short", day: "numeric", month: "short" });

function when(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  const now = new Date();
  const time = timeFmt.format(d);
  if (d.toDateString() === now.toDateString()) return time;
  if (now - d < 6 * 864e5) return `${dayFmt.format(d)} ${time}`;
  return `${dateFmt.format(d)}, ${time}`;
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 45) return "just now";
  if (s < 90) return "a minute ago";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} minutes ago`;
  return `at ${when(ms)}`;
}

function span(ms) {
  if (ms == null) return "No limit";
  const s = Math.round(ms / 1000);
  if (s < 90) return `${s} seconds`;
  const m = Math.round(s / 60);
  if (m < 90) return `${m} minutes`;
  const h = Math.round(m / 6) / 10;
  return `${h} hours`;
}

const osLabel = (os) => (os === "windows" ? "Windows" : "Linux");
const other = (os) => (os === "windows" ? "linux" : "windows");

function icon(name, size = 20, extraClass = "") {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", `bs-icon ${extraClass}`.trim());
  svg.setAttribute("width", size);
  svg.setAttribute("height", size);
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") node.className = v;
    else if (v !== undefined && v !== null) node.setAttribute(k, v);
  }
  node.append(...children.filter((c) => c !== null && c !== undefined && c !== false));
  return node;
}

function announce(text) {
  el.announcer.textContent = "";
  requestAnimationFrame(() => (el.announcer.textContent = text));
}

// ---------- server calls ----------

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(path, options = {}) {
  let res;
  try {
    res = await fetch(path, {
      cache: "no-store",
      credentials: "same-origin",
      headers: { Accept: "application/json", ...(options.body ? { "Content-Type": "application/json" } : {}) },
      ...options,
    });
  } catch {
    throw new ApiError(0, "Wake can't be reached. Check that you're on the home network.");
  }
  if (res.status === 204) return null;
  let body = null;
  try {
    body = await res.json();
  } catch {
    /* not JSON */
  }
  if (!res.ok) {
    if (res.status === 401 && path !== "/api/session") setLocked(true);
    throw new ApiError(res.status, body?.message || `Wake answered ${res.status}.`);
  }
  return body;
}

// ---------- rendering ----------

// The wake-progress list belongs to a wake that's happening now: while the PC
// is waking or booting, and for a few minutes after it answers. Once the PC is
// off (or the wake timed out), it's history, and the event log keeps it.
function wakeActive(s) {
  const wol = s.last_wol;
  if (!wol) return false;
  const status = s.pc.status;
  if (status === "waking" || status === "booting") return true;
  if (status === "online") return Date.now() - wol.at < s.wake_timeout_ms + 5 * 60e3;
  return false;
}

function grubAfterWake(s) {
  return s.last_boot && s.last_wol && s.last_boot.at >= s.last_wol.at - 1000 ? s.last_boot : null;
}

function headlineFor(s) {
  const name = s.pc.name;
  switch (s.pc.status) {
    case "waking":
      return `Waking ${name}…`;
    case "booting":
      return `${name} is starting up`;
    case "online":
      return wakeActive(s) ? `${name} is awake` : `${name} is on`;
    case "offline":
      return `${name} is off`;
    default:
      return name;
  }
}

function dekFor(s) {
  const name = s.pc.name;
  const grub = grubAfterWake(s);
  switch (s.pc.status) {
    case "waking":
      return Date.now() - (s.last_wol?.at || 0) < 4000
        ? "Sending the magic packet."
        : `Packet sent. Waiting for ${name} to start.`;
    case "booting":
      return grub
        ? `GRUB asked for its orders and was told ${osLabel(grub.os)}.`
        : "GRUB has asked for its orders.";
    case "online":
      if (grub) return `Started ${osLabel(grub.os)} at ${when(grub.at)}.`;
      return "Answering on the network.";
    case "offline":
      return s.pc.last_seen ? `Last seen ${ago(s.pc.last_seen)}.` : "Asleep or switched off.";
    default:
      return s.pc.probing
        ? "Checking…"
        : "Wake can't tell whether it's on. Set PC_IP so it can check.";
  }
}

const TAG_TONE = { online: "success", waking: "live", booting: "live", offline: "neutral", unknown: "neutral" };

function setText(node, text, animate) {
  if (node.textContent === text) return;
  node.textContent = text;
  if (animate && !reducedMotion.matches) {
    node.classList.remove("is-changing");
    void node.offsetWidth;
    node.classList.add("is-changing");
  }
}

function rollWord(os) {
  const text = osLabel(os);
  const current = el.nextWord.querySelector("span:not(.roll-out)");
  if (current && current.textContent === text) return;
  if (reducedMotion.matches || !current) {
    el.nextWord.replaceChildren(h("span", {}, text));
    return;
  }
  current.className = "roll-out";
  const next = h("span", { class: "roll-in" }, text);
  el.nextWord.append(next);
  setTimeout(() => current.remove(), 260);
}

function renderSlips(s) {
  const next = s.next_boot.os;
  for (const slip of el.slips) {
    const os = slip.dataset.os;
    const on = os === next;
    slip.setAttribute("aria-checked", String(on));
    slip.tabIndex = on ? 0 : -1;
    const note = slip.querySelector(".slip-note");
    note.textContent = on && s.next_boot.explicit ? "One time" : os === s.default_boot ? "Default" : "";
  }
}

function renderWakeButton(s) {
  const name = s.pc.name;
  const next = osLabel(s.next_boot.os);
  let label = `Wake ${name} into ${next}`;
  let primary = true;
  if (s.pc.status === "online") {
    label = "Send a magic packet anyway";
    primary = false;
  } else if (s.pc.status === "waking" || s.pc.status === "booting") {
    label = "Send another magic packet";
    primary = false;
  }
  if (!busy) el.wakeLabel.textContent = label;
  el.wakeBtn.classList.toggle("bs-btn-primary", primary);
  el.wakeBtn.classList.toggle("bs-btn-secondary", !primary);
}

function wireRow(state, text, at) {
  const lead =
    state === "done" ? icon("check", 18) : state === "failed" ? icon("alert", 18) : h("span", { class: "pending-dot", "aria-hidden": "true" });
  return h(
    "li",
    { class: state === "done" ? "" : state },
    lead,
    h("span", {}, text),
    at ? h("time", { datetime: new Date(at).toISOString() }, timeFmt.format(new Date(at))) : h("span"),
  );
}

function renderWire(s) {
  if (!wakeActive(s)) {
    el.wire.hidden = true;
    el.wire.replaceChildren();
    return;
  }
  const wol = s.last_wol;
  const grub = grubAfterWake(s);
  const rows = [];
  rows.push(wol.ok ? wireRow("done", "Magic packet sent", wol.at) : wireRow("failed", "Magic packet failed", wol.at));
  if (wol.ok) {
    if (grub) rows.push(wireRow("done", `GRUB asked, told it ${osLabel(grub.os)}`, grub.at));
    else if (s.pc.status !== "online") rows.push(wireRow("pending", "Waiting for GRUB to ask"));
    if (s.pc.status === "online") rows.push(wireRow("done", `${s.pc.name} answered`, onlineAt || s.pc.last_seen));
    else if (s.pc.probing && grub) rows.push(wireRow("pending", `Waiting for ${s.pc.name} to answer`));
  }
  // Re-render only when the rows change, so entry animations play once.
  const sig = rows.map((r) => r.textContent + r.className).join("|");
  if (el.wire.dataset.sig !== sig) {
    const old = el.wire.dataset.sig ? el.wire.dataset.sig.split("|") : [];
    el.wire.dataset.sig = sig;
    rows.forEach((r, i) => {
      if (old[i] === r.textContent + r.className) r.style.animation = "none";
    });
    el.wire.replaceChildren(...rows);
  }
  el.wire.hidden = false;
}

function renderNotice(s) {
  let n = notice;
  if (!n && s.last_wol && !s.last_wol.ok && Date.now() - s.last_wol.at < 10 * 60e3) {
    n = { key: `wol-${s.last_wol.at}`, tone: "live", title: "The magic packet didn't go out", text: s.last_wol.error || "" };
  }
  if (!n && !s.storage_ok) {
    n = {
      key: "storage",
      tone: "correction",
      title: "Wake can't save its state",
      text: "Choices still work, but they'll be forgotten if Wake restarts. Check the data volume's permissions.",
    };
  }
  const key = n ? n.key : "";
  if (el.noticeSlot.dataset.key === key) return;
  el.noticeSlot.dataset.key = key;
  if (!n) {
    el.noticeSlot.replaceChildren();
    return;
  }
  const iconName = { info: "info", success: "check", correction: "alert", live: "alert" }[n.tone];
  el.noticeSlot.replaceChildren(
    h(
      "div",
      { class: `bs-notice bs-notice-${n.tone}`, role: n.tone === "live" ? "alert" : "status" },
      icon(iconName, 20, "bs-notice-icon"),
      h("div", { class: "bs-notice-body" }, h("p", { class: "bs-notice-title" }, n.title), n.text ? h("p", { class: "bs-notice-text" }, n.text) : null),
    ),
  );
}

function renderLedger(s) {
  const b = s.last_boot;
  el.lastBoot.textContent = b ? `${osLabel(b.os)} · ${when(b.at)}` : "None yet";
  const w = s.last_wol;
  el.lastWake.textContent = w ? `${when(w.at)}${w.ok ? "" : " · failed"}` : "None yet";
  const nb = s.next_boot;
  if (!nb.explicit) el.choiceExpiry.textContent = "Default";
  else if (nb.expires_at) el.choiceExpiry.textContent = `${osLabel(nb.os)} until ${when(nb.expires_at)}`;
  else el.choiceExpiry.textContent = `${osLabel(nb.os)}, no expiry`;
}

function renderNextNote(s) {
  const nb = s.next_boot;
  const def = osLabel(s.default_boot);
  el.nextNote.textContent = nb.explicit
    ? nb.os === s.default_boot
      ? "Chosen for the next boot."
      : `One time only, then back to ${def}.`
    : `The default. Pick ${osLabel(other(s.default_boot))} for a one-time boot.`;
  el.choiceText.textContent = nb.explicit
    ? `${osLabel(nb.os)} is waiting for the next boot${nb.expires_at ? ` until ${when(nb.expires_at)}` : ""}.`
    : `Nothing is waiting, so GRUB gets the default: ${def}.`;
  el.resetBtn.disabled = !nb.explicit;
}

function renderEvents(s) {
  const items = (s.events || []).slice(0, 20).map((e) => {
    const name = { success: "check", warning: "alert", error: "alert", info: "info" }[e.kind] || "info";
    return h("li", { class: `ev-${e.kind}` }, icon(name, 18), h("span", {}, e.message), h("time", { datetime: new Date(e.at).toISOString() }, when(e.at)));
  });
  el.events.replaceChildren(...(items.length ? items : [h("li", { class: "events-empty" }, h("span"), h("span", {}, "Nothing yet."))]));
}

function renderAbout(s) {
  const st = s.setup;
  const rows = [
    ["Version", st.version],
    ["PC address", st.pc_ip || "Not set"],
    [st.pc_macs.length > 1 ? "PC MACs" : "PC MAC", st.pc_macs.join(", ")],
    ["Magic packet to", st.wol_target],
    ["Checks", st.probes.join(", ")],
    ["GRUB asks", `http://${location.hostname}:${st.grub_port}/grub/boot.env`],
    ["A choice lasts", span(st.choice_ttl_ms)],
  ];
  el.about.replaceChildren(...rows.map(([k, v]) => h("div", { class: "ledger-row" }, h("dt", {}, k), h("dd", {}, v))));
}

function renderAccess(s) {
  const required = locked || s?.auth_required;
  el.access.hidden = !required;
  el.unlockForm.hidden = !locked;
  el.unlocked.hidden = locked;
}

function renderLocked() {
  el.body.dataset.status = "unknown";
  el.tag.className = "bs-tag bs-tag-neutral";
  el.tagText.textContent = "Locked";
  setText(el.headline, "Wake is locked", false);
  el.dek.textContent = "Unlock it with the Wake token to see the PC and choose its next boot.";
  el.wakeLabel.textContent = "Unlock Wake";
  el.wakeBtn.classList.add("bs-btn-primary");
  el.wakeBtn.classList.remove("bs-btn-secondary");
  el.wire.hidden = true;
  renderAccess(null);
}

function render(prev) {
  if (locked || !S) {
    renderLocked();
    return;
  }
  const s = S;
  const status = s.pc.status;
  const prevStatus = prev?.pc.status;
  if (status === "online" && prevStatus && prevStatus !== "online") onlineAt = Date.now();
  if (status !== "online") onlineAt = null;

  el.body.dataset.status = status;
  el.body.dataset.next = s.next_boot.os;
  el.tag.className = `bs-tag bs-tag-${TAG_TONE[status] || "neutral"}`;
  el.tagText.textContent = s.pc.label;
  setText(el.headline, headlineFor(s), Boolean(prev));
  el.dek.textContent = dekFor(s);

  renderSlips(s);
  rollWord(s.next_boot.os);
  renderNextNote(s);
  renderWakeButton(s);
  renderWire(s);
  renderLedger(s);
  renderEvents(s);
  renderAbout(s);
  renderAccess(s);

  if ((prevStatus === "waking" || prevStatus === "booting") && status === "online") {
    celebrate(s);
  }
  if (prevStatus && prevStatus !== status) announce(`${headlineFor(s)}. ${dekFor(s)}`);
  renderNotice(s);
}

function update(next) {
  const prev = S;
  S = next;
  render(prev);
}

function celebrate(s) {
  const grub = grubAfterWake(s);
  notice = {
    key: `awake-${Date.now()}`,
    tone: "success",
    title: `${s.pc.name} is awake`,
    text: grub ? `It started ${osLabel(grub.os)}. Next time it goes back to ${osLabel(s.default_boot)}.` : "",
  };
  setTimeout(() => {
    if (notice && notice.key.startsWith("awake-")) {
      notice = null;
      if (S) renderNotice(S);
    }
  }, 60e3);
  if (reducedMotion.matches) return;
  const pieces = [];
  const n = 10;
  for (let i = 0; i < n; i++) {
    const angle = (i / n) * Math.PI * 2 + Math.random() * 0.4;
    const dist = 56 + Math.random() * 40;
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.style.setProperty("--dx", `${Math.cos(angle) * dist}px`);
    svg.style.setProperty("--dy", `${Math.sin(angle) * dist * 0.7}px`);
    svg.style.animationDelay = `${i * 18}ms`;
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", "#i-fleuron");
    svg.append(use);
    pieces.push(svg);
  }
  el.burst.replaceChildren(...pieces);
  setTimeout(() => el.burst.replaceChildren(), 1400);
}

// ---------- actions ----------

function setLocked(value) {
  locked = value;
  if (value) el.body.setAttribute("data-locked", "");
  else el.body.removeAttribute("data-locked");
  render(null);
}

function showError(err) {
  notice = { key: `err-${Date.now()}`, tone: "live", title: "That didn't work", text: err.message };
  if (S) renderNotice(S);
  else {
    el.noticeSlot.dataset.key = "";
    renderNotice({ storage_ok: true, last_wol: null });
  }
  announce(err.message);
  if (err.status === 401) openSettings();
}

function clearTransientNotice() {
  if (notice && !notice.key.startsWith("awake-")) notice = null;
}

async function choose(os) {
  if (locked) return openSettings();
  if (!S || busy) return;
  const slip = el.slips.find((s) => s.dataset.os === os);
  if (S.next_boot.os === os && S.next_boot.explicit) {
    pop(slip);
    return;
  }
  const before = S;
  // Show the choice straight away; the server confirms or we put it back.
  update({ ...S, next_boot: { ...S.next_boot, os, explicit: true } });
  pop(slip);
  slip.setAttribute("aria-busy", "true");
  try {
    clearTransientNotice();
    update(await api(`/api/boot/${os}`, { method: "POST" }));
    announce(`Next boot: ${osLabel(os)}.`);
  } catch (err) {
    update(before);
    showError(err);
  } finally {
    slip.removeAttribute("aria-busy");
  }
}

function pop(slip) {
  if (!slip || reducedMotion.matches) return;
  slip.classList.remove("just-picked");
  void slip.offsetWidth;
  slip.classList.add("just-picked");
  setTimeout(() => slip.classList.remove("just-picked"), 700);
}

async function wake() {
  if (locked) return openSettings();
  if (!S || busy) return;
  busy = true;
  el.wakeBtn.setAttribute("aria-busy", "true");
  el.wakeBtn.disabled = true;
  el.wakeLabel.textContent = "Sending…";
  clearTransientNotice();
  try {
    const res = await api("/api/wake", { method: "POST" });
    if (res.already_online) {
      notice = { key: `on-${Date.now()}`, tone: "info", title: `${res.status.pc.name} is already on`, text: `${osLabel(res.next_boot)} will be used the next time it restarts.` };
    }
    busy = false;
    update(res.status);
    announce(res.message);
    // "Sending the magic packet" gives way to "waiting" once the packets are out.
    setTimeout(() => S && !locked && (el.dek.textContent = dekFor(S)), 4200);
    if (!reducedMotion.matches) {
      el.wakeBtn.classList.remove("sent");
      void el.wakeBtn.offsetWidth;
      el.wakeBtn.classList.add("sent");
    }
  } catch (err) {
    busy = false;
    showError(err);
    if (S) renderWakeButton(S);
    // The server keeps a record of the failure; fetch it so the ledger updates.
    refresh();
  } finally {
    busy = false;
    el.wakeBtn.removeAttribute("aria-busy");
    el.wakeBtn.disabled = false;
    if (S && !locked) renderWakeButton(S);
  }
}

async function resetChoice() {
  try {
    update(await api("/api/boot/default", { method: "POST" }));
    announce("Next boot reset to the default.");
  } catch (err) {
    showError(err);
  }
}

async function refresh() {
  if (locked) return;
  try {
    update(await api("/api/status"));
  } catch {
    /* the connection indicator covers this */
  }
}

// ---------- live updates ----------

let source = null;
let reconnectTimer = null;

function connect() {
  if (locked) return;
  if (!("EventSource" in window)) {
    setInterval(refresh, 5000);
    return;
  }
  source?.close();
  source = new EventSource("/api/events");
  source.addEventListener("status", (e) => {
    clearTimeout(reconnectTimer);
    el.connection.hidden = true;
    try {
      update(JSON.parse(e.data));
    } catch {
      /* ignore a bad frame */
    }
  });
  source.addEventListener("error", () => {
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(() => {
      if (source.readyState !== EventSource.OPEN) {
        el.connection.hidden = false;
        // A token change shows up as a failing stream; a status call tells us.
        refresh();
      }
    }, 3000);
  });
}

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    refresh();
    if (!source || source.readyState === EventSource.CLOSED) connect();
  }
});

// ---------- settings sheet ----------

function openSettings() {
  if (!el.sheet.open) el.sheet.showModal();
  if (locked) setTimeout(() => el.tokenInput.focus(), 50);
}

el.openSettings.addEventListener("click", openSettings);
el.closeSettings.addEventListener("click", () => el.sheet.close());
el.sheet.addEventListener("click", (e) => {
  if (e.target === el.sheet) el.sheet.close();
});

el.unlockForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const token = el.tokenInput.value.trim();
  el.tokenError.hidden = true;
  el.tokenField.classList.remove("bs-field-invalid");
  if (!token) {
    el.tokenError.querySelector("span").textContent = "Enter the token first.";
    el.tokenError.hidden = false;
    return;
  }
  const btn = el.unlockForm.querySelector("button");
  btn.disabled = true;
  btn.textContent = "Unlocking…";
  try {
    await api("/api/session", { method: "POST", body: JSON.stringify({ token }) });
    location.reload();
  } catch (err) {
    el.tokenField.classList.add("bs-field-invalid");
    el.tokenError.querySelector("span").textContent = err.message;
    el.tokenError.hidden = false;
    btn.disabled = false;
    btn.textContent = "Unlock";
  }
});

el.lockBtn.addEventListener("click", async () => {
  await api("/api/session", { method: "DELETE" }).catch(() => {});
  location.reload();
});

el.resetBtn.addEventListener("click", resetChoice);

// ---------- boot choice: a radio group ----------

for (const slip of el.slips) {
  slip.addEventListener("click", () => choose(slip.dataset.os));
  slip.addEventListener("keydown", (e) => {
    const keys = { ArrowLeft: -1, ArrowUp: -1, ArrowRight: 1, ArrowDown: 1 };
    if (!(e.key in keys)) return;
    e.preventDefault();
    const i = el.slips.indexOf(slip);
    const next = el.slips[(i + keys[e.key] + el.slips.length) % el.slips.length];
    next.focus();
    choose(next.dataset.os);
  });
}

el.wakeBtn.addEventListener("click", wake);

// ---------- clock ----------

function tick() {
  const now = new Date();
  el.clock.dateTime = now.toISOString();
  el.clock.textContent = `${clockFmt.format(now)} · ${timeFmt.format(now)}`;
  // Relative times ("a minute ago") drift, so redraw the words too.
  if (S && !locked) {
    el.dek.textContent = dekFor(S);
    renderLedger(S);
  }
}

tick();
setInterval(tick, 30e3);
render(null);
connect();
