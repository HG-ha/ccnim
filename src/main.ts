import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import "./style.css";

type NimApiKey = {
  id: string;
  value: string;
  label?: string | null;
  expires_at?: number | null;
};

type AppConfig = {
  host: string;
  port: number;
  auth_token: string;
  nim_api_keys: NimApiKey[];
  model_mapping: {
    default_model: string;
    opus_model?: string | null;
    sonnet_model?: string | null;
    haiku_model?: string | null;
  };
  rate_limit_per_key: number;
  rate_window_secs: number;
  enable_thinking: boolean;
};

type KeySnapshot = {
  id: number;
  masked: string;
  label?: string | null;
  expires_at?: number | null;
  state: string;
  inflight: number;
  recent_requests: number;
  failure_count: number;
};

type ProxyStatus = {
  running: boolean;
  listen_url: string;
  default_model: string;
  keys: KeySnapshot[];
};

type IdeProfile = {
  id: string;
  name: string;
  settings_path: string;
  exists: boolean;
  configured_base_url: string | null;
  configured_auth_token: string | null;
};

type IdeApplyReport = {
  id: string;
  name: string;
  settings_path: string;
  backup_path: string | null;
  comments_stripped: boolean;
  created: boolean;
};

const app = document.querySelector<HTMLDivElement>("#app")!;
const toastEl = document.querySelector<HTMLDivElement>("#toast")!;
const tbStatus = document.querySelector<HTMLDivElement>("#tbStatus")!;
const tbStatusText = tbStatus.querySelector<HTMLSpanElement>(".tb-status-text")!;
const tbStatusUrl = tbStatus.querySelector<HTMLSpanElement>(".tb-status-url")!;

let config: AppConfig | null = null;
let proxyStatus: ProxyStatus | null = null;
let models: string[] = [];
let activeView = "dashboard";
let toastTimer: number | undefined;
let statusTimer: number | undefined;
let modelsTimer: number | undefined;
let showToken = false;
let diagnosticLog = "";
let ideProfiles: IdeProfile[] = [];
let idesScanning = false;
let idesScanError: string | null = null;
let ideApplying: string | null = null;
let sidebarCollapsed = (() => {
  try {
    return window.localStorage.getItem("fcc.sidebarCollapsed") === "1";
  } catch {
    return false;
  }
})();

/// How often the app re-pulls the upstream NVIDIA NIM model catalog. Models
/// turn over slowly enough that minute-level freshness is unnecessary; the
/// half-hour cadence keeps network noise tiny while still catching newly
/// published variants within one editing session.
const MODELS_REFRESH_MS = 30 * 60 * 1000;

type AddPanelMode = "single" | "batch" | null;
let addPanel: AddPanelMode = null;
let editingKeyId: string | null = null;
const singleAdd = { value: "", label: "", expiresAt: "" };
const batchAdd = { values: "", labelPrefix: "", expiresAt: "" };
const editForm = { value: "", label: "", expiresAt: "" };

const appWindow = getCurrentWindow();

/// Updater state machine. The update plugin returns an `Update` handle
/// once a newer release is detected; we keep that handle around so the
/// "立即安装" button in the dashboard banner can act on it without a
/// second roundtrip to the GitHub endpoint.
type UpdateStage = "idle" | "checking" | "available" | "downloading" | "ready" | "error";
type UpdateState = {
  stage: UpdateStage;
  currentVersion: string;
  /// Available update handle (only present when stage is "available" /
  /// "downloading" / "ready"). Not user-renderable directly.
  pending: Update | null;
  latestVersion: string | null;
  notes: string | null;
  /// Bytes downloaded so far during an in-progress install.
  downloaded: number;
  /// Total size as advertised by the update plugin's `Started` event.
  /// Some servers don't emit a content-length, in which case we just
  /// show a spinner instead of a precise progress bar.
  total: number | null;
  error: string | null;
};

const updateState: UpdateState = {
  stage: "idle",
  currentVersion: "",
  pending: null,
  latestVersion: null,
  notes: null,
  downloaded: 0,
  total: null,
  error: null,
};

/// Whether the "no update available" dialog should be shown after a
/// `checkForUpdate` call. Auto-checks (on app start, and the once-a-day
/// polling) keep this `false` so the user isn't pestered. Manual checks
/// via the "检查更新" button set it to `true` so we always confirm the
/// result, even when up-to-date.
let updateInteractive = false;

let updateModalOpen = false;

async function loadAppVersion() {
  try {
    updateState.currentVersion = await invoke<string>("app_version");
  } catch {
    updateState.currentVersion = "";
  }
}

/// Run a non-blocking update check. Pulls the latest manifest from the
/// configured GitHub Releases endpoint, then either:
///  - flips `stage` to "available" so the dashboard renders the banner,
///  - leaves `stage` at "idle" if no update is published,
///  - flips `stage` to "error" and surfaces a toast on the manual path.
async function checkForUpdate(interactive: boolean) {
  if (
    updateState.stage === "checking" ||
    updateState.stage === "downloading"
  ) {
    return;
  }
  updateInteractive = interactive;
  updateState.stage = "checking";
  updateState.error = null;
  if (interactive) {
    updateModalOpen = true;
    render();
  }
  try {
    const update = await check();
    if (update) {
      updateState.pending = update;
      updateState.latestVersion = update.version;
      updateState.notes = update.body ?? null;
      updateState.stage = "available";
    } else {
      updateState.pending = null;
      updateState.latestVersion = null;
      updateState.notes = null;
      updateState.stage = "idle";
      if (interactive) {
        toast(`已经是最新版本 (v${updateState.currentVersion})`, "success");
      }
    }
  } catch (error) {
    updateState.stage = "error";
    updateState.error = String(error);
    if (interactive) {
      toast(`检查更新失败: ${error}`, "error");
    }
  } finally {
    if (interactive) {
      updateModalOpen = updateState.stage === "available";
    }
    render();
  }
}

async function startUpdateInstall() {
  const update = updateState.pending;
  if (!update) return;
  updateState.stage = "downloading";
  updateState.downloaded = 0;
  updateState.total = null;
  updateState.error = null;
  updateModalOpen = true;
  render();
  try {
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          updateState.total = event.data.contentLength ?? null;
          updateState.downloaded = 0;
          break;
        case "Progress":
          updateState.downloaded += event.data.chunkLength;
          break;
        case "Finished":
          // Move to "ready" so the modal can prompt the user to relaunch.
          // We deliberately don't call `relaunch()` automatically — the
          // user might be mid-stream against a Claude Code session and
          // we want them to click through.
          updateState.stage = "ready";
          break;
      }
      render();
    });
    // Some upstreams skip the `Finished` event and just resolve the
    // promise, so make sure we always end up in "ready" if no error
    // was thrown.
    updateState.stage = "ready";
  } catch (error) {
    updateState.stage = "error";
    updateState.error = String(error);
    toast(`下载/安装失败: ${error}`, "error");
  }
  render();
}

async function relaunchApp() {
  try {
    await relaunch();
  } catch (error) {
    toast(`重启失败: ${error}`, "error");
  }
}

function dismissUpdateModal() {
  updateModalOpen = false;
  // Don't clear `pending` — keep the banner around so the user can come
  // back later. Only clear it after a successful install + relaunch
  // (which kills the process anyway) or an explicit "稍后再说" on the
  // banner.
  render();
}

