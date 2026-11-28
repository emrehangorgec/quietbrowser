"use strict";

const byId = (id) => document.getElementById(id);
const tabs = byId("tabs");
const control = byId("control");
const page = byId("page");
const error = byId("error");
const previewMessage = byId("preview-message");

let instance = null;
let active = null;
let currentTab = null;
let listKey = "";
let previewPending = false;
let changingControl = false;
let inputs = Promise.resolve();
let scrollTimer = null;
let scrollDelta = null;

async function api(path, body) {
  const headers = { "X-QuietBrowser-UI": "1" };
  const options = { headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    options.method = "POST";
    options.body = JSON.stringify(body);
  }
  const response = await fetch("/ui/" + path, options);
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}

function showError(message) {
  error.textContent = message ?? "";
  error.hidden = !message;
}

function statusText(status) {
  return {
    working: "Working",
    waiting: "Waiting for you",
    controlled: "You have control",
    ready: "Ready"
  }[status];
}

function clearPreview() {
  page.hidden = true;
  page.removeAttribute("src");
  previewMessage.hidden = false;
  previewMessage.textContent = "Loading preview…";
  showError("");
}

function selectTab(id) {
  if (active === id) return;
  active = id;
  currentTab = null;
  listKey = "";
  clearPreview();
  void refreshState();
}

function renderTabs(entries) {
  const key = JSON.stringify([active, entries.map(({ id, agent, title, status }) => [id, agent, title, status])]);
  if (key === listKey) return;
  listKey = key;
  const focused = tabs.contains(document.activeElement) ? document.activeElement.dataset.tab : null;
  const color = (name) => Array.from(name).reduce((sum, char) => sum + char.codePointAt(0), 0) % 4;
  tabs.replaceChildren(...entries.map((tab) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "tab";
    button.dataset.tab = tab.id;
    button.setAttribute("aria-pressed", String(tab.id === active));
    const icon = document.createElement("span");
    icon.className = "agent-icon";
    icon.setAttribute("aria-hidden", "true");
    icon.dataset.color = color(tab.agent);
    icon.textContent = tab.agent[0]?.toUpperCase() || "?";
    const labels = document.createElement("span");
    const name = document.createElement("span");
    name.className = "tab-name";
    name.textContent = tab.agent;
    const caption = document.createElement("span");
    caption.className = "tab-caption";
    caption.textContent = tab.status === "ready" ? (tab.title || "Untitled tab") : statusText(tab.status);
    labels.append(name, caption);
    button.append(icon, labels);
    button.addEventListener("click", () => selectTab(tab.id));
    return button;
  }));
  if (focused) Array.from(tabs.children).find((button) => button.dataset.tab === focused)?.focus();
}

function renderDetails() {
  byId("empty").hidden = !!currentTab;
  byId("details").hidden = !currentTab;
  if (!currentTab) return;
  const tab = currentTab;
  byId("agent").textContent = tab.agent;
  byId("title").textContent = tab.title || "Untitled tab";
  byId("url").textContent = tab.url || "";
  const activity = byId("activity");
  activity.dataset.status = tab.status;
  activity.textContent = [statusText(tab.status), tab.action && (tab.failed ? tab.action + " failed" : tab.action)].filter(Boolean).join(" · ");
  const controlled = tab.status === "controlled";
  control.textContent = controlled ? (tab.detached ? "Close tab" : "Return control") : "Take control";
  control.disabled = changingControl;
  page.tabIndex = controlled ? 0 : -1;
  byId("hint").textContent = controlled
    ? "Click the preview to type, paste or scroll. Shift + Esc leaves the preview."
    : "Live preview. Take control to click, type or scroll.";
}

let statePending = false;
let stateAgain = false;
async function refreshState() {
  if (statePending) {
    stateAgain = true;
    return;
  }
  statePending = true;
  try {
    const state = await api("state");
    if (instance !== state.instance) {
      instance = state.instance;
      active = null;
      clearPreview();
    }
    if (!state.tabs.some((tab) => tab.id === active)) {
      active = state.tabs[0]?.id ?? null;
      clearPreview();
    }
    currentTab = state.tabs.find((tab) => tab.id === active) ?? null;
    byId("tab-count").textContent = state.tabs.length;
    byId("connection").textContent = "Local browser connected";
    byId("connection").classList.add("online");
    renderTabs(state.tabs);
    renderDetails();
  } catch {
    currentTab = null;
    active = null;
    instance = null;
    clearPreview();
    renderTabs([]);
    renderDetails();
    byId("tab-count").textContent = "0";
    byId("connection").textContent = "Reconnecting…";
    byId("connection").classList.remove("online");
  } finally {
    statePending = false;
    if (stateAgain) {
      stateAgain = false;
      void refreshState();
    }
  }
}