function dismissUpdateBanner() {
  updateState.pending = null;
  updateState.latestVersion = null;
  updateState.notes = null;
  updateState.stage = "idle";
  render();
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function setupTitlebar() {
  document.getElementById("winMin")?.addEventListener("click", () => appWindow.minimize());
  document.getElementById("winMax")?.addEventListener("click", () => appWindow.toggleMaximize());
  document.getElementById("winClose")?.addEventListener("click", () => appWindow.close());
}

function toggleSidebar() {
  sidebarCollapsed = !sidebarCollapsed;
  try {
    window.localStorage.setItem("fcc.sidebarCollapsed", sidebarCollapsed ? "1" : "0");
  } catch {
    // localStorage may be unavailable inside the WebView (private mode etc.);
    // sidebar state is purely cosmetic, so silently fall back to in-memory.
  }
  render();
}

function syncTitlebarStatus() {
  const running = proxyStatus?.running ?? false;
  const url = proxyStatus?.listen_url ?? "";
  tbStatus.classList.toggle("online", running);
  tbStatus.classList.toggle("offline", !running);
  tbStatus.classList.toggle("has-url", running && !!url);
  tbStatusText.textContent = running ? "运行中" : "未启动";
  tbStatusUrl.textContent = running && url ? url : "";
}

async function load() {
  setupTitlebar();
  try {
    config = await invoke<AppConfig>("load_config");
    await loadAppVersion();
    await refreshStatus();
    syncTitlebarStatus();
    render();
    if (statusTimer) window.clearInterval(statusTimer);
    statusTimer = window.setInterval(async () => {
      await refreshStatus();
      syncTitlebarStatus();
      updateRuntimeUI();
    }, 3000);
    // Pull the model catalog once on startup, then every 30 min. When the
    // user has no keys yet, fetchModelsAuto returns immediately; we instead
    // rely on the per-key-add trigger in submitSingleAdd / submitBatchImport
    // to populate the catalog as soon as the first key is configured.
    void fetchModelsAuto();
    scheduleModelsRefresh();
    // Silent update probe a few seconds after launch so we don't race
    // the rest of the bootstrap. Re-check once a day for users who
    // leave the GUI open across multiple work sessions.
    window.setTimeout(() => {
      void checkForUpdate(false);
    }, 4000);
    window.setInterval(() => {
      void checkForUpdate(false);
    }, 24 * 60 * 60 * 1000);
  } catch (error) {
    app.innerHTML = `<div class="loading error">加载失败: ${escapeHtml(String(error))}</div>`;
  }
}

async function refreshStatus() {
  try {
    proxyStatus = await invoke<ProxyStatus>("proxy_status");
  } catch {
    proxyStatus = null;
  }
}

async function save(silent = false): Promise<boolean> {
  if (!config) return false;
  try {
    await invoke("save_config", { config });
    if (!silent) toast("配置已保存", "success");
    return true;
  } catch (error) {
    toast(`保存失败: ${error}`, "error");
    return false;
  }
}

async function startProxy() {
  await save(true);
  try {
    await invoke("start_proxy");
    toast("代理已启动", "success");
  } catch (error) {
    toast(`启动失败: ${error}`, "error");
  }
  await refreshStatus();
  syncTitlebarStatus();
  render();
}

async function stopProxy() {
  try {
    await invoke("stop_proxy");
    toast("代理已停止", "info");
  } catch (error) {
    toast(`停止失败: ${error}`, "error");
  }
  await refreshStatus();
  syncTitlebarStatus();
  render();
}

/// Silent background refresh of the upstream model list. Skips the call if no
/// API key is configured (the proxy would just bounce it). Only re-renders
/// when the list actually changed AND the user is currently looking at the
/// models page, so periodic ticks don't disturb other workflows.
async function fetchModelsAuto() {
  if (!config || config.nim_api_keys.length === 0) return;
  try {
    const response = await invoke<{ data: Array<{ id: string }> }>("fetch_nim_models");
    const next = response.data.map((m) => m.id).sort();
    const changed =
      next.length !== models.length || next.some((m, i) => m !== models[i]);
    if (!changed) return;
    models = next;
    if (activeView === "models") render();
  } catch {
    // Best-effort: a missing/invalid key, an offline network, or upstream
    // 5xx all hit this branch. Surfacing a toast every 30 minutes would be
    // worse than just waiting for the next tick.
  }
}

function scheduleModelsRefresh() {
  if (modelsTimer) window.clearInterval(modelsTimer);
  modelsTimer = window.setInterval(() => {
    void fetchModelsAuto();
  }, MODELS_REFRESH_MS);
}

async function openClaudeTerminal() {
  await save(true);
  try {
    await invoke("open_claude_terminal", { cwd: "" });
    toast("已尝试打开 Claude Code 终端", "success");
  } catch (error) {
    toast(`打开失败: ${error}`, "error");
  }
}

async function openClaudeDesktopApp() {
 await save(true);
 try {
 await invoke("open_claude_desktop");
 toast("已尝试打开 Claude Desktop", "success");
 } catch (error) {
 toast(`打开失败：${error}`, "error");
 }
}

async function refreshDiagnostic() {
  try {
    diagnosticLog = await invoke<string>("read_diagnostic_log");
    render();
  } catch (error) {
    toast(`读取诊断日志失败: ${error}`, "error");
  }
}

/// Refresh the list of detected IDEs. Pulls every known VSCode-family
/// vendor directory (`Code`, `Cursor`, `Windsurf`, ...) and reports back
/// which `settings.json` files exist + what they currently set under
/// `claudeCode.environmentVariables`. We keep this lazy — only fired when
/// the IDE page is visible — so users who never click "IDE 接入" pay no
/// startup cost.
async function refreshIdeProfiles() {
  if (idesScanning) return;
  idesScanning = true;
  idesScanError = null;
  try {
    ideProfiles = await invoke<IdeProfile[]>("scan_ides");
  } catch (error) {
    idesScanError = String(error);
    ideProfiles = [];
  } finally {
    idesScanning = false;
    if (activeView === "ide") render();
  }
}

async function applyIdeSettings(ideId: string) {
  if (!config) return;
  ideApplying = ideId;
  if (activeView === "ide") render();
  try {
    const report = await invoke<IdeApplyReport>("apply_ide_settings", { ideId });
    const parts = [`已写入 ${report.name}`];
    if (report.created) parts.push("（新建文件）");
    if (report.backup_path) parts.push(`已备份至 ${report.backup_path}`);
    if (report.comments_stripped) parts.push("注意：原文件含注释，已转为标准 JSON");
    toast(parts.join(" · "), "success");
  } catch (error) {
    toast(`写入失败: ${error}`, "error");
  } finally {
    ideApplying = null;
    await refreshIdeProfiles();
  }
}

function openAddPanel(mode: AddPanelMode) {
  addPanel = mode;
  if (mode === "single") {
    singleAdd.value = "";
    singleAdd.label = "";
    singleAdd.expiresAt = "";
  } else if (mode === "batch") {
    batchAdd.values = "";
    batchAdd.labelPrefix = "";
    batchAdd.expiresAt = "";
  }
  editingKeyId = null;
  render();
}

function closeAddPanel() {
  addPanel = null;
  render();
}

async function submitSingleAdd() {
  if (!config) return;
  const value = singleAdd.value.trim();
  if (!isValidNvapi(value)) {
    toast("Key 必须以 nvapi- 开头并且是合法的 NVIDIA NIM API Key", "error");
    return;
  }
  if (config.nim_api_keys.some((k) => k.value === value)) {
    toast("这个 Key 已经存在了", "error");
    return;
  }
  const newKey: NimApiKey = {
    id: newId(),
    value,
    label: singleAdd.label.trim() || null,
    expires_at: datetimeLocalToUnix(singleAdd.expiresAt),
  };
  config.nim_api_keys.push(newKey);
  if (!(await save(true))) {
    // Persistence failed; roll back so the in-memory state matches disk.
    const idx = config.nim_api_keys.findIndex((k) => k.id === newKey.id);
    if (idx >= 0) config.nim_api_keys.splice(idx, 1);
    return;
  }
  toast("已添加 1 个 Key", "success");
  addPanel = null;
  render();
  // Newly available key: kick off an immediate model-list pull so the
  // models page populates without waiting for the next 30-min tick.
  void fetchModelsAuto();
}

async function submitBatchImport() {
  if (!config) return;
  const candidates = batchAdd.values
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"));
  if (candidates.length === 0) {
    toast("请粘贴至少一行 nvapi- 开头的 Key", "error");
    return;
  }
  const invalid = candidates.filter((c) => !isValidNvapi(c));
  if (invalid.length > 0) {
    toast(`有 ${invalid.length} 行不是合法的 nvapi- Key，已中止导入`, "error");
    return;
  }
  const existing = new Set(config.nim_api_keys.map((k) => k.value));
  const sharedExpiry = datetimeLocalToUnix(batchAdd.expiresAt);
  const sharedLabel = batchAdd.labelPrefix.trim() || null;
  const newKeys: NimApiKey[] = [];
  let skipped = 0;
  for (const raw of candidates) {
    if (existing.has(raw)) {
      skipped += 1;
      continue;
    }
    existing.add(raw);
    const k: NimApiKey = {
      id: newId(),
      value: raw,
      label: sharedLabel,
      expires_at: sharedExpiry,
    };
    config.nim_api_keys.push(k);
    newKeys.push(k);
  }
  if (newKeys.length === 0) {
    toast(`全部 ${skipped} 个 Key 都已存在，无需导入`, "info");
    return;
  }
  if (!(await save(true))) {
    const ids = new Set(newKeys.map((k) => k.id));
    config.nim_api_keys = config.nim_api_keys.filter((k) => !ids.has(k.id));
    return;
  }
  toast(
    `导入完成：新增 ${newKeys.length} 个${skipped > 0 ? `，跳过重复 ${skipped} 个` : ""}`,
    "success",
  );
  addPanel = null;
  render();
  void fetchModelsAuto();
}

function beginEditKey(id: string) {
  if (!config) return;
  const key = config.nim_api_keys.find((k) => k.id === id);
  if (!key) return;
  editForm.value = key.value;
  editForm.label = key.label ?? "";
  editForm.expiresAt = unixToDatetimeLocal(key.expires_at);
  editingKeyId = id;
  addPanel = null;
  render();
}

function cancelEdit() {
  editingKeyId = null;
  render();
}

async function submitEditKey(id: string) {
  if (!config) return;
  const idx = config.nim_api_keys.findIndex((k) => k.id === id);
  if (idx < 0) return;
  const value = editForm.value.trim();
  if (!isValidNvapi(value)) {
    toast("Key 必须以 nvapi- 开头并且是合法的 NVIDIA NIM API Key", "error");
    return;
  }
  if (config.nim_api_keys.some((k, i) => i !== idx && k.value === value)) {
    toast("已经存在另一个相同值的 Key", "error");
    return;
  }
  const before = config.nim_api_keys[idx];
  config.nim_api_keys[idx] = {
    ...before,
    value,
    label: editForm.label.trim() || null,
    expires_at: datetimeLocalToUnix(editForm.expiresAt),
  };
  if (!(await save(true))) {
    config.nim_api_keys[idx] = before;
    return;
  }
  toast("已更新该 Key", "success");
  editingKeyId = null;
  render();
}

async function deleteKey(id: string) {
  if (!config) return;
  const idx = config.nim_api_keys.findIndex((k) => k.id === id);
  if (idx < 0) return;
  const k = config.nim_api_keys[idx];
  const label = k.label ? ` (${k.label})` : "";
  if (!window.confirm(`确认删除 ${maskKey(k.value)}${label} 吗？`)) return;
  const removed = config.nim_api_keys.splice(idx, 1)[0];
  if (!(await save(true))) {
    config.nim_api_keys.splice(idx, 0, removed);
    return;
  }
  toast("已删除", "info");
  if (editingKeyId === id) editingKeyId = null;
  render();
}

async function copy(text: string, kind = "已复制"): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    toast(`${kind}: ${text.length > 48 ? text.slice(0, 48) + "..." : text}`, "success");
  } catch (error) {
    toast(`复制失败: ${error}`, "error");
  }
}

function setConfig(path: string, value: string | number | boolean | string[] | null) {
  if (!config) return;
  const parts = path.split(".");
  let target: Record<string, unknown> = config as unknown as Record<string, unknown>;
  while (parts.length > 1) {
    target = target[parts.shift()!] as Record<string, unknown>;
  }
  target[parts[0]] = value;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function toast(message: string, kind: "success" | "error" | "info" = "info") {
  toastEl.className = `toast ${kind} show`;
  toastEl.textContent = message;
  if (toastTimer) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastEl.classList.remove("show");
  }, 3000);
}

const ICONS = {
  dashboard:
    '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="9"/><rect x="14" y="3" width="7" height="5"/><rect x="14" y="12" width="7" height="9"/><rect x="3" y="16" width="7" height="5"/></svg>',
  proxy:
    '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12V7a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v5"/><path d="M3 12v5a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-5"/><path d="M3 12h18"/><circle cx="7" cy="8.5" r="0.6" fill="currentColor"/><circle cx="7" cy="15.5" r="0.6" fill="currentColor"/></svg>',
  keys: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="14" r="4"/><path d="M11 11l9-9 3 3-3 3 2 2-3 3-2-2-3 3"/></svg>',
  models:
    '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 1v4M12 19v4M4.22 4.22l2.83 2.83M16.95 16.95l2.83 2.83M1 12h4M19 12h4M4.22 19.78l2.83-2.83M16.95 7.05l2.83-2.83"/></svg>',
  ide: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>',
  copy: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>',
  eye: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>',
  eyeOff:
    '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94"/><path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19"/><line x1="1" y1="1" x2="23" y2="23"/></svg>',
  info: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>',
  bolt: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/></svg>',
  shield:
    '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/></svg>',
  chevronLeft:
    '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 18 9 12 15 6"/></svg>',
};

function normalizeState(state: string): string {
  return state
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/[\s-]+/g, "_")
    .toLowerCase();
}

function keyStateBadge(state: string): string {
  const map: Record<string, { label: string; cls: string }> = {
    healthy: { label: "健康", cls: "badge-ok" },
    cooling_down: { label: "冷却中", cls: "badge-warn" },
    rate_limited: { label: "限流", cls: "badge-warn" },
    disabled: { label: "已禁用", cls: "badge-error" },
    exhausted: { label: "已耗尽", cls: "badge-error" },
    expired: { label: "已过期", cls: "badge-error" },
  };
  const norm = normalizeState(state);
  const item = map[norm] ?? { label: state, cls: "badge-neutral" };
  return `<span class="badge ${item.cls}">${item.label}</span>`;
}

function newId(): string {
  // crypto.randomUUID is available in modern WebView2/WKWebView; fall back
  // to a math-based id for older runtimes so we never silently emit "".
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  return `k-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function maskKey(value: string): string {
  if (value.length <= 10) return "********";
  return `${value.slice(0, 6)}...${value.slice(-4)}`;
}

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/// Convert a unix-seconds timestamp to the value format required by
/// `<input type="datetime-local">` (`YYYY-MM-DDTHH:mm`, in the user's local
/// timezone). Empty / null input yields an empty string.
function unixToDatetimeLocal(unix: number | null | undefined): string {
  if (!unix) return "";
  const date = new Date(unix * 1000);
  if (Number.isNaN(date.getTime())) return "";
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}T${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

/// Inverse of `unixToDatetimeLocal`. Returns `null` for empty / unparseable
/// input. Date.parse interprets the local-time string in the user's tz, which
/// matches what the input control collected.
function datetimeLocalToUnix(value: string): number | null {
  if (!value) return null;
  const ms = Date.parse(value);
  if (Number.isNaN(ms)) return null;
  return Math.floor(ms / 1000);
}

type ExpiryTone = "muted" | "ok" | "warn" | "danger";

function formatExpiry(unix: number | null | undefined): { text: string; tone: ExpiryTone } {
  if (!unix) return { text: "永不过期", tone: "muted" };
  const now = Math.floor(Date.now() / 1000);
  const date = new Date(unix * 1000);
  const dateText = `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  const diffSec = unix - now;
  if (diffSec <= 0) {
    const days = Math.max(1, Math.floor((now - unix) / 86400));
    return { text: `${dateText}（已过期 ${days}d）`, tone: "danger" };
  }
  const days = Math.floor(diffSec / 86400);
  if (days === 0) {
    const hours = Math.max(1, Math.floor(diffSec / 3600));
    return { text: `${dateText}（剩 ${hours}h）`, tone: "warn" };
  }
  if (days <= 7) return { text: `${dateText}（剩 ${days}d）`, tone: "warn" };
  return { text: `${dateText}（剩 ${days}d）`, tone: "ok" };
}

function isValidNvapi(raw: string): boolean {
  return /^nvapi-[A-Za-z0-9_\-]{10,}$/.test(raw.trim());
}

function copyButton(value: string, attrName = "copy"): string {
  const id = `cp_${attrName}_${Math.random().toString(36).slice(2, 8)}`;
  return `<button id="${id}" class="btn-icon" data-copy="${escapeHtml(value)}" title="复制" aria-label="复制">${ICONS.copy}</button>`;
}

function render() {
  if (!config) {
    app.innerHTML = '<div class="loading">加载中...</div>';
    return;
  }

  app.innerHTML = `
    <div class="app-shell ${sidebarCollapsed ? "sidebar-collapsed" : ""}">
      <aside class="sidebar">
        <button
          id="sidebarToggle"
          class="sidebar-toggle"
          aria-label="${sidebarCollapsed ? "展开侧栏" : "收起侧栏"}"
          title="${sidebarCollapsed ? "展开侧栏" : "收起侧栏"}"
        >${ICONS.chevronLeft}</button>
        <div class="brand">
          <div class="brand-mark">CC</div>
          <div class="brand-text">
            <div class="brand-title">CCNim</div>
            <div class="brand-subtitle">Claude Code · NVIDIA NIM 桌面代理</div>
          </div>
        </div>
        <nav class="nav">
          <div class="nav-section">导航</div>
          ${[
            { id: "dashboard", label: "仪表盘" },
            { id: "proxy", label: "代理设置" },
            { id: "keys", label: "API Keys" },
            { id: "models", label: "模型映射" },
            { id: "ide", label: "IDE 接入" },
          ]
            .map(
              (it) => `
            <button
              class="nav-item ${activeView === it.id ? "active" : ""}"
              data-target="${it.id}"
              title="${it.label}"
            >
              ${ICONS[it.id as keyof typeof ICONS] ?? ""}<span>${it.label}</span>
            </button>`,
            )
            .join("")}
        </nav>
      </aside>

      <main class="workspace">
        <div class="content">${renderView()}</div>
      </main>
    </div>
    ${renderUpdateModal()}
  `;

  bind();
}

function renderView(): string {
  switch (activeView) {
    case "dashboard":
      return renderDashboard();
    case "proxy":
      return renderProxy();
    case "keys":
      return renderKeys();
    case "models":
      return renderModels();
    case "ide":
      // First visit: kick off the scan asynchronously. The empty list will
      // render as "扫描中..."; the scan callback will trigger another
      // render() once data is back. Subsequent visits keep the cached list
      // and rely on an explicit refresh button.
      if (ideProfiles.length === 0 && !idesScanning && !idesScanError) {
        void refreshIdeProfiles();
      }
      return renderIDE();
    default:
      return renderDashboard();
  }
}

function rateLimitOf(): { limit: number; window: number } {
  return {
    limit: config?.rate_limit_per_key ?? 40,
    window: config?.rate_window_secs ?? 60,
  };
}

function renderUpdateBanner(): string {
  if (updateState.stage !== "available" || !updateState.latestVersion) return "";
  return `
    <div class="update-banner">
      <span><strong>发现新版本 v${escapeHtml(updateState.latestVersion)}</strong> · 当前 v${escapeHtml(updateState.currentVersion)}</span>
      <div class="update-banner-actions">
        <button id="updateView" class="btn-ghost">查看</button>
        <button id="updateInstall" class="btn-primary">立即安装</button>
        <button id="updateDismiss" class="btn-ghost" title="稍后再说">×</button>
      </div>
    </div>
  `;
}

function renderUpdateModal(): string {
  if (!updateModalOpen) return "";
  const stage = updateState.stage;
  let title = "检查更新";
  let body = "";
  let actions = `<button id="modalClose" class="btn-ghost">关闭</button>`;

  if (stage === "checking") {
    title = "正在检查更新…";
    body = `<p>正在从 GitHub Releases 拉取最新发布信息，请稍候。</p>`;
    actions = "";
  } else if (stage === "available" && updateState.latestVersion) {
    title = "发现新版本";
    body = `
      <div class="modal-version">
        <span class="version-pill">v${escapeHtml(updateState.currentVersion)}</span>
        <span class="version-arrow">→</span>
        <span class="version-pill accent">v${escapeHtml(updateState.latestVersion)}</span>
      </div>
      ${updateState.notes ? `<div class="modal-notes">${escapeHtml(updateState.notes)}</div>` : `<p>已检测到更新版本，建议现在安装。</p>`}
    `;
    actions = `
      <button id="modalLater" class="btn-ghost">稍后</button>
      <button id="modalInstall" class="btn-primary">下载并安装</button>
    `;
  } else if (stage === "downloading") {
    title = "正在下载更新…";
    const total = updateState.total;
    const pct = total ? Math.min(100, (updateState.downloaded / total) * 100) : null;
    body = `
      <p>请保持网络连接，下载完成后会自动安装。</p>
      <div class="modal-progress">
        <div class="modal-progress-track">
          <div class="modal-progress-fill" style="width:${pct === null ? 100 : pct.toFixed(1)}%${pct === null ? ";opacity:.5" : ""}"></div>
        </div>
        <div class="modal-progress-meta">
          <span>${formatBytes(updateState.downloaded)}${total ? ` / ${formatBytes(total)}` : ""}</span>
          <span>${pct === null ? "下载中…" : pct.toFixed(1) + "%"}</span>
        </div>
      </div>
    `;
    actions = "";
  } else if (stage === "ready") {
    title = "安装完成";
    body = `<p>新版本已安装，重启应用以生效。重启会先关闭代理服务，端口将正常释放。</p>`;
    actions = `
      <button id="modalLater" class="btn-ghost">下次启动再说</button>
      <button id="modalRelaunch" class="btn-primary">立即重启</button>
    `;
  } else if (stage === "error") {
    title = "更新失败";
    body = `<p>${escapeHtml(updateState.error ?? "未知错误")}</p><p>可以稍后再试，或前往 GitHub Releases 手动下载安装包。</p>`;
    actions = `<button id="modalClose" class="btn-primary">关闭</button>`;
  } else {
    title = "已是最新版本";
    body = `<p>当前版本 v${escapeHtml(updateState.currentVersion)} 已是最新。</p>`;
  }

  return `
    <div class="modal-backdrop" id="modalBackdrop">
      <div class="modal" role="dialog" aria-labelledby="modalTitle">
        <div class="modal-head">
          <h3 id="modalTitle">${title}</h3>
        </div>
        <div class="modal-body">${body}</div>
        <div class="modal-actions">${actions}</div>
      </div>
    </div>
  `;
}