function action(tab, generation, name, props = {}) {
  return api("action", { instance: generation, tab, action: name, ...props });
}

function selected(tab, generation) {
  return tab === active && generation === instance;
}

async function refreshPreview() {
  if (previewPending || !currentTab || currentTab.status === "working" || document.hidden) return;
  previewPending = true;
  const tab = active, generation = instance;
  try {
    const { image } = await action(tab, generation, "preview");
    if (!selected(tab, generation)) return;
    const src = "data:image/jpeg;base64," + image;
    if (page.getAttribute("src") !== src) page.src = src;
    page.hidden = false;
    previewMessage.hidden = true;
  } catch (err) {
    if (selected(tab, generation) && page.hidden) previewMessage.textContent = err.message;
  } finally {
    previewPending = false;
  }
}

control.addEventListener("click", async () => {
  if (!currentTab || changingControl) return;
  changingControl = true;
  renderDetails();
  showError("");
  const tab = active, generation = instance;
  const take = currentTab.status !== "controlled";
  try {
    await inputs;
    await action(tab, generation, take ? "take" : "return");
    if (selected(tab, generation) && currentTab) {
      currentTab.status = take ? "controlled" : "ready";
      renderDetails();
      if (take && !page.hidden) page.focus();
    }
  } catch (err) {
    if (selected(tab, generation)) showError(err.message);
  } finally {
    changingControl = false;
    renderDetails();
    void refreshState();
    void refreshPreview();
  }
});

function queueInput(name, props) {
  if (currentTab?.status !== "controlled" || changingControl) return;
  const tab = active, generation = instance;
  inputs = inputs.then(async () => {
    if (!selected(tab, generation)) return;
    try {
      await action(tab, generation, name, props);
      showError("");
    } catch (err) {
      if (selected(tab, generation)) showError(err.message);
    }
  });
}

function point(event) {
  const rect = page.getBoundingClientRect();
  const x = (event.clientX - rect.left) / rect.width;
  const y = (event.clientY - rect.top) / rect.height;
  return x >= 0 && y >= 0 && x < 1 && y < 1 ? { x, y } : null;
}

page.addEventListener("click", (event) => {
  if (currentTab?.status !== "controlled") return;
  page.focus();
  const at = point(event);
  if (at) queueInput("click", at);
});

page.addEventListener("wheel", (event) => {
  if (currentTab?.status !== "controlled" || changingControl) return;
  const at = point(event);
  if (!at) return;
  event.preventDefault();
  const scale = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? page.clientHeight : 1;
  if (!scrollDelta || !selected(scrollDelta.tab, scrollDelta.generation)) {
    scrollDelta = { ...at, dx: 0, dy: 0, tab: active, generation: instance };
  }
  scrollDelta.dx += event.deltaX * scale;
  scrollDelta.dy += event.deltaY * scale;
  if (scrollTimer === null) {
    scrollTimer = setTimeout(() => {
      const { tab, generation, ...delta } = scrollDelta;
      scrollDelta = scrollTimer = null;
      if (selected(tab, generation)) queueInput("scroll", delta);
    }, 80);
  }
}, { passive: false });

const keys = new Set(["Backspace", "Tab", "Enter", "Escape", "Delete", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End", "PageUp", "PageDown"]);
page.addEventListener("keydown", (event) => {
  if (currentTab?.status !== "controlled" || changingControl || event.isComposing) return;
  if (event.key === "Escape" && event.shiftKey) {
    event.preventDefault();
    control.focus();
  } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") {
    event.preventDefault();
    queueInput("key", { key: "a", meta: event.metaKey });
  } else if (!event.ctrlKey && !event.metaKey && !event.altKey) {
    if (keys.has(event.key)) {
      event.preventDefault();
      queueInput("key", { key: event.key, shift: event.shiftKey });
    } else if (Array.from(event.key).length === 1) {
      event.preventDefault();
      queueInput("text", { text: event.key });
    }
  }
});

page.addEventListener("paste", (event) => {
  if (currentTab?.status !== "controlled" || changingControl) return;
  event.preventDefault();
  const text = event.clipboardData?.getData("text/plain");
  if (text) queueInput("text", { text });
});

void refreshState().then(refreshPreview);
setInterval(() => { if (!document.hidden) void refreshState(); }, 650);
setInterval(refreshPreview, 950);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) void refreshState().then(refreshPreview);
});