function bindUpdateUi() {
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;
  byId<HTMLButtonElement>("checkUpdate")?.addEventListener("click", () => {
    void checkForUpdate(true);
  });
  byId<HTMLButtonElement>("updateView")?.addEventListener("click", () => {
    updateModalOpen = true;
    render();
  });
  byId<HTMLButtonElement>("updateInstall")?.addEventListener("click", () => {
    void startUpdateInstall();
  });
  byId<HTMLButtonElement>("updateDismiss")?.addEventListener("click", dismissUpdateBanner);
  byId<HTMLButtonElement>("modalLater")?.addEventListener("click", dismissUpdateModal);
  byId<HTMLButtonElement>("modalClose")?.addEventListener("click", dismissUpdateModal);
  byId<HTMLButtonElement>("modalInstall")?.addEventListener("click", () => {
    void startUpdateInstall();
  });
  byId<HTMLButtonElement>("modalRelaunch")?.addEventListener("click", () => {
    void relaunchApp();
  });
  // Click outside the modal to dismiss — only when not in a state where
  // closing would be destructive (e.g. mid-download). The downloading
  // path has no actions buttons, so the user visually understands they
  // need to wait; clicking the backdrop in that state is a no-op.
  byId<HTMLDivElement>("modalBackdrop")?.addEventListener("click", (e) => {
    if (e.target !== e.currentTarget) return;
    if (updateState.stage === "downloading") return;
    dismissUpdateModal();
  });
}

function renderDashboard(): string {
  if (!config) return "";
  const isRunning = proxyStatus?.running ?? false;
  const listenUrl = proxyStatus?.listen_url || `http://${config.host}:${config.port}`;
  const keyCount = config.nim_api_keys.length;
  const keys = proxyStatus?.keys ?? [];
  const healthyKeys = keys.filter((k) => normalizeState(k.state) === "healthy").length;
  const totalInflight = keys.reduce((sum, k) => sum + k.inflight, 0);
  const totalRecent = keys.reduce((sum, k) => sum + k.recent_requests, 0);
  const { limit } = rateLimitOf();

  return `
    <header class="page-header">
      <div>
        <h1>仪表盘 ${updateState.currentVersion ? `<span class="version-pill">v${escapeHtml(updateState.currentVersion)}</span>` : ""}</h1>
        <p>本地 Anthropic 兼容代理，使用 NVIDIA NIM 作为上游提供 Claude Code 服务。</p>
      </div>
      <div class="header-actions">
        <button id="checkUpdate" class="btn-ghost" ${updateState.stage === "checking" ? "disabled" : ""}>
          ${updateState.stage === "checking" ? "检查中..." : "检查更新"}
        </button>
        ${
          isRunning
            ? `<button id="stop" class="btn-danger">停止代理</button>`
            : `<button id="start" class="btn-primary">启动代理</button>`
        }
      </div>
    </header>

    ${renderUpdateBanner()}

    <div class="metric-grid">
      <div class="metric-card ${isRunning ? "metric-ok" : ""}">
        <div class="metric-label">代理状态</div>
        <div class="metric-value">${isRunning ? "运行中" : "未启动"}</div>
        <div class="metric-sub mono">${escapeHtml(listenUrl)}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">默认模型</div>
        <div class="metric-value mono">${escapeHtml(config.model_mapping.default_model)}</div>
        <div class="metric-sub">Anthropic → NIM 路由</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">API Keys</div>
        <div class="metric-value">${healthyKeys}<span style="font-size:14px;color:var(--text-dim);font-weight:600"> / ${keyCount}</span></div>
        <div class="metric-sub">${keyCount === 0 ? "未配置 Key" : `${healthyKeys} 个健康 · 总配额 ${keyCount * limit}/分钟`}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">实时请求</div>
        <div class="metric-value">${totalInflight}<span style="font-size:14px;color:var(--text-dim);font-weight:600"> 进行中</span></div>
        <div class="metric-sub">最近 ${rateLimitOf().window}s 共 ${totalRecent} 次</div>
      </div>
    </div>

    <section class="card">
      <div class="card-head">
        <div>
          <h2>快速操作</h2>
          <p>启动代理后，使用一键启动按钮就能在外部终端打开已注入环境变量的 Claude Code。</p>
        </div>
      </div>
      <div class="quick-grid">
        <button id="openClaude" class="quick-btn primary" ${isRunning ? "" : "disabled"}>
          <strong>打开预配置终端</strong>
          <span>已注入环境变量，cd 到项目目录后运行 claude</span>
        </button>
        <button class="quick-btn" data-nav="keys">
          <strong>管理 API Keys</strong>
          <span>多 Key 自动切换 · ${limit} 次/分钟 限速</span>
        </button>
        <button class="quick-btn" data-nav="models">
          <strong>模型映射</strong>
          <span>Opus / Sonnet / Haiku → NIM</span>
        </button>
        <button class="quick-btn" data-nav="ide">
          <strong>IDE 接入指南</strong>
          <span>VSCode / Cursor / JetBrains 中安装 Claude Code 插件</span>
        </button>
      </div>
    </section>

    <section class="card">
      <div class="card-head">
        <div>
          <h2>API Key 健康概览</h2>
          <p>每个 Key 的实时状态、并发请求与最近窗口用量，按健康度自动切换。</p>
        </div>
        <button class="btn-ghost" data-nav="keys">前往管理 →</button>
      </div>
      <div id="keyList">${renderKeyList(keys, 6)}</div>
    </section>

    <section class="card">
      <div class="card-head">
        <div>
          <h2>诊断日志</h2>
          <p>记录每次配置加载/保存的结果（包括 secrets.json 写入后的回读校验），方便排查"保存的 Key 未持久化"等问题。</p>
        </div>
        <div style="display:flex;gap:8px">
          <button id="diagRefresh" class="btn-ghost">刷新</button>
        </div>
      </div>
      ${
        diagnosticLog
          ? `<pre class="diag-pre selectable">${escapeHtml(diagnosticLog.split("\n").slice(-40).join("\n"))}</pre>`
          : `<div class="empty"><strong>暂无诊断日志</strong><br/>点击"刷新"加载最近 40 条；或保存配置/重启代理后再来这里查看 secrets.json 读写结果。</div>`
      }
    </section>
  `;
}

function renderKeyList(keys: KeySnapshot[], limit?: number): string {
  if (keys.length === 0) {
    const configured = config?.nim_api_keys.length ?? 0;
    if (configured === 0) {
      return `<div class="empty"><strong>尚未配置 NVIDIA NIM API Key</strong><br/>前往 "API Keys" 页面添加 nvapi- 开头的 Key。</div>`;
    }
    if (!proxyStatus?.running) {
      return `<div class="empty"><strong>已配置 ${configured} 个 Key</strong><br/>启动代理后这里会显示每个 Key 的实时健康状态。</div>`;
    }
    return `<div class="empty"><strong>正在收集运行数据…</strong><br/>请等待几秒，或先发起一次请求触发统计。</div>`;
  }
  const list = limit ? keys.slice(0, limit) : keys;
  return `<div class="keys-grid">${list.map((k) => renderKeyCard(k)).join("")}</div>`;
}

function renderKeyCard(k: KeySnapshot): string {
  const { limit, window: rateWindow } = rateLimitOf();
  const ratio = limit > 0 ? Math.min(1, k.recent_requests / limit) : 0;
  const fillCls = ratio >= 0.9 ? "danger" : ratio >= 0.6 ? "warn" : "";
  const widthPct = (ratio * 100).toFixed(1);
  const stateNorm = normalizeState(k.state);
  const expiry = formatExpiry(k.expires_at);
  return `
    <div class="key-card state-${stateNorm} expiry-${expiry.tone}">
      <div class="key-card-head">
        <div class="key-id-row">
          <span class="key-num">#${k.id + 1}</span>
          <span class="key-id selectable">${escapeHtml(k.masked)}</span>
        </div>
        ${keyStateBadge(k.state)}
      </div>
      ${k.label ? `<div class="key-label">${escapeHtml(k.label)}</div>` : ""}
      <div class="usage-bar">
        <div class="usage-bar-head">
          <span>最近 ${rateWindow}s 用量</span>
          <span class="usage-fraction">${k.recent_requests} / ${limit}</span>
        </div>
        <div class="usage-bar-track">
          <div class="usage-bar-fill ${fillCls}" style="width:${widthPct}%"></div>
        </div>
      </div>
      <dl class="key-meta">
        <div><dt>并发</dt><dd>${k.inflight}</dd></div>
        <div><dt>失败</dt><dd class="${k.failure_count > 0 ? "fail" : ""}">${k.failure_count}</dd></div>
        <div class="key-expiry-row"><dt>到期</dt><dd class="expiry-${expiry.tone}">${escapeHtml(expiry.text)}</dd></div>
      </dl>
    </div>
  `;
}

function renderProxy(): string {
  if (!config) return "";
  const listenUrl = `http://${config.host}:${config.port}`;
  const tokenInputType = showToken ? "text" : "password";
  return `
    <header class="page-header">
      <div>
        <h1>代理设置</h1>
        <p>本地 Anthropic 兼容端点的监听地址、端口与认证 Token。修改后请保存并重启代理。</p>
      </div>
      <div class="header-actions">
        <button id="save" class="btn-primary">保存配置</button>
      </div>
    </header>

    <section class="card">
      <div class="card-head">
        <div><h2>监听信息</h2><p>客户端用以下 URL 与 Token 调用本地代理。</p></div>
      </div>
      <div class="form-grid three">
        <label class="field"><span>Host</span><input id="host" value="${escapeHtml(config.host)}" placeholder="127.0.0.1" /></label>
        <label class="field"><span>Port</span><input id="port" type="number" min="1" max="65535" value="${config.port}" /></label>
        <label class="field">
          <span>Auth Token <em class="hint-inline">客户端使用</em></span>
          <div class="input-group">
            <input id="token" type="${tokenInputType}" value="${escapeHtml(config.auth_token)}" placeholder="freecc" />
            <button id="tokToggle" class="btn-icon" type="button" title="${showToken ? "隐藏" : "显示"}" aria-label="切换显示">${showToken ? ICONS.eyeOff : ICONS.eye}</button>
            ${copyButton(config.auth_token, "token")}
          </div>
        </label>
      </div>
      <div class="info-grid" style="margin-top:14px">
        <div class="info-box">
          <div class="info-box-head">
            <span>Anthropic Base URL</span>
            ${copyButton(listenUrl, "url")}
          </div>
          <code>${escapeHtml(listenUrl)}</code>
        </div>
        <div class="info-box">
          <div class="info-box-head">
            <span>状态</span>
          </div>
          <code>${proxyStatus?.running ? "运行中" : "未启动"}</code>
        </div>
        <div class="info-box">
          <div class="info-box-head">
            <span>默认模型</span>
          </div>
          <code>${escapeHtml(config.model_mapping.default_model)}</code>
        </div>
      </div>
    </section>

    <section class="card">
      <div class="card-head">
        <div><h2>限流策略</h2><p>每个 Key 在指定窗口内允许的最大请求数，超过后自动切换到其他 Key。</p></div>
      </div>
      <div class="form-grid two">
        <label class="field"><span>每 Key 限流 <em class="hint-inline">建议 40</em></span><input id="rateLimit" type="number" min="1" value="${config.rate_limit_per_key}" /></label>
        <label class="field"><span>窗口秒数 <em class="hint-inline">建议 60</em></span><input id="rateWindow" type="number" min="1" value="${config.rate_window_secs}" /></label>
      </div>
    </section>
  `;
}

function renderKeys(): string {
  if (!config) return "";
  const { limit, window: rateWindow } = rateLimitOf();
  const snapshots = proxyStatus?.keys ?? [];
  const totalQuota = config.nim_api_keys.length * limit;
  return `
    <header class="page-header">
      <div>
        <h1>API Keys</h1>
        <p>逐个添加或批量导入 nvapi- Key，可附加备注与到期时间。运行时按健康度、并发与最近请求自动切换。</p>
      </div>
      <div class="header-actions">
        ${proxyStatus?.running ? `<span class="hint-inline">编辑后立即生效，无需重启代理</span>` : ""}
      </div>
    </header>

    <div class="banner">
      <div class="banner-icon">${ICONS.bolt}</div>
      <div class="banner-text">
        <strong>每个 NVIDIA NIM 账号限速 <span class="mono">${limit}</span> 次 / <span class="mono">${rateWindow}</span> 秒</strong>
        <p>当前共配置 <strong>${config.nim_api_keys.length}</strong> 个 Key，理论合并配额约 <strong>${totalQuota}</strong> 次/分钟。已过期、被上游禁用或冷却中的 Key 不参与轮询。</p>
        <p class="banner-secure">${ICONS.shield}<span>Key 写入用户配置目录下独立的 <code>secrets.json</code>（仅当前用户可读，Unix 自动 chmod 600），不写入项目仓库，亦不上传任何远程服务。</span></p>
      </div>
    </div>

    <section class="card">
      <div class="card-head">
        <div><h2>添加 Key</h2><p>逐个添加可以同时填备注与到期时间；批量导入支持一行一个 nvapi- 值。</p></div>
        <div class="header-actions">
          <button id="btnAddSingle" class="${addPanel === "single" ? "btn-primary" : "btn-ghost"}">+ 添加单个</button>
          <button id="btnAddBatch" class="${addPanel === "batch" ? "btn-primary" : "btn-ghost"}">批量导入</button>
        </div>
      </div>
      ${addPanel === "single" ? renderSingleAddForm() : ""}
      ${addPanel === "batch" ? renderBatchAddForm() : ""}
    </section>

    <section class="card">
      <div class="card-head">
        <div><h2>已保存 (${config.nim_api_keys.length})</h2><p>每张卡片对应一个 Key，可独立编辑备注、到期与值。${proxyStatus?.running ? "右侧实时显示运行健康状态，每 3 秒刷新。" : "代理未运行，运行后会显示实时健康状态。"}</p></div>
      </div>
      <div id="keyList">${renderManagedKeys(snapshots)}</div>
    </section>
  `;
}

function renderSingleAddForm(): string {
  return `
    <div class="add-form">
      <label class="field">
        <span>API Key 值 <em class="hint-inline">必填，nvapi- 开头</em></span>
        <div class="input-group">
          <input id="addValue" type="${showToken ? "text" : "password"}" autocomplete="off" placeholder="nvapi-xxxxxxxxxxxxxxxx" value="${escapeHtml(singleAdd.value)}" />
          <button id="addToggle" class="btn-icon" type="button" aria-label="切换显示" title="${showToken ? "隐藏" : "显示"}">${showToken ? ICONS.eyeOff : ICONS.eye}</button>
        </div>
      </label>
      <div class="form-grid two">
        <label class="field"><span>备注 <em class="hint-inline">可选</em></span><input id="addLabel" placeholder="例如：主账号 / dev" value="${escapeHtml(singleAdd.label)}" /></label>
        <label class="field">
          <span>到期时间 <em class="hint-inline">可选 · 留空表示永不过期</em></span>
          <div class="input-group">
            <input id="addExpiry" type="datetime-local" value="${escapeHtml(singleAdd.expiresAt)}" />
            <button id="addExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
      <div class="form-actions">
        <button id="addCancel" class="btn-ghost" type="button">取消</button>
        <button id="addSubmit" class="btn-primary" type="button">添加</button>
      </div>
    </div>
  `;
}

function renderBatchAddForm(): string {
  return `
    <div class="add-form">
      <label class="field">
        <span>批量 API Key <em class="hint-inline">每行一个，# 开头视为注释</em></span>
        <textarea id="batchValues" rows="6" class="keys-textarea" placeholder="nvapi-xxxxxxxxxxxxxxxx&#10;nvapi-yyyyyyyyyyyyyyyy&#10;# 这一行会被忽略">${escapeHtml(batchAdd.values)}</textarea>
      </label>
      <div class="form-grid two">
        <label class="field"><span>共享备注 <em class="hint-inline">可选 · 应用到所有导入项</em></span><input id="batchLabel" placeholder="例如：team-a" value="${escapeHtml(batchAdd.labelPrefix)}" /></label>
        <label class="field">
          <span>共享到期时间 <em class="hint-inline">可选 · 应用到所有导入项</em></span>
          <div class="input-group">
            <input id="batchExpiry" type="datetime-local" value="${escapeHtml(batchAdd.expiresAt)}" />
            <button id="batchExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
      <div class="form-actions">
        <button id="batchCancel" class="btn-ghost" type="button">取消</button>
        <button id="batchSubmit" class="btn-primary" type="button">导入</button>
      </div>
    </div>
  `;
}

function renderManagedKeys(snapshots: KeySnapshot[]): string {
  if (!config) return "";
  if (config.nim_api_keys.length === 0) {
    return `<div class="empty"><strong>还没有配置任何 Key</strong><br/>点上方"+ 添加单个"或"批量导入"开始。</div>`;
  }
  const byId = new Map(snapshots.map((s) => [s.id, s] as const));
  return `<div class="keys-grid managed">
    ${config.nim_api_keys.map((k, i) => renderManagedKeyCard(k, i, byId.get(i))).join("")}
  </div>`;
}

function renderManagedKeyCard(k: NimApiKey, index: number, snap: KeySnapshot | undefined): string {
  if (editingKeyId === k.id) return renderEditCard(k);

  const masked = maskKey(k.value);
  const stateBadge = snap ? keyStateBadge(snap.state) : `<span class="badge badge-neutral">未运行</span>`;
  const stateNorm = snap ? normalizeState(snap.state) : "neutral";
  const expiry = formatExpiry(k.expires_at);
  const { limit, window: rateWindow } = rateLimitOf();
  const recent = snap?.recent_requests ?? 0;
  const ratio = limit > 0 ? Math.min(1, recent / limit) : 0;
  const fillCls = ratio >= 0.9 ? "danger" : ratio >= 0.6 ? "warn" : "";
  const widthPct = (ratio * 100).toFixed(1);
  return `
    <div class="key-card managed state-${stateNorm} expiry-${expiry.tone}">
      <div class="key-card-head">
        <div class="key-id-row">
          <span class="key-num">#${index + 1}</span>
          ${stateBadge}
        </div>
        <div class="key-actions">
          <button class="btn-icon" data-edit-id="${escapeHtml(k.id)}" type="button" title="编辑" aria-label="编辑">✎</button>
          <button class="btn-icon danger" data-delete-id="${escapeHtml(k.id)}" type="button" title="删除" aria-label="删除">×</button>
        </div>
      </div>
      <div class="key-value-row">
        <code class="key-id selectable">${escapeHtml(masked)}</code>
        <div class="key-value-actions">
          ${copyButton(k.value, "key_" + k.id)}
        </div>
      </div>
      <dl class="key-attrs">
        <div><dt>备注</dt><dd>${k.label ? escapeHtml(k.label) : '<span class="muted">未填写</span>'}</dd></div>
        <div><dt>到期</dt><dd class="expiry expiry-${expiry.tone}">${escapeHtml(expiry.text)}</dd></div>
      </dl>
      <div class="usage-bar">
        <div class="usage-bar-head">
          <span>最近 ${rateWindow}s 用量</span>
          <span class="usage-fraction">${recent} / ${limit}</span>
        </div>
        <div class="usage-bar-track">
          <div class="usage-bar-fill ${fillCls}" style="width:${widthPct}%"></div>
        </div>
      </div>
      <dl class="key-meta">
        <div><dt>并发</dt><dd>${snap?.inflight ?? 0}</dd></div>
        <div><dt>失败</dt><dd class="${(snap?.failure_count ?? 0) > 0 ? "fail" : ""}">${snap?.failure_count ?? 0}</dd></div>
      </dl>
    </div>
  `;
}

function renderEditCard(k: NimApiKey): string {
  return `
    <div class="key-card managed editing">
      <div class="key-card-head">
        <strong>编辑 Key</strong>
        <div class="key-actions">
          <button id="editCancel" class="btn-ghost" type="button">取消</button>
          <button id="editSave" class="btn-primary" type="button" data-edit-save="${escapeHtml(k.id)}">保存</button>
        </div>
      </div>
      <label class="field">
        <span>API Key 值</span>
        <div class="input-group">
          <input id="editValue" type="${showToken ? "text" : "password"}" autocomplete="off" value="${escapeHtml(editForm.value)}" />
          <button id="editToggle" class="btn-icon" type="button" aria-label="切换显示" title="${showToken ? "隐藏" : "显示"}">${showToken ? ICONS.eyeOff : ICONS.eye}</button>
        </div>
      </label>
      <div class="form-grid two">
        <label class="field"><span>备注</span><input id="editLabel" placeholder="可选" value="${escapeHtml(editForm.label)}" /></label>
        <label class="field">
          <span>到期时间 <em class="hint-inline">留空表示永不过期</em></span>
          <div class="input-group">
            <input id="editExpiry" type="datetime-local" value="${escapeHtml(editForm.expiresAt)}" />
            <button id="editExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
    </div>
  `;
}

function combobox(id: string, value: string, placeholder: string): string {
  return `
    <div class="combobox" data-combobox="${id}">
      <input id="${id}" class="combobox-input" value="${escapeHtml(value)}" placeholder="${escapeHtml(placeholder)}" autocomplete="off" />
      <button type="button" class="combobox-toggle" aria-label="展开下拉">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="6 9 12 15 18 9"/></svg>
      </button>
      <div class="combobox-list" hidden></div>
    </div>
  `;
}

function renderModels(): string {
  if (!config) return "";
  const hasKeys = config.nim_api_keys.length > 0;
  let modelsHint: string;
  if (models.length > 0) {
    modelsHint = `已加载 ${models.length} 个 NVIDIA NIM 模型，每 30 分钟自动刷新一次。可点击下拉或直接输入过滤。`;
  } else if (!hasKeys) {
    modelsHint = "尚未配置 API Key，无法拉取模型列表。先在「API Keys」页面添加 nvapi- Key 后软件会自动获取。";
  } else {
    modelsHint = "正在自动拉取 NVIDIA NIM 模型列表（首次启动可能需要数秒）。也可以手动输入模型 ID。";
  }
  return `
    <header class="page-header">
      <div>
        <h1>模型映射</h1>
        <p>Claude 的 Opus / Sonnet / Haiku 请求会按这里的设置映射到对应的 NIM 模型，留空走默认。</p>
      </div>
      <div class="header-actions">
        <button id="save" class="btn-primary">保存配置</button>
      </div>
    </header>

    <section class="card">
      <div class="card-head">
        <div><h2>路由配置</h2><p>${escapeHtml(modelsHint)}</p></div>
      </div>
      <div class="form-stack">
        <label class="field"><span>默认模型 <em>必填</em></span>${combobox("defaultModel", config.model_mapping.default_model, "选择或输入模型 ID")}</label>
        <label class="field"><span>Claude Opus 映射 <em class="hint-inline">留空走默认</em></span>${combobox("opusModel", config.model_mapping.opus_model ?? "", "留空时使用默认模型")}</label>
        <label class="field"><span>Claude Sonnet 映射 <em class="hint-inline">留空走默认</em></span>${combobox("sonnetModel", config.model_mapping.sonnet_model ?? "", "留空时使用默认模型")}</label>
        <label class="field"><span>Claude Haiku 映射 <em class="hint-inline">留空走默认</em></span>${combobox("haikuModel", config.model_mapping.haiku_model ?? "", "留空时使用默认模型")}</label>
      </div>
    </section>

    <section class="card">
      <div class="card-head">
        <div><h2>高级</h2></div>
      </div>
      <label class="toggle-row">
        <input id="thinking" type="checkbox" ${config.enable_thinking ? "checked" : ""} />
        <div>
          <strong>启用 thinking 输出</strong>
          <p>对 Claude 模型的 thinking 块进行透传，部分上游模型可能不支持。</p>
        </div>
      </label>
    </section>
  `;
}

function setupComboboxes() {
  document.querySelectorAll<HTMLDivElement>(".combobox").forEach((root) => {
    const input = root.querySelector<HTMLInputElement>(".combobox-input")!;
    const toggle = root.querySelector<HTMLButtonElement>(".combobox-toggle")!;
    const list = root.querySelector<HTMLDivElement>(".combobox-list")!;
    let activeIndex = -1;
    let visible: string[] = [];

    const filter = (q: string) => {
      const lower = q.trim().toLowerCase();
      visible = lower ? models.filter((m) => m.toLowerCase().includes(lower)) : models.slice();
      activeIndex = -1;
      if (visible.length === 0) {
        list.innerHTML = `<div class="combobox-empty">未找到模型 — 软件会在配置 NIM Key 后自动拉取（每 30 分钟刷新一次）</div>`;
      } else {
        list.innerHTML = visible
          .map(
            (m, i) =>
              `<div class="combobox-option" data-idx="${i}" title="${escapeHtml(m)}">${escapeHtml(m)}</div>`,
          )
          .join("");
      }
    };

    const open = () => {
      filter(input.value);
      list.hidden = false;
      root.classList.add("open");
    };

    const close = () => {
      list.hidden = true;
      root.classList.remove("open");
    };

    const choose = (value: string) => {
      input.value = value;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      close();
    };

    const setActive = (idx: number) => {
      const items = list.querySelectorAll<HTMLDivElement>(".combobox-option");
      items.forEach((el, i) => el.classList.toggle("active", i === idx));
      activeIndex = idx;
      const target = items[idx];
      if (target) target.scrollIntoView({ block: "nearest" });
    };

    input.addEventListener("focus", open);
    input.addEventListener("input", () => open());
    toggle.addEventListener("mousedown", (e) => {
      e.preventDefault();
      if (root.classList.contains("open")) {
        close();
      } else {
        input.focus();
        open();
      }
    });

    list.addEventListener("mousedown", (e) => {
      const target = (e.target as HTMLElement).closest<HTMLDivElement>(".combobox-option");
      if (!target) return;
      e.preventDefault();
      const idx = Number(target.dataset.idx);
      if (!Number.isNaN(idx) && visible[idx] !== undefined) {
        choose(visible[idx]);
      }
    });

    input.addEventListener("keydown", (e) => {
      if (list.hidden && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
        open();
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive(Math.min(activeIndex + 1, visible.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive(Math.max(activeIndex - 1, 0));
      } else if (e.key === "Enter") {
        if (!list.hidden && activeIndex >= 0) {
          e.preventDefault();
          choose(visible[activeIndex]);
        }
      } else if (e.key === "Escape") {
        close();
      }
    });

    document.addEventListener(
      "mousedown",
      (e) => {
        if (!root.contains(e.target as Node)) close();
      },
      { passive: true },
    );
  });
}

/// "已检测到的 IDE" 一键写入区。VSCode/Cursor/Windsurf 等共用同一份
/// `settings.json` schema，扫描其 `claudeCode.environmentVariables` 现状并
/// 提供一键写入按钮。后端 always 备份后再写、用 JSONC stripper 兼容用户
/// 的注释/尾随逗号，但因为我们以标准 JSON 回写所以注释会被剥掉——这点
/// 由 toast 显式提示用户。
function renderIdeAutoSection(currentBaseUrl: string): string {
  if (!config) return "";
  const currentToken = config.auth_token;
  return `
    <section class="card">
      <div class="card-head">
        <div>
          <h2>一键配置 VSCode 系列扩展</h2>
          <p>检测本机已安装的 IDE，写入 <code>claudeCode.environmentVariables</code> 把官方扩展指向 <code class="selectable">${escapeHtml(currentBaseUrl)}</code>。写入前会自动备份原文件到同目录的 <code>settings.json.bak</code>。</p>
        </div>
        <button id="ideRescan" class="btn-ghost" ${idesScanning ? "disabled" : ""}>${idesScanning ? "扫描中..." : "重新扫描"}</button>
      </div>
      ${renderIdeBody(currentBaseUrl, currentToken)}
    </section>
  `;
}

function renderIdeBody(currentBaseUrl: string, currentToken: string): string {
  if (idesScanError) {
    return `<div class="banner-warn">扫描失败：${escapeHtml(idesScanError)}</div>`;
  }
  if (idesScanning && ideProfiles.length === 0) {
    return `<div class="loading-inline">正在扫描已安装的 IDE...</div>`;
  }
  const installed = ideProfiles.filter((p) => p.exists);
  const missing = ideProfiles.filter((p) => !p.exists);

  const installedHtml = installed.length === 0
    ? `<div class="empty-state">尚未检测到任何 VSCode 系列 IDE。手动安装后点击右上角"重新扫描"。</div>`
    : `<div class="ide-auto-grid">${installed
        .map((p) => renderIdeAutoCard(p, currentBaseUrl, currentToken))
        .join("")}</div>`;

  const missingHtml = missing.length === 0 ? "" : `
    <details class="ide-missing">
      <summary>未安装 / 未检测到 (${missing.length})</summary>
      <ul>${missing
        .map(
          (p) => `<li><strong>${escapeHtml(p.name)}</strong> <span class="path-hint selectable">${escapeHtml(p.settings_path)}</span></li>`,
        )
        .join("")}</ul>
    </details>
  `;

  return installedHtml + missingHtml;
}

/// Reduce a base URL to a comparison-stable canonical form so that
/// equivalent loopback aliases register as "matching" instead of "stale".
/// Anything resolving to the local machine — `localhost`, `127.0.0.1`,
/// `0.0.0.0`, `::1` — collapses to the same canonical host. Trailing
/// slashes on an empty path are also normalized away. Falls back to a
/// trimmed copy of the input if the URL is unparseable.
function canonicalProxyUrl(url: string): string {
  try {
    const u = new URL(url);
    const host = u.hostname.toLowerCase().replace(/^\[|\]$/g, "");
    const isLoopback =
      host === "localhost" ||
      host === "127.0.0.1" ||
      host === "0.0.0.0" ||
      host === "::1";
    const canonHost = isLoopback ? "127.0.0.1" : host;
    const port = u.port ? `:${u.port}` : "";
    const path = u.pathname === "/" ? "" : u.pathname;
    return `${u.protocol}//${canonHost}${port}${path}`;
  } catch {
    return url.trim();
  }
}

function renderIdeAutoCard(
  profile: IdeProfile,
  currentBaseUrl: string,
  currentToken: string,
): string {
  // Three-way status:
  //   - "matches"   : both env vars present and equal to current proxy → green
  //   - "stale"     : key set but values differ from current proxy   → amber
  //   - "missing"   : key not set                                     → grey
  let badge: string;
  let statusText: string;
  const hasBoth =
    profile.configured_base_url !== null && profile.configured_auth_token !== null;
  if (hasBoth) {
    // Compare URLs after canonicalization so localhost ↔ 127.0.0.1 ↔
    // 0.0.0.0 ↔ ::1 (all loopback aliases) read as equivalent. The
    // raw value is still shown to the user in the "stale" message so
    // they can see exactly what's in their settings.json.
    const matches =
      canonicalProxyUrl(profile.configured_base_url ?? "") ===
        canonicalProxyUrl(currentBaseUrl) &&
      profile.configured_auth_token === currentToken;
    if (matches) {
      badge = `<span class="ide-badge ide-badge-ok">已配置</span>`;
      statusText = "扩展已经指向当前代理。";
    } else {
      badge = `<span class="ide-badge ide-badge-warn">需更新</span>`;
      statusText = `当前指向 <code class="selectable">${escapeHtml(profile.configured_base_url ?? "")}</code>，与本地代理不一致。`;
    }
  } else {
    badge = `<span class="ide-badge ide-badge-muted">未配置</span>`;
    statusText = "尚未在 settings.json 写入 Claude Code 环境变量。";
  }

  const applying = ideApplying === profile.id;
  const buttonLabel = hasBoth ? "重新写入" : "一键写入";
  return `
    <article class="ide-auto-card">
      <header class="ide-auto-head">
        <div>
          <h3>${escapeHtml(profile.name)}</h3>
          ${badge}
        </div>
        <button class="btn-primary ide-apply" data-ide-id="${escapeHtml(profile.id)}" ${applying ? "disabled" : ""}>
          ${applying ? "写入中..." : buttonLabel}
        </button>
      </header>
      <p class="ide-auto-status">${statusText}</p>
      <div class="ide-auto-path">
        <span class="path-label">settings.json</span>
        <code class="selectable">${escapeHtml(profile.settings_path)}</code>
      </div>
    </article>
  `;
}

function renderIDE(): string {
  if (!config) return "";
  const listenUrl = `http://${config.host}:${config.port}`;
  const psSnippet = `$env:ANTHROPIC_BASE_URL = '${listenUrl}'\n$env:ANTHROPIC_AUTH_TOKEN = '${config.auth_token}'\nclaude`;
  const bashSnippet = `export ANTHROPIC_BASE_URL='${listenUrl}'\nexport ANTHROPIC_AUTH_TOKEN='${config.auth_token}'\nclaude`;
  return `
    <header class="page-header">
      <div>
        <h1>IDE 接入</h1>
        <p>VSCode / Cursor / Windsurf 等 VSCode 系列 IDE 共用同一份 <code>settings.json</code>，可以一键写入 Claude Code 扩展所需的 <code>claudeCode.environmentVariables</code>，把官方扩展指向本地代理。</p>
      </div>
      <div class="header-actions">
        <button id="openClaude" class="btn-primary" ${proxyStatus?.running ? "" : "disabled title='请先启动代理'"}>打开预配置终端</button>
 <button id="openClaudeDesktop" class="btn-secondary" ${proxyStatus?.running ? "" : "disabled title='请先启动代理'"}>打开 Claude Desktop</button>
      </div>
    </header>

    ${renderIdeAutoSection(listenUrl)}

    <section class="card">
      <div class="card-head"><div><h2>连接参数</h2><p>所有 IDE 都通过这两个环境变量找到本地代理。</p></div></div>
      <div class="info-grid">
        <div class="info-box">
          <div class="info-box-head">
            <span>ANTHROPIC_BASE_URL</span>
            ${copyButton(listenUrl, "ide_url")}
          </div>
          <code class="selectable">${escapeHtml(listenUrl)}</code>
        </div>
        <div class="info-box">
          <div class="info-box-head">
            <span>ANTHROPIC_AUTH_TOKEN</span>
            ${copyButton(config.auth_token, "ide_token")}
          </div>
          <code class="selectable">${escapeHtml(config.auth_token)}</code>
        </div>
        <div class="info-box">
          <div class="info-box-head">
            <span>CLI 安装命令</span>
            ${copyButton("npm install -g @anthropic-ai/claude-code", "ide_install")}
          </div>
          <code class="selectable">npm install -g @anthropic-ai/claude-code</code>
        </div>
      </div>
    </section>

    <section class="card">
      <div class="card-head"><div><h2>整体思路</h2></div></div>
      <ol class="steps">
        <li>启动代理（顶部"启动代理"按钮），监听 <code class="selectable">${escapeHtml(listenUrl)}</code>。</li>
        <li>本机安装 Claude Code CLI：<code class="selectable">npm install -g @anthropic-ai/claude-code</code>。</li>
        <li>在任意终端导出 <code class="selectable">ANTHROPIC_BASE_URL</code> 与 <code class="selectable">ANTHROPIC_AUTH_TOKEN</code>，再运行 <code class="selectable">claude</code>。或者直接点上方"打开预配置终端"。</li>
        <li>在 IDE 里按下方说明安装并启用 Claude Code 插件，插件会自动调用同一个 <code>claude</code> CLI，从而透明使用本代理。</li>
      </ol>
    </section>

    <section class="card">
      <div class="card-head"><div><h2>环境变量片段（拷贝即用）</h2></div></div>
      <div class="info-grid">
        <div class="info-box">
          <div class="info-box-head">
            <span>PowerShell / Windows Terminal</span>
            ${copyButton(psSnippet, "ide_ps")}
          </div>
          <pre class="snippet selectable">${escapeHtml(psSnippet)}</pre>
        </div>
        <div class="info-box">
          <div class="info-box-head">
            <span>bash / zsh / fish-compatible</span>
            ${copyButton(bashSnippet, "ide_sh")}
          </div>
          <pre class="snippet selectable">${escapeHtml(bashSnippet)}</pre>
        </div>
      </div>
    </section>

    <div class="ide-grid">
      <article class="ide-card claude-card">
        <div class="ide-head">
          <div class="ide-mark">VS</div>
          <div>
            <h3>VSCode</h3>
            <p>官方 Claude Code 扩展，会自动检测系统中的 <code>claude</code> CLI。</p>
          </div>
        </div>
        <ol class="steps">
          <li>扩展市场搜索 <strong>Claude Code</strong>（发布者：Anthropic）。</li>
          <li>在终端中执行上面的"PowerShell"或"bash"片段，启动 <code>claude</code> 一次。</li>
          <li>VSCode 内打开命令面板：<code>Claude Code: Start Session</code>。插件会沿用刚才的环境变量。</li>
        </ol>
      </article>

      <article class="ide-card cursor-card">
        <div class="ide-head">
          <div class="ide-mark">CR</div>
          <div>
            <h3>Cursor / Windsurf</h3>
            <p>这些是 VSCode 分支，扩展兼容。同样安装 Anthropic 的 Claude Code 扩展。</p>
          </div>
        </div>
        <ol class="steps">
          <li>Cursor 扩展市场搜索 <strong>Claude Code</strong>，安装。</li>
          <li>不要走 Cursor 自家的 Anthropic Provider；让 Claude Code 扩展接管。</li>
          <li>用上方按钮打开预配置终端，或在 Cursor 自带终端里 export 环境变量再运行 <code>claude</code>。</li>
        </ol>
      </article>

      <article class="ide-card continue-card">
        <div class="ide-head">
          <div class="ide-mark">JB</div>
          <div>
            <h3>JetBrains 全家桶</h3>
            <p>IntelliJ / PyCharm / WebStorm / GoLand / RustRover 等。</p>
          </div>
        </div>
        <ol class="steps">
          <li>Settings → Plugins → Marketplace 搜索 <strong>Claude Code</strong>，安装并重启。</li>
          <li>Settings → Tools → Terminal，确保 "Environment variables" 中已有 <code>ANTHROPIC_BASE_URL</code> 与 <code>ANTHROPIC_AUTH_TOKEN</code>。</li>
          <li>插件侧边栏点击连接，会调用 <code>claude</code> CLI；首次需先在终端跑一次确认通畅。</li>
        </ol>
      </article>

      <article class="ide-card cline-card">
        <div class="ide-head">
          <div class="ide-mark">CL</div>
          <div>
            <h3>命令行 / 其它编辑器</h3>
            <p>Vim / Neovim / Emacs / Helix / Zed 等只要能开终端都能用。</p>
          </div>
        </div>
        <ol class="steps">
          <li>把上面的"bash 片段"加入你的 shell rc（<code>~/.bashrc</code>、<code>~/.zshrc</code>、<code>$PROFILE</code> 等）。</li>
          <li>新开终端直接 <code>claude</code> 即可，所有调用都会经本地代理转发到 NVIDIA NIM。</li>
          <li>编辑器侧用 <code>:terminal</code>、<code>tmux</code> 等启动 claude，无需额外插件。</li>
        </ol>
      </article>
    </div>
  `;
}

let lastRenderedRunning: boolean | null = null;

function updateRuntimeUI() {
  if (!proxyStatus) return;
  const isRunning = proxyStatus.running;
  // The dashboard primary CTA flips between "启动" / "停止", and several
  // other elements show different copy/styling depending on the running
  // state. Doing a full re-render only when the boolean actually flips
  // avoids stomping on focus/edit state during the 3s polling tick.
  if (lastRenderedRunning !== null && lastRenderedRunning !== isRunning) {
    lastRenderedRunning = isRunning;
    render();
    return;
  }
  lastRenderedRunning = isRunning;

  const keyList = document.getElementById("keyList");
  if (!keyList) return;
  if (activeView === "dashboard") {
    keyList.innerHTML = renderKeyList(proxyStatus.keys, 6);
    return;
  }
  if (activeView === "keys") {
    // Skip live re-render while the user is mid-edit so we don't yank focus
    // out of the inline form. The next save / cancel will trigger a full
    // render() that picks up the fresh stats.
    if (editingKeyId !== null) return;
    keyList.innerHTML = renderManagedKeys(proxyStatus.keys);
    bindManagedKeyControls();
    bindCopyButtons();
  }
}

function bindCopyButtons() {
  document.querySelectorAll<HTMLButtonElement>("button[data-copy]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const value = btn.getAttribute("data-copy") ?? "";
      await copy(value);
      btn.classList.add("copied");
      window.setTimeout(() => btn.classList.remove("copied"), 1200);
    });
  });
}

function bind() {
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

  document.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.target;
      if (!target || target === activeView) return;
      activeView = target;
      render();
    });
  });

  document.querySelectorAll<HTMLButtonElement>("[data-nav]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.nav;
      if (!target) return;
      activeView = target;
      render();
    });
  });

  byId<HTMLInputElement>("host")?.addEventListener("input", (e) =>
    setConfig("host", (e.target as HTMLInputElement).value),
  );
  byId<HTMLInputElement>("port")?.addEventListener("input", (e) =>
    setConfig("port", Number((e.target as HTMLInputElement).value)),
  );
  byId<HTMLInputElement>("token")?.addEventListener("input", (e) =>
    setConfig("auth_token", (e.target as HTMLInputElement).value),
  );
  byId<HTMLInputElement>("rateLimit")?.addEventListener("input", (e) =>
    setConfig("rate_limit_per_key", Number((e.target as HTMLInputElement).value)),
  );
  byId<HTMLInputElement>("rateWindow")?.addEventListener("input", (e) =>
    setConfig("rate_window_secs", Number((e.target as HTMLInputElement).value)),
  );
  byId<HTMLInputElement>("defaultModel")?.addEventListener("input", (e) =>
    setConfig("model_mapping.default_model", (e.target as HTMLInputElement).value),
  );
  byId<HTMLInputElement>("opusModel")?.addEventListener("input", (e) =>
    setConfig("model_mapping.opus_model", (e.target as HTMLInputElement).value || null),
  );
  byId<HTMLInputElement>("sonnetModel")?.addEventListener("input", (e) =>
    setConfig("model_mapping.sonnet_model", (e.target as HTMLInputElement).value || null),
  );
  byId<HTMLInputElement>("haikuModel")?.addEventListener("input", (e) =>
    setConfig("model_mapping.haiku_model", (e.target as HTMLInputElement).value || null),
  );
  byId<HTMLInputElement>("thinking")?.addEventListener("change", (e) =>
    setConfig("enable_thinking", (e.target as HTMLInputElement).checked),
  );

  byId<HTMLButtonElement>("start")?.addEventListener("click", startProxy);
  byId<HTMLButtonElement>("stop")?.addEventListener("click", stopProxy);
  byId<HTMLButtonElement>("sidebarToggle")?.addEventListener("click", toggleSidebar);
  byId<HTMLButtonElement>("save")?.addEventListener("click", () => save());
  byId<HTMLButtonElement>("openClaude")?.addEventListener("click", openClaudeTerminal);
 byId<HTMLButtonElement>("openClaudeDesktop")?.addEventListener("click", openClaudeDesktopApp);
  byId<HTMLButtonElement>("diagRefresh")?.addEventListener("click", refreshDiagnostic);
  byId<HTMLButtonElement>("tokToggle")?.addEventListener("click", () => {
    showToken = !showToken;
    render();
  });

  bindKeysPage();
  bindIdePage();
  bindUpdateUi();

  bindCopyButtons();
  setupComboboxes();
}

function bindIdePage() {
  if (activeView !== "ide") return;
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

  byId<HTMLButtonElement>("ideRescan")?.addEventListener("click", () => {
    void refreshIdeProfiles();
  });

  document.querySelectorAll<HTMLButtonElement>(".ide-apply").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.dataset.ideId;
      if (!id) return;
      void applyIdeSettings(id);
    });
  });
}

function bindKeysPage() {
  if (activeView !== "keys") return;
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

  byId<HTMLButtonElement>("btnAddSingle")?.addEventListener("click", () =>
    openAddPanel(addPanel === "single" ? null : "single"),
  );
  byId<HTMLButtonElement>("btnAddBatch")?.addEventListener("click", () =>
    openAddPanel(addPanel === "batch" ? null : "batch"),
  );

  byId<HTMLInputElement>("addValue")?.addEventListener("input", (e) => {
    singleAdd.value = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("addLabel")?.addEventListener("input", (e) => {
    singleAdd.label = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("addExpiry")?.addEventListener("input", (e) => {
    singleAdd.expiresAt = (e.target as HTMLInputElement).value;
  });
  byId<HTMLButtonElement>("addExpiryClear")?.addEventListener("click", () => {
    singleAdd.expiresAt = "";
    const input = byId<HTMLInputElement>("addExpiry");
    if (input) input.value = "";
  });
  byId<HTMLButtonElement>("addToggle")?.addEventListener("click", () => {
    showToken = !showToken;
    render();
  });
  byId<HTMLButtonElement>("addCancel")?.addEventListener("click", closeAddPanel);
  byId<HTMLButtonElement>("addSubmit")?.addEventListener("click", () => {
    void submitSingleAdd();
  });

  byId<HTMLTextAreaElement>("batchValues")?.addEventListener("input", (e) => {
    batchAdd.values = (e.target as HTMLTextAreaElement).value;
  });
  byId<HTMLInputElement>("batchLabel")?.addEventListener("input", (e) => {
    batchAdd.labelPrefix = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("batchExpiry")?.addEventListener("input", (e) => {
    batchAdd.expiresAt = (e.target as HTMLInputElement).value;
  });
  byId<HTMLButtonElement>("batchExpiryClear")?.addEventListener("click", () => {
    batchAdd.expiresAt = "";
    const input = byId<HTMLInputElement>("batchExpiry");
    if (input) input.value = "";
  });
  byId<HTMLButtonElement>("batchCancel")?.addEventListener("click", closeAddPanel);
  byId<HTMLButtonElement>("batchSubmit")?.addEventListener("click", () => {
    void submitBatchImport();
  });

  bindManagedKeyControls();
}

function bindManagedKeyControls() {
  document.querySelectorAll<HTMLButtonElement>("button[data-edit-id]").forEach((btn) => {
    btn.addEventListener("click", () => beginEditKey(btn.dataset.editId ?? ""));
  });
  document.querySelectorAll<HTMLButtonElement>("button[data-delete-id]").forEach((btn) => {
    btn.addEventListener("click", () => {
      void deleteKey(btn.dataset.deleteId ?? "");
    });
  });

  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

  byId<HTMLInputElement>("editValue")?.addEventListener("input", (e) => {
    editForm.value = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("editLabel")?.addEventListener("input", (e) => {
    editForm.label = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("editExpiry")?.addEventListener("input", (e) => {
    editForm.expiresAt = (e.target as HTMLInputElement).value;
  });
  byId<HTMLButtonElement>("editExpiryClear")?.addEventListener("click", () => {
    editForm.expiresAt = "";
    const input = byId<HTMLInputElement>("editExpiry");
    if (input) input.value = "";
  });
  byId<HTMLButtonElement>("editToggle")?.addEventListener("click", () => {
    showToken = !showToken;
    render();
  });
  byId<HTMLButtonElement>("editCancel")?.addEventListener("click", cancelEdit);
  byId<HTMLButtonElement>("editSave")?.addEventListener("click", (e) => {
    const id = (e.currentTarget as HTMLButtonElement).dataset.editSave ?? "";
    void submitEditKey(id);
  });
}

load();
