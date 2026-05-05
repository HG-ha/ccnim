import { invoke } from "@tauri-apps/api/core";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import "./style.css";

/// Wire-level discriminator for upstream protocol families. Must stay in
/// lockstep with the Rust `ProviderKind` enum (snake_case, `serde(rename_all)`).
type ProviderKind = "nim" | "openai_compat" | "anthropic_compat";

type NimApiKey = {
  id: string;
  value: string;
  label?: string | null;
  expires_at?: number | null;
  /// Optional in legacy configs; backend defaults missing values to "nim".
  provider?: ProviderKind;
  /// Empty / undefined means "use provider's default base URL".
  base_url?: string;
  /// Whether this key is enabled.
  enabled?: boolean;
  /// Per-key model mapping. If undefined, falls back to global config.
  /// Each slot also carries an optional `*_extra_body` JSON object —
  /// arbitrary fields the proxy deep-merges into the outgoing request
  /// body whenever this slot is the one that wins resolution. Lets
  /// power users pin upstream-specific knobs (`temperature`, `top_p`,
  /// `chat_template_kwargs.thinking`, …) per-mapping without dedicated
  /// GUI fields. Config values *win* over anything Claude Code sent.
  model_mapping?: {
    default_model?: string | null;
    default_extra_body?: Record<string, unknown> | null;
    opus_model?: string | null;
    opus_extra_body?: Record<string, unknown> | null;
    sonnet_model?: string | null;
    sonnet_extra_body?: Record<string, unknown> | null;
    haiku_model?: string | null;
    haiku_extra_body?: Record<string, unknown> | null;
  };
  /// Per-key rate-limit override (requests per the global window).
  /// `null` / undefined means "use the provider default": NIM picks
  /// the global `rate_limit_per_key`, while other providers have no
  /// local cap by default.
  rate_limit?: number | null;
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
  /// Stable identifier (matches `NimApiKey.id`). Used to attach
  /// per-key live metrics so usage history follows the key across
  /// edits / pool rebuilds.
  stable_id?: string;
  masked: string;
  label?: string | null;
  expires_at?: number | null;
  provider: ProviderKind;
  base_url: string;
  state: string;
  inflight: number;
  recent_requests: number;
  failure_count: number;
  /// Seconds remaining until the active cooldown lifts. Only present
  /// when `state === "cooling_down"`; lets the dashboard render an
  /// "auto-recover in 4m32s" hint without doing wall-clock math itself.
  cooldown_remaining_secs?: number | null;
  /// Effective rate-limit cap resolved by the backend (per-key
  /// override or provider default). `null` means "no local cap" —
  /// the GUI renders this as "不限" instead of `recent / 0`.
  rate_limit?: number | null;
};

/// Live per-key statistics surfaced by the running proxy. `None` on
/// the wire when the proxy is stopped.
type KeyMetrics = {
  stable_id: string;
  requests: number;
  successes: number;
  failures: number;
  input_tokens: number;
  output_tokens: number;
  avg_latency_ms: number;
  last_latency_ms: number;
  last_request_at: number;
};

type ModelMetrics = {
  model: string;
  calls: number;
  successes: number;
  failures: number;
  input_tokens: number;
  output_tokens: number;
  last_used_at: number;
};

type MetricsSnapshot = {
  started_at: number;
  uptime_secs: number;
  total_requests: number;
  total_successes: number;
  total_failures: number;
  total_input_tokens: number;
  total_output_tokens: number;
  keys: KeyMetrics[];
  models: ModelMetrics[];
};

/// Provider metadata for UI rendering and validation. The defaults come
/// from `ProviderKind::default_base_url` on the Rust side; the labels
/// here are user-facing only and do not need to match anything.
const PROVIDERS: Record<
  ProviderKind,
  {
    /// Long form shown in dropdown rows.
    label: string;
    /// Short badge text for cards.
    short: string;
    /// Pre-fill for the base URL input. Empty string means "user must
    /// type one explicitly" (true for `openai_compat`).
    defaultBaseUrl: string;
    /// Placeholder shown when the base URL is empty.
    placeholder: string;
    /// Free-form description shown beneath the dropdown.
    description: string;
    /// Sample key value used as placeholder text.
    keyPlaceholder: string;
  }
> = {
  nim: {
    label: "NVIDIA NIM",
    short: "NIM",
    defaultBaseUrl: "https://integrate.api.nvidia.com/v1",
    placeholder: "https://integrate.api.nvidia.com/v1",
    description:
      "NVIDIA NIM 官方端点。Key 必须以 nvapi- 开头。多 Key 自动轮询、限速。",
    keyPlaceholder: "nvapi-xxxxxxxxxxxxxxxx",
  },
  openai_compat: {
    label: "OpenAI 兼容",
    short: "OpenAI",
    defaultBaseUrl: "",
    placeholder: "https://api.deepseek.com 或 https://your-host/v1",
    description:
      "任何 OpenAI 兼容的 /chat/completions 端点。常见：DeepSeek、Moonshot、Groq、OpenRouter、自建 vLLM。",
    keyPlaceholder: "sk-xxxxxxxxxxxxxxxx",
  },
  anthropic_compat: {
    label: "Anthropic 兼容",
    short: "Anthropic",
    defaultBaseUrl: "https://api.anthropic.com",
    placeholder: "https://api.anthropic.com",
    description:
      "原生 Anthropic Messages API。请求/响应直接透传，保留 thinking、tool_use 等原生能力。",
    keyPlaceholder: "sk-ant-xxxxxxxxxxxxxxxx",
  },
};

const PROVIDER_KINDS: ProviderKind[] = ["nim", "openai_compat", "anthropic_compat"];

type ProxyStatus = {
  running: boolean;
  listen_url: string;
  default_model: string;
  keys: KeySnapshot[];
  /// Present only while the proxy is running — counters live in the
  /// running server's memory and reset on stop/start.
  metrics?: MetricsSnapshot | null;
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
/// Dedicated mount point for the edit-key modal — lives in the body
/// directly (see `index.html`), NOT inside `#app`. Keeping it out of
/// the main render tree means full `render()` calls (status polls,
/// sidebar nav, etc.) don't blow away the modal DOM mid-edit, so the
/// CSS entrance animation only plays once per session and the
/// currently-focused input keeps its focus / cursor / IME state.
const editModalRoot = document.querySelector<HTMLDivElement>("#editModalRoot")!;
/// Tracks whether the modal wrapper (`.modal-backdrop > .modal`) is
/// currently mounted. Used by `renderEditModalRoot` to decide between
/// a first-mount (full innerHTML write + animation) and an in-place
/// update (focus-preserving inner re-render only).
let editModalMounted = false;

let config: AppConfig | null = null;
let proxyStatus: ProxyStatus | null = null;
let models: string[] = [];
let activeView = "dashboard";
let toastTimer: number | undefined;
let statusTimer: number | undefined;
let modelsTimer: number | undefined;
let showToken = false;
let ideProfiles: IdeProfile[] = [];
let idesScanning = false;
let idesScanError: string | null = null;
let ideApplying: string | null = null;
let sidebarCollapsed = (() => {
  // Persisted user choice always wins. With no saved preference we
  // fall back on a width heuristic — narrow windows default to
  // collapsed so the 232 px expanded sidebar doesn't eat most of
  // the workspace on first launch — but the moment the user clicks
  // the toggle that choice sticks via localStorage and the heuristic
  // stops applying (resize during a session does NOT bounce the
  // sidebar around uninvited).
  try {
    const stored = window.localStorage.getItem("fcc.sidebarCollapsed");
    if (stored === "1") return true;
    if (stored === "0") return false;
  } catch {
    // localStorage unavailable (private mode, embedded webview) —
    // fall through to the width heuristic.
  }
  try {
    return window.innerWidth < 980;
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
/// Tracks the document-level Esc keydown handler the edit modal
/// installs, so subsequent re-renders during the same edit session can
/// detach the previous one before attaching a fresh one.
let editModalEscHandler: ((ev: KeyboardEvent) => void) | null = null;

/// Per-form scratch state. `baseUrl` is what the user typed (empty
/// string means "fall back to provider default" — the backend honours
/// the same convention). The `provider` field is always set to a
/// concrete value; we never let it be undefined to keep the rendering
/// code branchless.
const singleAdd = {
  value: "",
  label: "",
  expiresAt: "",
  provider: "nim" as ProviderKind,
  baseUrl: "",
  /// Rate-limit input as a string so the user can clear it back to
  /// "" (= inherit provider default). Parsed by `parseRateLimit` at
  /// submit time.
  rateLimit: "",
};
const batchAdd = {
  values: "",
  labelPrefix: "",
  expiresAt: "",
  provider: "nim" as ProviderKind,
  baseUrl: "",
  rateLimit: "",
};
/// Edit form state for an existing key card. The model-mapping fields
/// are stored as plain strings (`""` means "inherit"); they get
/// normalised into `null`s by `submitEditKey` before crossing the IPC
/// boundary so the Rust side sees a tidy `Option<String>`.
///
/// `availableModels` is populated lazily by `refreshEditAvailableModels`
/// — when the user opens the card we kick off a per-key
/// `fetch_models_for_key` call so the autocomplete dropdown shows
/// models that *this* upstream actually serves, instead of the ones
/// from whichever key happened to win the global fetch race.
const editForm = {
  value: "",
  label: "",
  expiresAt: "",
  provider: "nim" as ProviderKind,
  baseUrl: "",
  /// Each slot keeps both the upstream model ID *and* the slot's
  /// `extra_body` as a free-form JSON text. We store the raw text
  /// (rather than a parsed object) so half-written input survives
  /// modal re-renders without exploding on every keystroke; parsing
  /// happens at save time inside [`buildPerKeyModelMapping`].
  modelMapping: {
    defaultModel: "",
    defaultExtraBody: "",
    opusModel: "",
    opusExtraBody: "",
    sonnetModel: "",
    sonnetExtraBody: "",
    haikuModel: "",
    haikuExtraBody: "",
  },
  /// Which slot's `extra_body` editor is currently expanded. Tracked
  /// in form state (rather than via a CSS-only `<details>`) so the
  /// disclosure survives the partial re-renders that
  /// `renderEditModalRoot` triggers when models load, the provider
  /// changes, etc. Auto-opens when there's pre-existing content for
  /// a slot so users don't have to hunt for their own data.
  advancedExpanded: {
    default: false,
    opus: false,
    sonnet: false,
    haiku: false,
  },
  availableModels: [] as string[],
  modelsLoading: false,
  modelsError: null as string | null,
  /// Per-key rate-limit override as a free-text input. "" means
  /// "inherit the provider default"; a positive integer overrides it.
  rateLimit: "",
};

/// Updater state machine. The update plugin returns an `Update` handle
/// once a newer release is detected; we keep that handle around so the
/// "立即安装" button in the dashboard banner can act on it without a
/// second roundtrip to the GitHub endpoint.
type UpdateStage =
  | "idle"
  | "checking"
  | "available"
  | "downloading"
  /// Download is complete and the platform-specific installer has been
  /// spawned. On Windows specifically, the NSIS installer in
  /// `passive` mode starts trying to overwrite CCNim.exe within a
  /// few seconds and will *forcibly kill* the running process if it
  /// hasn't exited by then — which the user perceives as a sudden
  /// crash. We use this stage to (1) show a clear "应用即将自动重启"
  /// notice for ~800 ms, then (2) call `relaunch()` ourselves so the
  /// graceful shutdown path runs (proxy stops, listener socket
  /// released, in-flight requests drained) before the installer
  /// takes over the binary.
  | "installing"
  /// Fallback when auto-relaunch fails (rare). Surfaces a manual
  /// "立即重启" button.
  | "ready"
  | "error";
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
          // Don't go to "ready" here — on Windows the NSIS installer
          // (spawned in `passive` mode) will start trying to
          // overwrite CCNim.exe within seconds and forcibly kill our
          // process if we sit waiting for the user to click "重启".
          // That kill is what users perceived as a sudden crash.
          // Switch to the "installing" stage instead so the modal
          // shows a clear "正在安装，应用将自动重启" notice; the
          // post-resolve block below then triggers our graceful
          // exit so we beat the installer to the kill.
          updateState.stage = "installing";
          break;
      }
      render();
    });
    // Some platforms skip the `Finished` event and just resolve the
    // promise — make sure we're in "installing" either way.
    updateState.stage = "installing";
    render();
    // Brief, deliberate pause so the "installing/restarting" message
    // is actually readable before the window vanishes. 800 ms is
    // short enough that the NSIS installer hasn't usually reached
    // its "stop running instances" step yet on a typical Windows
    // box, so our `relaunch()` (which routes through the
    // `RunEvent::ExitRequested` graceful-shutdown path: stops the
    // proxy, releases the listener socket, drains in-flight
    // requests) wins the race against the installer's forced kill.
    // After we exit, NSIS overwrites the binary and re-launches it
    // via the NSIS template's built-in /RunAfter step.
    await new Promise((resolve) => window.setTimeout(resolve, 800));
    try {
      await relaunch();
      // `relaunch()` does not return on success: control transfers
      // out of the renderer when the process tears down. If we *do*
      // continue past this line, treat it as a soft failure and
      // give the user a manual restart affordance below.
    } catch (relaunchErr) {
      updateState.stage = "ready";
      updateState.error = `自动重启失败: ${relaunchErr}`;
      toast(
        `自动重启失败，请手动关闭并重新打开 CCNim 完成安装：${relaunchErr}`,
        "error",
      );
      render();
    }
  } catch (error) {
    updateState.stage = "error";
    updateState.error = String(error);
    toast(`下载/安装失败: ${error}`, "error");
    render();
  }
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

/// Tag the document with the host platform so any future per-platform
/// CSS tweaks have a hook. We no longer ship a custom titlebar —
/// every platform uses the system window chrome — so this is purely a
/// forward-looking selector and not load-bearing today.
function setupPlatformTag() {
  const platform = /Mac|iPhone|iPad/.test(navigator.userAgent) ? "macos" : "other";
  document.documentElement.dataset.platform = platform;
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

/// Refresh the small running-state pill embedded in the sidebar
/// header. Replaced the old custom-titlebar pill when we switched to
/// native window chrome — we re-render it on every status tick so the
/// indicator follows the proxy state without a full DOM rebuild. The
/// pill is rendered by `renderSidebarStatus()` and may be absent on
/// the first paint (before the sidebar mounts), so all queries are
/// null-safe.
function syncStatusPill() {
  const pill = document.getElementById("sbStatus");
  if (!pill) return;
  const text = pill.querySelector<HTMLSpanElement>(".sb-status-text");
  const url = pill.querySelector<HTMLSpanElement>(".sb-status-url");
  const running = proxyStatus?.running ?? false;
  const listenUrl = proxyStatus?.listen_url ?? "";
  pill.classList.toggle("online", running);
  pill.classList.toggle("offline", !running);
  pill.classList.toggle("has-url", running && !!listenUrl);
  if (text) text.textContent = running ? "运行中" : "未启动";
  if (url) url.textContent = running && listenUrl ? listenUrl : "";
}

async function load() {
  setupPlatformTag();
  installResetKeyDelegate();
  try {
    config = await invoke<AppConfig>("load_config");
    await loadAppVersion();
    await refreshStatus();
    render();
    syncStatusPill();
    if (statusTimer) window.clearInterval(statusTimer);
    statusTimer = window.setInterval(async () => {
      await refreshStatus();
      syncStatusPill();
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
  // Pre-validate so the user gets an immediate, page-aware toast
  // instead of round-tripping through Tauri only to be told the
  // backend rejected the payload. Mirrors `AppConfig::validate_for_save`
  // on the Rust side — keep them in lockstep.
  const validationError = validateConfigBeforeSave(config);
  if (validationError) {
    toast(validationError, "error");
    return false;
  }
  try {
    await invoke("save_config", { config });
    if (!silent) toast("配置已保存", "success");
    return true;
  } catch (error) {
    toast(`保存失败: ${error}`, "error");
    return false;
  }
}

/// Frontend mirror of `AppConfig::validate_for_save`. Returns a
/// user-facing error message when something is wrong, or `null`
/// when the config is safe to persist. Centralised so both the
/// "保存配置" button and the per-key edit / add flows pick up the
/// same checks without duplicating the rules at every call site.
function validateConfigBeforeSave(cfg: AppConfig): string | null {
  if (!cfg.model_mapping.default_model.trim()) {
    return "默认模型不能为空 — 请在「模型映射」页填写一个默认模型 ID 后再保存";
  }
  return null;
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
  render();
  syncStatusPill();
}

async function stopProxy() {
  try {
    await invoke("stop_proxy");
    toast("代理已停止", "info");
  } catch (error) {
    toast(`停止失败: ${error}`, "error");
  }
  await refreshStatus();
  render();
  syncStatusPill();
}

/// Silent background refresh of the upstream model list. Skips the call
/// if no API key is configured (the proxy would just bounce it). Only
/// re-renders when the list actually changed AND the user is currently
/// looking at the models page, so periodic ticks don't disturb other
/// workflows.
///
/// `provider` selects which upstream catalog to fetch. We default to
/// the first OpenAI-compatible provider the user has a key for so the
/// dropdown is populated regardless of whether NIM is in use; if the
/// only configured keys are Anthropic-compat we skip the fetch
/// (Anthropic upstreams don't expose a /v1/models endpoint).
async function fetchModelsAuto(provider?: ProviderKind) {
  if (!config || config.nim_api_keys.length === 0) return;
  const target =
    provider ??
    config.nim_api_keys
      .map((k) => (k.provider ?? "nim") as ProviderKind)
      .find((p) => p !== "anthropic_compat");
  if (!target || target === "anthropic_compat") return;
  try {
    const response = await invoke<{ data: Array<{ id: string }> }>("fetch_nim_models", {
      provider: target,
    });
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
 const report = await invoke<string>("open_claude_desktop");
        toast("配置文件已写入。\n\n后续步骤：\n1. 首次使用需要在 Claude Desktop 中启用开发者模式\n2. 打开 Claude Desktop → Help → Troubleshooting → Enable Developer Mode\n3. 重启 Claude Desktop\n4. 进入 Settings → Claude Code → Developer → Configure Third-party Inference\n5. 配置 Base URL 和 API Key，点击 Apply Locally", "success");
 } catch (error) {
 toast(`打开失败：${error}`, "error");
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
    singleAdd.provider = "nim";
    singleAdd.baseUrl = "";
  } else if (mode === "batch") {
    batchAdd.values = "";
    batchAdd.labelPrefix = "";
    batchAdd.expiresAt = "";
    batchAdd.provider = "nim";
    batchAdd.baseUrl = "";
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
  const valueError = validateKeyValue(value, singleAdd.provider);
  if (valueError) {
    toast(valueError, "error");
    return;
  }
  if (!hasUsableBaseUrl(singleAdd.provider, singleAdd.baseUrl)) {
    toast("请填写端点 URL（OpenAI 兼容供应商没有默认地址）", "error");
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
    provider: singleAdd.provider,
    base_url: singleAdd.baseUrl.trim().replace(/\/$/, ""),
    rate_limit: parseRateLimit(singleAdd.rateLimit),
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
  // Only meaningful for OpenAI-compatible providers — Anthropic-compat
  // hosts have no /v1/models so we skip the request rather than
  // surface an error.
  if (newKey.provider !== "anthropic_compat") {
    void fetchModelsAuto(newKey.provider);
  }
}

async function submitBatchImport() {
  if (!config) return;
  const candidates = batchAdd.values
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"));
  if (candidates.length === 0) {
    toast("请粘贴至少一行 Key", "error");
    return;
  }
  if (!hasUsableBaseUrl(batchAdd.provider, batchAdd.baseUrl)) {
    toast("请填写端点 URL（OpenAI 兼容供应商没有默认地址）", "error");
    return;
  }
  const invalid = candidates
    .map((c) => ({ raw: c, err: validateKeyValue(c, batchAdd.provider) }))
    .filter((x) => x.err !== null);
  if (invalid.length > 0) {
    toast(`有 ${invalid.length} 行 Key 校验失败：${invalid[0].err}，已中止导入`, "error");
    return;
  }
  const existing = new Set(config.nim_api_keys.map((k) => k.value));
  const sharedExpiry = datetimeLocalToUnix(batchAdd.expiresAt);
  const sharedLabel = batchAdd.labelPrefix.trim() || null;
  const sharedBaseUrl = batchAdd.baseUrl.trim().replace(/\/$/, "");
  const sharedRateLimit = parseRateLimit(batchAdd.rateLimit);
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
      provider: batchAdd.provider,
      base_url: sharedBaseUrl,
      rate_limit: sharedRateLimit,
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
  if (batchAdd.provider !== "anthropic_compat") {
    void fetchModelsAuto(batchAdd.provider);
  }
}

function beginEditKey(id: string) {
  if (!config) return;
  const key = config.nim_api_keys.find((k) => k.id === id);
  if (!key) return;
  editForm.value = key.value;
  editForm.label = key.label ?? "";
  editForm.expiresAt = unixToDatetimeLocal(key.expires_at);
  editForm.provider = key.provider ?? "nim";
  editForm.baseUrl = key.base_url ?? "";
  editForm.rateLimit =
    typeof key.rate_limit === "number" && key.rate_limit > 0 ? String(key.rate_limit) : "";
  if (key.model_mapping) {
    editForm.modelMapping = {
      defaultModel: key.model_mapping.default_model ?? "",
      defaultExtraBody: stringifyExtraBody(key.model_mapping.default_extra_body),
      opusModel: key.model_mapping.opus_model ?? "",
      opusExtraBody: stringifyExtraBody(key.model_mapping.opus_extra_body),
      sonnetModel: key.model_mapping.sonnet_model ?? "",
      sonnetExtraBody: stringifyExtraBody(key.model_mapping.sonnet_extra_body),
      haikuModel: key.model_mapping.haiku_model ?? "",
      haikuExtraBody: stringifyExtraBody(key.model_mapping.haiku_extra_body),
    };
  } else {
    editForm.modelMapping = {
      defaultModel: "",
      defaultExtraBody: "",
      opusModel: "",
      opusExtraBody: "",
      sonnetModel: "",
      sonnetExtraBody: "",
      haikuModel: "",
      haikuExtraBody: "",
    };
  }
  // Auto-expand any slot that already has an `extra_body` so the
  // user can see (and edit) their existing override on first paint
  // without an extra click.
  editForm.advancedExpanded = {
    default: editForm.modelMapping.defaultExtraBody.length > 0,
    opus: editForm.modelMapping.opusExtraBody.length > 0,
    sonnet: editForm.modelMapping.sonnetExtraBody.length > 0,
    haiku: editForm.modelMapping.haikuExtraBody.length > 0,
  };
  // Seed the dropdown with the global cache so it's never empty during
  // the round-trip; the per-key fetch below will replace it as soon as
  // the upstream responds.
  editForm.availableModels = models;
  editForm.modelsLoading = false;
  editForm.modelsError = null;
  editingKeyId = id;
  addPanel = null;
  // Mount the modal directly in its dedicated root — no need to
  // rebuild the entire app shell just to show a dialog.
  renderEditModalRoot();
  void refreshEditAvailableModels(id);
}

/// Pull the model catalog for a single configured key and stash it on
/// `editForm.availableModels`. Re-renders only the modal (not the
/// whole app) when the editor is still open for *this* key, so a slow
/// response that lands after the user already cancelled / switched
/// cards doesn't snap the UI back open and a response that lands
/// while the user is typing doesn't steal focus from the input.
async function refreshEditAvailableModels(keyId: string) {
  const key = config?.nim_api_keys.find((k) => k.id === keyId);
  if (!key) return;
  if ((key.provider ?? "nim") === "anthropic_compat") {
    editForm.availableModels = [];
    editForm.modelsLoading = false;
    editForm.modelsError =
      "Anthropic 兼容上游不暴露 /models，请直接手动填写模型 ID";
    if (editingKeyId === keyId) renderEditModalRoot();
    return;
  }
  editForm.modelsLoading = true;
  editForm.modelsError = null;
  if (editingKeyId === keyId) renderEditModalRoot();
  try {
    const response = await invoke<{ data: Array<{ id: string }> }>(
      "fetch_models_for_key",
      { keyId },
    );
    if (editingKeyId !== keyId) return;
    editForm.availableModels = response.data.map((m) => m.id).sort();
    editForm.modelsError = null;
  } catch (error) {
    if (editingKeyId !== keyId) return;
    editForm.modelsError = String(error);
  } finally {
    if (editingKeyId === keyId) {
      editForm.modelsLoading = false;
      renderEditModalRoot();
    }
  }
}

function cancelEdit() {
  editingKeyId = null;
  closeEditModal();
}

async function submitEditKey(id: string) {
  if (!config) return;
  const idx = config.nim_api_keys.findIndex((k) => k.id === id);
  if (idx < 0) return;
  const value = editForm.value.trim();
  const valueError = validateKeyValue(value, editForm.provider);
  if (valueError) {
    toast(valueError, "error");
    return;
  }
  if (!hasUsableBaseUrl(editForm.provider, editForm.baseUrl)) {
    toast("请填写端点 URL（OpenAI 兼容供应商没有默认地址）", "error");
    return;
  }
  if (config.nim_api_keys.some((k, i) => i !== idx && k.value === value)) {
    toast("已经存在另一个相同值的 Key", "error");
    return;
  }
  const mappingResult = buildPerKeyModelMapping(editForm.modelMapping);
  if (!mappingResult.ok) {
    toast(mappingResult.error, "error");
    return;
  }
  const before = config.nim_api_keys[idx];
  config.nim_api_keys[idx] = {
    ...before,
    value,
    label: editForm.label.trim() || null,
    expires_at: datetimeLocalToUnix(editForm.expiresAt),
    provider: editForm.provider,
    base_url: editForm.baseUrl.trim().replace(/\/$/, ""),
    model_mapping: mappingResult.mapping,
    rate_limit: parseRateLimit(editForm.rateLimit),
  };
  if (!(await save(true))) {
    config.nim_api_keys[idx] = before;
    return;
  }
  toast("已更新该 Key", "success");
  editingKeyId = null;
  // Close the modal first (no replay of the entrance animation since
  // it was mounted-once), then refresh the underlying keys grid so
  // the card reflects the new label / expiry / provider / mapping.
  closeEditModal();
  render();
}

/// Pretty-print an `extra_body` JSON object into the multi-line form
/// the textarea expects. Returns `""` when the input is null/undefined
/// or an empty object so the textarea starts empty (and the
/// auto-expand heuristic in `beginEditKey` can tell "user has data
/// here" apart from "slot is empty").
function stringifyExtraBody(value: Record<string, unknown> | null | undefined): string {
  if (value == null) return "";
  if (typeof value !== "object" || Array.isArray(value)) return "";
  if (Object.keys(value).length === 0) return "";
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return "";
  }
}

/// Parse outcome for an `extra_body` textarea: either a JSON object
/// ready to ship to the backend, the explicit `null` "user left it
/// blank, no override here", or an `error` carrying a human-readable
/// reason why the input couldn't be accepted.
type ExtraBodyParse =
  | { ok: true; value: Record<string, unknown> | null }
  | { ok: false; error: string };

/// Parse a single `extra_body` textarea into the wire format. Empty /
/// whitespace-only input is "no override" (`null`); otherwise we
/// require a JSON object — non-object values (`42`, `"hello"`,
/// `[1,2,3]`) are rejected with a clear message because they can't
/// be sensibly merged into a request body and the runtime would just
/// ignore them anyway. The slot label (e.g. "默认", "Opus") is
/// embedded in the error so the user knows which textarea to fix
/// when several have problems at once.
function parseExtraBodyField(raw: string, slotLabel: string): ExtraBodyParse {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return { ok: true, value: null };
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch (err) {
    return {
      ok: false,
      error: `${slotLabel} 的高级参数不是合法 JSON：${(err as Error).message}`,
    };
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return {
      ok: false,
      error: `${slotLabel} 的高级参数必须是 JSON 对象（{...}），收到的是 ${Array.isArray(parsed) ? "数组" : typeof parsed}`,
    };
  }
  if (Object.keys(parsed as Record<string, unknown>).length === 0) {
    return { ok: true, value: null };
  }
  return { ok: true, value: parsed as Record<string, unknown> };
}

/// Pack the per-slot model + extra_body fields back into a
/// `model_mapping` object, returning `undefined` when every slot is
/// blank. This is what makes the persisted JSON tidy: a key with no
/// overrides serialises as a plain credential record without a
/// redundant `"model_mapping": {}`.
///
/// Returns a `{ error }` envelope when any of the textareas contains
/// invalid JSON so `submitEditKey` can surface a precise toast and
/// abort the save before mutating the config.
type BuildMappingResult =
  | { ok: true; mapping: NimApiKey["model_mapping"] | undefined }
  | { ok: false; error: string };

function buildPerKeyModelMapping(form: {
  defaultModel: string;
  defaultExtraBody: string;
  opusModel: string;
  opusExtraBody: string;
  sonnetModel: string;
  sonnetExtraBody: string;
  haikuModel: string;
  haikuExtraBody: string;
}): BuildMappingResult {
  const trim = (s: string) => {
    const t = s.trim();
    return t.length === 0 ? null : t;
  };
  const slots = [
    { key: "default", label: "默认", model: trim(form.defaultModel), extraRaw: form.defaultExtraBody },
    { key: "opus", label: "Opus", model: trim(form.opusModel), extraRaw: form.opusExtraBody },
    { key: "sonnet", label: "Sonnet", model: trim(form.sonnetModel), extraRaw: form.sonnetExtraBody },
    { key: "haiku", label: "Haiku", model: trim(form.haikuModel), extraRaw: form.haikuExtraBody },
  ] as const;

  const mapping: NonNullable<NimApiKey["model_mapping"]> = {};
  let anySet = false;

  for (const slot of slots) {
    const parsed = parseExtraBodyField(slot.extraRaw, slot.label);
    if (!parsed.ok) return { ok: false, error: parsed.error };
    if (slot.model) {
      mapping[`${slot.key}_model` as const] = slot.model;
      anySet = true;
    }
    if (parsed.value) {
      mapping[`${slot.key}_extra_body` as const] = parsed.value;
      anySet = true;
    }
  }

  return { ok: true, mapping: anySet ? mapping : undefined };
}

/// Convert a free-form rate-limit input back into the wire shape:
///   - empty / non-numeric / ≤ 0 → `null` (use provider default)
///   - positive integer  → that integer
///
/// We deliberately collapse "" and bad input to `null` instead of
/// rejecting the form. The backend already treats `null` and `0` as
/// "inherit / unlimited", so this matches users' expectation that
/// "leave it blank" means "use whatever the default is".
function parseRateLimit(raw: string): number | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  const n = Number(trimmed);
  if (!Number.isFinite(n) || n <= 0 || !Number.isInteger(n)) return null;
  return n;
}

/// Placeholder text shown beneath the rate-limit input. Spelled out
/// per provider so the user knows whether leaving it blank means "use
/// the global NIM cap" or "no local cap at all".
function rateLimitPlaceholder(provider: ProviderKind): string {
  if (provider === "nim") {
    const def = config?.rate_limit_per_key ?? 40;
    return `留空 → 走 NIM 默认 ${def} 次 / ${rateLimitWindow()} 秒`;
  }
  return `留空 → 不限速（由上游配额决定）`;
}

function rateLimitWindow(): number {
  return config?.rate_window_secs ?? 60;
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
  if (editingKeyId === id) {
    editingKeyId = null;
    closeEditModal();
  }
  render();
}

async function toggleKeyEnabled(id: string, enabled: boolean) {
  if (!config) return;
  const idx = config.nim_api_keys.findIndex((k) => k.id === id);
  if (idx < 0) return;
  config.nim_api_keys[idx].enabled = enabled;
  if (!(await save(true))) {
    // Revert on failure
    config.nim_api_keys[idx].enabled = !enabled;
    toast("保存失败，已恢复原状态", "error");
    render();
    return;
  }
  toast(enabled ? "已启用" : "已禁用", enabled ? "success" : "info");
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

/// Pretty-print the seconds remaining in a cooldown as "Xm Ys" /
/// "Xs". We don't try to be clever about hours — the longest soft
/// cooldown the backend ever emits is 30 minutes, anything past that
/// has been promoted to `Disabled`.
function formatCooldown(secs: number): string {
  if (secs <= 0) return "即将恢复";
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  if (m === 0) return `${s}s`;
  if (s === 0) return `${m}m`;
  return `${m}m ${s}s`;
}

/// Optional inline status row appended under a key's mask: shows the
/// auto-recover countdown for `cooling_down` keys and a manual reset
/// button for both `cooling_down` and `disabled`. Returns `""` for
/// healthy / expired keys so the layout collapses cleanly.
function renderKeyStatusActions(snap: KeySnapshot): string {
  const norm = normalizeState(snap.state);
  if (norm !== "cooling_down" && norm !== "disabled") return "";
  const stableId = snap.stable_id ?? "";
  if (!stableId) return "";
  const cooldown = snap.cooldown_remaining_secs;
  const hint =
    norm === "cooling_down" && typeof cooldown === "number" && cooldown > 0
      ? `<span class="dash-status-hint">自动恢复约 ${escapeHtml(formatCooldown(cooldown))}</span>`
      : norm === "disabled"
        ? `<span class="dash-status-hint">连续认证失败已禁用</span>`
        : "";
  return `
    <div class="dash-status-actions">
      ${hint}
      <button class="btn-ghost btn-sm" type="button" data-reset-id="${escapeHtml(stableId)}" title="清除冷却 / 禁用，给该 Key 一次重试机会">重置状态</button>
    </div>
  `;
}

/// Tauri `reset_key` invocation. The backend pops the soft-cooldown /
/// Disabled flag and returns whether a matching key was found; we
/// surface that as a toast so the user knows the click had effect.
async function resetKeyById(stableId: string) {
  if (!stableId) return;
  try {
    const ok = await invoke<boolean>("reset_key", { stableId });
    if (ok) {
      toast("已重置状态，下次请求将再次尝试", "success");
      void refreshStatus();
    } else {
      toast("代理未运行或未找到对应 Key", "error");
    }
  } catch (err) {
    toast(`重置失败：${err}`, "error");
  }
}

/// Document-level click delegate for `data-reset-id` buttons. We
/// install it once at boot rather than re-binding inside per-render
/// helpers because the buttons live in two surfaces (dashboard table,
/// keys page card) that each have their own refresh cycle, and
/// per-render binding is what causes the duplicate-handler bugs we
/// already saw with the edit modal.
function installResetKeyDelegate() {
  document.addEventListener("click", (ev) => {
    const target = ev.target as HTMLElement | null;
    if (!target) return;
    const btn = target.closest("button[data-reset-id]") as HTMLButtonElement | null;
    if (!btn) return;
    ev.preventDefault();
    void resetKeyById(btn.dataset.resetId ?? "");
  });
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

/// True for syntactically plausible NIM API keys (`nvapi-` + ≥10 chars
/// of `[A-Za-z0-9_-]`). We deliberately keep this regex strict for the
/// NIM provider — pasting an OpenAI-shaped key into a NIM slot is a
/// configuration mistake that's worth catching at submit time.
function isValidNvapi(raw: string): boolean {
  return /^nvapi-[A-Za-z0-9_\-]{10,}$/.test(raw.trim());
}

/// Provider-aware key value validation. NIM keeps the strict
/// `nvapi-` regex; OpenAI and Anthropic compatible providers only
/// require a non-empty value (they accept too many bearer formats to
/// pin down a useful regex).
function validateKeyValue(raw: string, provider: ProviderKind): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return "Key 不能为空";
  if (provider === "nim" && !isValidNvapi(trimmed)) {
    return "NIM Key 必须以 nvapi- 开头";
  }
  return null;
}

/// Returns the base URL the proxy will actually use for a key — user
/// input if non-empty, otherwise the provider's canonical default.
function effectiveBaseUrl(provider: ProviderKind, baseUrl: string | undefined | null): string {
  const trimmed = (baseUrl ?? "").trim();
  if (trimmed) return trimmed.replace(/\/$/, "");
  return PROVIDERS[provider].defaultBaseUrl;
}

/// True if the form fields supply enough information for the backend
/// to actually talk to the upstream. `openai_compat` requires the
/// user to type a base URL because there is no canonical default.
function hasUsableBaseUrl(provider: ProviderKind, baseUrl: string | undefined | null): boolean {
  return effectiveBaseUrl(provider, baseUrl).length > 0;
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
        <div id="sbStatus" class="sb-status offline" title="代理状态">
          <span class="sb-status-dot"></span>
          <span class="sb-status-text">未启动</span>
          <span class="sb-status-url"></span>
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
  // Re-render the edit modal *after* the main app DOM exists, but
  // separately into its own root so a full render() (sidebar nav,
  // status poll, etc.) doesn't tear the modal down. Keeps the entrance
  // animation single-shot per session and preserves input focus.
  renderEditModalRoot();
  // The sidebar status pill is part of the just-rebuilt DOM; refresh
  // it immediately so the indicator doesn't flash "未启动" for a tick
  // before the next status poll comes back.
  syncStatusPill();
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
  } else if (stage === "installing") {
    title = "正在安装更新…";
    body = `
      <p>更新包已下载完成，安装程序已启动。</p>
      <p>应用将<strong>自动关闭并重新启动</strong>以完成安装，请稍候…</p>
      <p class="modal-notes">如果新版本未能自动启动，可以手动从开始菜单或桌面快捷方式重新打开 CCNim。</p>
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
  // closing would be destructive. `downloading` and `installing` are
  // both no-action stages where the user is meant to wait (the
  // installing stage is an ~800 ms window before we proactively
  // relaunch); swallowing the backdrop click in those states avoids
  // an accidental dismiss right as the modal does its job.
  byId<HTMLDivElement>("modalBackdrop")?.addEventListener("click", (e) => {
    if (e.target !== e.currentTarget) return;
    if (updateState.stage === "downloading" || updateState.stage === "installing") return;
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
  const metrics = proxyStatus?.metrics ?? null;

  const totalReq = metrics?.total_requests ?? 0;
  const totalOk = metrics?.total_successes ?? 0;
  const totalFail = metrics?.total_failures ?? 0;
  const totalIn = metrics?.total_input_tokens ?? 0;
  const totalOut = metrics?.total_output_tokens ?? 0;
  const successRate = totalReq > 0 ? `${((totalOk / totalReq) * 100).toFixed(1)}%` : "—";

  return `
    <header class="page-header">
      <div>
        <h1>仪表盘 ${updateState.currentVersion ? `<span class="version-pill">v${escapeHtml(updateState.currentVersion)}</span>` : ""}</h1>
        <p>本地 Anthropic 兼容代理 — 实时跟踪每个 Key 的用量、健康度与 Token 消耗。</p>
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
        <div class="metric-label">API Keys</div>
        <div class="metric-value">${healthyKeys}<span class="metric-sup"> / ${keyCount}</span></div>
        <div class="metric-sub">${keyCount === 0 ? "未配置 Key" : `${healthyKeys} 健康 · ${totalInflight} 并发`}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">累计请求</div>
        <div class="metric-value">${formatCount(totalReq)}</div>
        <div class="metric-sub">成功率 ${successRate} · 失败 ${formatCount(totalFail)}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">Token 消耗</div>
        <div class="metric-value">${formatTokenCount(totalIn + totalOut)}</div>
        <div class="metric-sub">输入 ${formatTokenCount(totalIn)} · 输出 ${formatTokenCount(totalOut)}</div>
      </div>
    </div>

    <section class="card">
      <div class="card-head">
        <div>
          <h2>API Key 用量</h2>
          <p>${
            isRunning
              ? "每个 Key 的并发、用量、Token 消耗与平均响应时间，自启动以来累计。"
              : "启动代理后这里会显示每个 Key 的实时用量与 Token 消耗。"
          }</p>
        </div>
        <button class="btn-ghost" data-nav="keys">前往管理 →</button>
      </div>
      <div id="dashKeyTable">${renderDashboardKeyTable(keys, metrics)}</div>
    </section>

    <section class="card">
      <div class="card-head">
        <div>
          <h2>模型调用排行</h2>
          <p>按调用次数倒序排列，统计代理启动以来每个上游模型的请求量与 Token 用量。</p>
        </div>
      </div>
      <div id="dashModelTable">${renderDashboardModelTable(metrics)}</div>
    </section>
  `;
}

/// Patch only the four metric tiles + the two tables on the dashboard.
/// Avoids a full `render()` per tick so the user's scroll position
/// survives across the polling cadence.
function refreshDashboardLiveSections() {
  if (!config) return;
  const isRunning = proxyStatus?.running ?? false;
  const keys = proxyStatus?.keys ?? [];
  const metrics = proxyStatus?.metrics ?? null;
  const totalReq = metrics?.total_requests ?? 0;
  const totalOk = metrics?.total_successes ?? 0;
  const totalFail = metrics?.total_failures ?? 0;
  const totalIn = metrics?.total_input_tokens ?? 0;
  const totalOut = metrics?.total_output_tokens ?? 0;
  const successRate = totalReq > 0 ? `${((totalOk / totalReq) * 100).toFixed(1)}%` : "—";
  const healthyKeys = keys.filter((k) => normalizeState(k.state) === "healthy").length;
  const totalInflight = keys.reduce((sum, k) => sum + k.inflight, 0);
  const keyCount = config.nim_api_keys.length;

  const grid = document.querySelector<HTMLDivElement>(".metric-grid");
  if (grid) {
    const listenUrl = proxyStatus?.listen_url || `http://${config.host}:${config.port}`;
    grid.innerHTML = `
      <div class="metric-card ${isRunning ? "metric-ok" : ""}">
        <div class="metric-label">代理状态</div>
        <div class="metric-value">${isRunning ? "运行中" : "未启动"}</div>
        <div class="metric-sub mono">${escapeHtml(listenUrl)}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">API Keys</div>
        <div class="metric-value">${healthyKeys}<span class="metric-sup"> / ${keyCount}</span></div>
        <div class="metric-sub">${keyCount === 0 ? "未配置 Key" : `${healthyKeys} 健康 · ${totalInflight} 并发`}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">累计请求</div>
        <div class="metric-value">${formatCount(totalReq)}</div>
        <div class="metric-sub">成功率 ${successRate} · 失败 ${formatCount(totalFail)}</div>
      </div>
      <div class="metric-card">
        <div class="metric-label">Token 消耗</div>
        <div class="metric-value">${formatTokenCount(totalIn + totalOut)}</div>
        <div class="metric-sub">输入 ${formatTokenCount(totalIn)} · 输出 ${formatTokenCount(totalOut)}</div>
      </div>
    `;
  }

  const keyTable = document.getElementById("dashKeyTable");
  if (keyTable) keyTable.innerHTML = renderDashboardKeyTable(keys, metrics);

  const modelTable = document.getElementById("dashModelTable");
  if (modelTable) modelTable.innerHTML = renderDashboardModelTable(metrics);
}

/// One row per configured key, joined against `metrics.keys` by stable
/// id. Keys with no traffic yet still appear, with zeros — that's the
/// signal the user uses to spot rotated-but-unused upstreams.
function renderDashboardKeyTable(
  keys: KeySnapshot[],
  metrics: MetricsSnapshot | null,
): string {
  if (keys.length === 0) {
    const configured = config?.nim_api_keys.length ?? 0;
    if (configured === 0) {
      return `<div class="empty"><strong>尚未配置 API Key</strong><br/>前往 "API Keys" 页面添加后这里会出现实时用量。</div>`;
    }
    return `<div class="empty"><strong>已配置 ${configured} 个 Key</strong><br/>启动代理后这里会显示每个 Key 的运行指标。</div>`;
  }
  const byId = new Map<string, KeyMetrics>();
  for (const m of metrics?.keys ?? []) byId.set(m.stable_id, m);
  const rateWindow = rateLimitWindow();

  const rows = keys
    .map((k) => {
      const m = byId.get(k.stable_id ?? "");
      const stateNorm = normalizeState(k.state);
      const provider = PROVIDERS[k.provider];
      const requests = m?.requests ?? 0;
      const successes = m?.successes ?? 0;
      const failures = m?.failures ?? 0;
      const inputTok = m?.input_tokens ?? 0;
      const outputTok = m?.output_tokens ?? 0;
      const avg = m?.avg_latency_ms ?? 0;
      const last = m?.last_latency_ms ?? 0;
      const ok = requests > 0 ? `${((successes / requests) * 100).toFixed(1)}%` : "—";
      const limit = typeof k.rate_limit === "number" && k.rate_limit > 0 ? k.rate_limit : null;
      const ratio = limit ? Math.min(1, k.recent_requests / limit) : 0;
      const fillCls = ratio >= 0.9 ? "danger" : ratio >= 0.6 ? "warn" : "";
      const widthPct = (ratio * 100).toFixed(1);
      const usageCell = limit
        ? `
            <div class="dash-usage-num">${k.recent_requests}<span class="dash-usage-denom">/${limit}</span></div>
            <div class="usage-bar-track tight"><div class="usage-bar-fill ${fillCls}" style="width:${widthPct}%"></div></div>
            <div class="dash-cell-sub">${rateWindow}s 窗口 · 并发 ${k.inflight}</div>`
        : `
            <div class="dash-usage-num">${k.recent_requests}<span class="dash-usage-denom"> 次</span></div>
            <div class="dash-cell-sub">不限速 · ${rateWindow}s 窗口 · 并发 ${k.inflight}</div>`;
      const labelCell = k.label
        ? `<div class="dash-key-label">${escapeHtml(k.label)}</div><div class="dash-key-mask mono">${escapeHtml(k.masked)}</div>`
        : `<div class="dash-key-mask mono">${escapeHtml(k.masked)}</div>`;
      return `
        <tr class="dash-row state-${stateNorm}">
          <td class="dash-cell-key">
            <div class="dash-key-head">
              <span class="key-num">#${k.id + 1}</span>
              <span class="badge badge-provider provider-${k.provider}" title="${escapeHtml(k.base_url)}">${escapeHtml(provider.short)}</span>
              ${keyStateBadge(k.state)}
            </div>
            ${labelCell}
            ${renderKeyStatusActions(k)}
          </td>
          <td>
            <div class="dash-usage">${usageCell}
            </div>
          </td>
          <td class="num">
            <div class="dash-cell-strong">${formatCount(requests)}</div>
            <div class="dash-cell-sub">成功率 ${ok}</div>
          </td>
          <td class="num">
            <div class="dash-cell-strong ${failures > 0 ? "fail" : ""}">${formatCount(failures)}</div>
            <div class="dash-cell-sub">连续失败 ${k.failure_count}</div>
          </td>
          <td class="num">
            <div class="dash-cell-strong">${avg > 0 ? formatLatency(avg) : "—"}</div>
            <div class="dash-cell-sub">最近 ${last > 0 ? formatLatency(last) : "—"}</div>
          </td>
          <td class="num">
            <div class="dash-cell-strong">${formatTokenCount(inputTok)}</div>
            <div class="dash-cell-sub">输入</div>
          </td>
          <td class="num">
            <div class="dash-cell-strong">${formatTokenCount(outputTok)}</div>
            <div class="dash-cell-sub">输出</div>
          </td>
        </tr>
      `;
    })
    .join("");

  return `
    <div class="dash-table-wrap">
      <table class="dash-table">
        <thead>
          <tr>
            <th>Key</th>
            <th>实时用量</th>
            <th class="num">请求</th>
            <th class="num">失败</th>
            <th class="num">响应时间</th>
            <th class="num">输入 Token</th>
            <th class="num">输出 Token</th>
          </tr>
        </thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  `;
}

function renderDashboardModelTable(metrics: MetricsSnapshot | null): string {
  if (!metrics || metrics.models.length === 0) {
    return `<div class="empty"><strong>暂无模型调用数据</strong><br/>${
      proxyStatus?.running
        ? "发起一次请求后，这里会按模型聚合调用次数与 Token 消耗。"
        : "启动代理并发起请求后，这里会显示模型调用排行。"
    }</div>`;
  }
  const rows = metrics.models
    .map((m, idx) => {
      const ok = m.calls > 0 ? `${((m.successes / m.calls) * 100).toFixed(1)}%` : "—";
      const total = m.input_tokens + m.output_tokens;
      const last = m.last_used_at > 0 ? formatRelativeTime(m.last_used_at) : "—";
      return `
        <tr>
          <td class="dash-rank">${idx + 1}</td>
          <td class="dash-model mono selectable">${escapeHtml(m.model)}</td>
          <td class="num"><div class="dash-cell-strong">${formatCount(m.calls)}</div><div class="dash-cell-sub">成功率 ${ok}</div></td>
          <td class="num"><div class="dash-cell-strong">${formatTokenCount(m.input_tokens)}</div></td>
          <td class="num"><div class="dash-cell-strong">${formatTokenCount(m.output_tokens)}</div></td>
          <td class="num"><div class="dash-cell-strong">${formatTokenCount(total)}</div></td>
          <td class="dash-cell-sub">${escapeHtml(last)}</td>
        </tr>
      `;
    })
    .join("");
  return `
    <div class="dash-table-wrap">
      <table class="dash-table">
        <thead>
          <tr>
            <th class="dash-rank">#</th>
            <th>模型</th>
            <th class="num">调用</th>
            <th class="num">输入 Token</th>
            <th class="num">输出 Token</th>
            <th class="num">合计 Token</th>
            <th>最近使用</th>
          </tr>
        </thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  `;
}

/// Compact human formatting for large request counts.
function formatCount(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "0";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 10_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toLocaleString("en-US");
}

function formatTokenCount(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toLocaleString("en-US");
}

function formatLatency(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

function formatRelativeTime(unix: number): string {
  if (!unix) return "—";
  const diff = Math.floor(Date.now() / 1000) - unix;
  if (diff < 5) return "刚刚";
  if (diff < 60) return `${diff} 秒前`;
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  const date = new Date(unix * 1000);
  return `${date.getMonth() + 1}-${date.getDate()} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
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
        <div><h2>限流策略</h2><p>窗口长度与 NIM 默认配额。每个 Key 都可以在「API Keys」页面单独覆盖；OpenAI / Anthropic 兼容上游默认不限速，需要时同样可以在 Key 上手填。</p></div>
      </div>
      <div class="form-grid two">
        <label class="field"><span>NIM 默认每 Key 限流 <em class="hint-inline">官方上限 40 / 分钟</em></span><input id="rateLimit" type="number" min="1" value="${config.rate_limit_per_key}" /></label>
        <label class="field"><span>窗口秒数 <em class="hint-inline">建议 60</em></span><input id="rateWindow" type="number" min="1" value="${config.rate_window_secs}" /></label>
      </div>
    </section>
  `;
}

function renderKeys(): string {
  if (!config) return "";
  const rateWindow = rateLimitWindow();
  const nimDefault = config.rate_limit_per_key;
  const snapshots = proxyStatus?.keys ?? [];
  // Per-provider count — gives the user a quick sanity check that
  // keys actually got tagged with the protocol they intended.
  const providerCounts = config.nim_api_keys.reduce<Record<ProviderKind, number>>(
    (acc, k) => {
      const p = (k.provider ?? "nim") as ProviderKind;
      acc[p] = (acc[p] ?? 0) + 1;
      return acc;
    },
    { nim: 0, openai_compat: 0, anthropic_compat: 0 },
  );
  const providerSummary = PROVIDER_KINDS.filter((k) => providerCounts[k] > 0)
    .map((k) => `${PROVIDERS[k].short} × ${providerCounts[k]}`)
    .join(" · ");
  return `
    <header class="page-header">
      <div>
        <h1>API Keys</h1>
        <p>逐个添加或批量导入上游 API Key，每个 Key 可独立选择端点类型（NIM / OpenAI 兼容 / Anthropic 兼容）和上游 URL。运行时按健康度、并发与最近请求自动切换，跨端点统一调度。</p>
      </div>
      <div class="header-actions">
        ${proxyStatus?.running ? `<span class="hint-inline">编辑后立即生效，无需重启代理</span>` : ""}
      </div>
    </header>

    <div class="banner">
      <div class="banner-icon">${ICONS.bolt}</div>
      <div class="banner-text">
        <strong>分 Provider 限速：NIM 默认 <span class="mono">${nimDefault}</span> 次 / <span class="mono">${rateWindow}</span> 秒，OpenAI / Anthropic 兼容默认不限</strong>
        <p>当前共配置 <strong>${config.nim_api_keys.length}</strong> 个 Key${providerSummary ? `（${providerSummary}）` : ""}。每个 Key 的「限流」字段可单独覆盖默认值，留空则按所属 provider 的默认策略走。已过期、被上游禁用或冷却中的 Key 不参与轮询。</p>
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

/// HTML fragment for picking a provider. Renders as a row of pill
/// buttons (rather than a `<select>`) so the user can see all three
/// options at once and read the description of the active one.
function providerPicker(idPrefix: string, value: ProviderKind): string {
  const meta = PROVIDERS[value];
  const buttons = PROVIDER_KINDS.map((kind) => {
    const m = PROVIDERS[kind];
    const active = kind === value ? "active" : "";
    return `<button type="button" class="provider-pill ${active}" data-provider-pick="${idPrefix}" data-provider="${kind}">${escapeHtml(m.label)}</button>`;
  }).join("");
  return `
    <div class="provider-picker" data-provider-id="${idPrefix}">
      <div class="provider-pill-row">${buttons}</div>
      <div class="provider-desc">${escapeHtml(meta.description)}</div>
    </div>
  `;
}

function renderSingleAddForm(): string {
  const meta = PROVIDERS[singleAdd.provider];
  const keyHint =
    singleAdd.provider === "nim" ? "必填，nvapi- 开头" : "必填，原样填写上游签发的 Key";
  return `
    <div class="add-form">
      <label class="field">
        <span>端点类型 <em class="hint-inline">选择上游协议</em></span>
        ${providerPicker("addProvider", singleAdd.provider)}
      </label>
      <label class="field">
        <span>端点 URL <em class="hint-inline">${meta.defaultBaseUrl ? "可留空使用默认" : "必填"}</em></span>
        <input id="addBaseUrl" type="text" placeholder="${escapeHtml(meta.placeholder)}" value="${escapeHtml(singleAdd.baseUrl)}" />
      </label>
      <label class="field">
        <span>API Key 值 <em class="hint-inline">${keyHint}</em></span>
        <div class="input-group">
          <input id="addValue" type="${showToken ? "text" : "password"}" autocomplete="off" placeholder="${escapeHtml(meta.keyPlaceholder)}" value="${escapeHtml(singleAdd.value)}" />
          <button id="addToggle" class="btn-icon" type="button" aria-label="切换显示" title="${showToken ? "隐藏" : "显示"}">${showToken ? ICONS.eyeOff : ICONS.eye}</button>
        </div>
      </label>
      <div class="form-grid two">
        <label class="field"><span>备注 <em class="hint-inline">可选</em></span><input id="addLabel" placeholder="例如：主账号 / dev" value="${escapeHtml(singleAdd.label)}" /></label>
        <label class="field">
          <span>到期时间 <em class="hint-inline">可选 · 留空表示永不过期</em></span>
          <div class="input-group">
            <input id="addExpiry" type="datetime-local" ${singleAdd.expiresAt ? `value="${escapeHtml(singleAdd.expiresAt)}"` : ""} />
            <button id="addExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
      <label class="field">
        <span>限流（次 / ${rateLimitWindow()} 秒）<em class="hint-inline">${escapeHtml(rateLimitPlaceholder(singleAdd.provider))}</em></span>
        <input id="addRateLimit" type="number" min="0" inputmode="numeric" placeholder="${escapeHtml(rateLimitPlaceholder(singleAdd.provider))}" value="${escapeHtml(singleAdd.rateLimit)}" />
      </label>
      <div class="form-actions">
        <button id="addCancel" class="btn-ghost" type="button">取消</button>
        <button id="addSubmit" class="btn-primary" type="button">添加</button>
      </div>
    </div>
  `;
}

function renderBatchAddForm(): string {
  const meta = PROVIDERS[batchAdd.provider];
  const placeholderKey = meta.keyPlaceholder;
  const placeholder = `${placeholderKey}\n${placeholderKey.replace("xxx", "yyy")}\n# 这一行会被忽略`;
  return `
    <div class="add-form">
      <label class="field">
        <span>端点类型 <em class="hint-inline">本批次 Key 共用此协议</em></span>
        ${providerPicker("batchProvider", batchAdd.provider)}
      </label>
      <label class="field">
        <span>端点 URL <em class="hint-inline">${meta.defaultBaseUrl ? "可留空使用默认" : "必填"}</em></span>
        <input id="batchBaseUrl" type="text" placeholder="${escapeHtml(meta.placeholder)}" value="${escapeHtml(batchAdd.baseUrl)}" />
      </label>
      <label class="field">
        <span>批量 API Key <em class="hint-inline">每行一个，# 开头视为注释</em></span>
        <textarea id="batchValues" rows="6" class="keys-textarea" placeholder="${escapeHtml(placeholder)}">${escapeHtml(batchAdd.values)}</textarea>
      </label>
      <div class="form-grid two">
        <label class="field"><span>共享备注 <em class="hint-inline">可选 · 应用到所有导入项</em></span><input id="batchLabel" placeholder="例如：team-a" value="${escapeHtml(batchAdd.labelPrefix)}" /></label>
        <label class="field">
          <span>共享到期时间 <em class="hint-inline">可选 · 应用到所有导入项</em></span>
          <div class="input-group">
            <input id="batchExpiry" type="datetime-local" ${batchAdd.expiresAt ? `value="${escapeHtml(batchAdd.expiresAt)}"` : ""} />
            <button id="batchExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
      <label class="field">
        <span>共享限流（次 / ${rateLimitWindow()} 秒）<em class="hint-inline">${escapeHtml(rateLimitPlaceholder(batchAdd.provider))}</em></span>
        <input id="batchRateLimit" type="number" min="0" inputmode="numeric" placeholder="${escapeHtml(rateLimitPlaceholder(batchAdd.provider))}" value="${escapeHtml(batchAdd.rateLimit)}" />
      </label>
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
  const masked = maskKey(k.value);
  const stateBadge = snap ? keyStateBadge(snap.state) : `<span class="badge badge-neutral">未运行</span>`;
  const stateNorm = snap ? normalizeState(snap.state) : "neutral";
  const expiry = formatExpiry(k.expires_at);
  const rateWindow = rateLimitWindow();
  // Prefer the snapshot value (already resolved by the backend) so a
  // user editing a key without restarting the proxy still sees the
  // *previous* effective limit until they save; fall back to the
  // unsaved config + provider default for cards that haven't reached
  // the live pool yet.
  const limit =
    typeof snap?.rate_limit === "number" && snap.rate_limit > 0
      ? snap.rate_limit
      : effectiveRateLimitForKey(k);
  const recent = snap?.recent_requests ?? 0;
  const ratio = limit ? Math.min(1, recent / limit) : 0;
  const fillCls = ratio >= 0.9 ? "danger" : ratio >= 0.6 ? "warn" : "";
  const widthPct = (ratio * 100).toFixed(1);
  const provider = (k.provider ?? "nim") as ProviderKind;
  const providerMeta = PROVIDERS[provider];
  const baseUrl = effectiveBaseUrl(provider, k.base_url);
  const providerBadge = `<span class="badge badge-provider provider-${provider}">${escapeHtml(providerMeta.short)}</span>`;
  return `
    <div class="key-card managed state-${stateNorm} expiry-${expiry.tone}">
      <div class="key-card-head">
        <div class="key-id-row">
          <span class="key-num">#${index + 1}</span>
          ${providerBadge}
        ${stateBadge}
        <label class="toggle-switch" title="启用/禁用此 Key">
          <input type="checkbox" class="key-enabled-toggle" data-toggle-id="${escapeHtml(k.id)}" ${k.enabled ? 'checked' : ''} />
          <span class="toggle-slider"></span>
        </label>
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
      <div class="key-endpoint-row" title="${escapeHtml(baseUrl)}">
        <span class="key-endpoint-label">端点</span>
        <code class="key-endpoint selectable">${escapeHtml(baseUrl)}</code>
      </div>
      <dl class="key-attrs">
        <div><dt>备注</dt><dd>${k.label ? escapeHtml(k.label) : '<span class="muted">未填写</span>'}</dd></div>
        <div><dt>到期</dt><dd class="expiry expiry-${expiry.tone}">${escapeHtml(expiry.text)}</dd></div>
      </dl>
      <div class="usage-bar">
        <div class="usage-bar-head">
          <span>最近 ${rateWindow}s 用量</span>
          <span class="usage-fraction">${recent}${limit ? ` / ${limit}` : "（不限速）"}</span>
        </div>
        ${
          limit
            ? `<div class="usage-bar-track"><div class="usage-bar-fill ${fillCls}" style="width:${widthPct}%"></div></div>`
            : ""
        }
      </div>
      <dl class="key-meta">
        <div><dt>并发</dt><dd>${snap?.inflight ?? 0}</dd></div>
        <div><dt>失败</dt><dd class="${(snap?.failure_count ?? 0) > 0 ? "fail" : ""}">${snap?.failure_count ?? 0}</dd></div>
      </dl>
      ${snap ? renderKeyStatusActions(snap) : ""}
    </div>
  `;
}

/// Mirror of the Rust-side `NimApiKey::effective_rate_limit` so the
/// Keys page can render the right cap without waiting for the snapshot
/// to round-trip through the proxy. Stays in sync with that helper —
/// any change there should be reflected here.
function effectiveRateLimitForKey(k: NimApiKey): number | null {
  if (typeof k.rate_limit === "number" && k.rate_limit > 0) return k.rate_limit;
  const provider = (k.provider ?? "nim") as ProviderKind;
  if (provider === "nim") return config?.rate_limit_per_key ?? 40;
  return null;
}

/// Render (or update) the edit-key modal in its dedicated
/// `#editModalRoot` mount point. Splits the work in two:
///
///   - **First mount** (when no modal was previously visible): write
///     the full `.modal-backdrop > .modal` markup so the CSS entrance
///     animation plays exactly once, and attach the page-level
///     listeners (Esc, backdrop click).
///   - **In-place update** (when the modal was already mounted, e.g.
///     after a provider switch or after the per-key model fetch
///     settled): rewrite only the modal's inner contents, preserving
///     keyboard focus and the input's selection range so the user's
///     typing isn't interrupted. The backdrop and modal wrappers
///     stay attached, so the entrance animation does NOT replay —
///     fixing the long-standing "flicker on every edit" bug.
///
/// `editingKeyId === null` (or the key was deleted out from under us)
/// closes the modal cleanly via `closeEditModal`.
function renderEditModalRoot() {
  if (editingKeyId === null || !config) {
    closeEditModal();
    return;
  }
  const idx = config.nim_api_keys.findIndex((x) => x.id === editingKeyId);
  if (idx < 0) {
    closeEditModal();
    return;
  }
  const k = config.nim_api_keys[idx];
  const body = renderEditCardBody(k, idx);
  if (!editModalMounted) {
    editModalRoot.innerHTML = `
      <div class="modal-backdrop" id="editBackdrop">
        <div class="modal modal-wide" role="dialog" aria-labelledby="editModalTitle" aria-modal="true" id="editModalDialog">
          ${body}
        </div>
      </div>
    `;
    editModalRoot.setAttribute("aria-hidden", "false");
    editModalMounted = true;
    attachEditModalGlobalListeners();
  } else {
    const dialog = document.getElementById("editModalDialog");
    if (dialog) {
      withFocusPreservation(dialog, () => {
        dialog.innerHTML = body;
      });
    }
  }
  bindEditModal();
}

/// Tear down the modal and detach the page-level listeners it owns.
/// Safe to call when nothing is mounted — both the innerHTML clear
/// and the listener detach are idempotent.
function closeEditModal() {
  if (!editModalMounted) {
    detachEditModalGlobalListeners();
    return;
  }
  editModalRoot.innerHTML = "";
  editModalRoot.setAttribute("aria-hidden", "true");
  editModalMounted = false;
  detachEditModalGlobalListeners();
}

/// Re-render the inner contents of `root` while preserving focus and
/// selection range on whichever element was active. Best-effort:
/// non-text inputs (`number`, `date`) reject `setSelectionRange`; we
/// silently fall back to "just refocus" for those.
function withFocusPreservation(root: HTMLElement, mutate: () => void) {
  const active = document.activeElement as HTMLElement | null;
  const focusedId = active && root.contains(active) && active.id ? active.id : null;
  let savedStart: number | null = null;
  let savedEnd: number | null = null;
  if (focusedId && active && "selectionStart" in active) {
    try {
      savedStart = (active as HTMLInputElement).selectionStart;
      savedEnd = (active as HTMLInputElement).selectionEnd;
    } catch {
      // Some input types throw on selection access — ignore.
    }
  }
  mutate();
  if (focusedId) {
    const next = document.getElementById(focusedId);
    if (next && typeof (next as HTMLInputElement).focus === "function") {
      (next as HTMLInputElement).focus();
      if (savedStart !== null && savedEnd !== null) {
        try {
          (next as HTMLInputElement).setSelectionRange(savedStart, savedEnd);
        } catch {
          // Same as above — non-text inputs reject selection range.
        }
      }
    }
  }
}

/// Form contents inside the edit modal. Kept as a separate function so
/// the modal wrapper can stay small and the body can be inserted into
/// other surfaces in the future without re-implementing the layout.
function renderEditCardBody(k: NimApiKey, index: number): string {
  const meta = PROVIDERS[editForm.provider];
  let mappingHint: string;
  if (editForm.modelsLoading) {
    mappingHint = "正在拉取该 Key 的模型目录…";
  } else if (editForm.modelsError) {
    mappingHint = `下拉建议不可用：${editForm.modelsError}（仍可手动输入）`;
  } else if (editForm.availableModels.length > 0) {
    mappingHint = `已加载 ${editForm.availableModels.length} 个上游模型，输入时自动过滤。留空字段会沿用上方"模型映射"页的全局配置。`;
  } else {
    mappingHint = '尚未拉取到模型列表，可以直接手动输入；留空字段会沿用全局"模型映射"配置。';
  }
  return `
    <header class="edit-modal-head">
      <h3 id="editModalTitle">编辑 Key <span class="key-num">#${index + 1}</span></h3>
      <button id="editClose" class="btn-icon" type="button" aria-label="关闭" title="关闭 (Esc)">×</button>
    </header>
    <div class="edit-modal-body">
      <label class="field">
        <span>端点类型</span>
        ${providerPicker("editProvider", editForm.provider)}
      </label>
      <label class="field">
        <span>端点 URL <em class="hint-inline">${meta.defaultBaseUrl ? "可留空使用默认" : "必填"}</em></span>
        <input id="editBaseUrl" type="text" placeholder="${escapeHtml(meta.placeholder)}" value="${escapeHtml(editForm.baseUrl)}" />
      </label>
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
            <input id="editExpiry" type="datetime-local" ${editForm.expiresAt ? `value="${escapeHtml(editForm.expiresAt)}"` : ""} />
            <button id="editExpiryClear" class="btn-icon" type="button" aria-label="清除到期" title="清除">×</button>
          </div>
        </label>
      </div>
      <label class="field">
        <span>限流（次 / ${rateLimitWindow()} 秒）<em class="hint-inline">${escapeHtml(rateLimitPlaceholder(editForm.provider))}</em></span>
        <input id="editRateLimit" type="number" min="0" inputmode="numeric" placeholder="${escapeHtml(rateLimitPlaceholder(editForm.provider))}" value="${escapeHtml(editForm.rateLimit)}" />
      </label>
      <div class="edit-mapping">
        <div class="edit-mapping-head">
          <span>该 Key 的模型映射 <em class="hint-inline">留空 → 沿用全局</em></span>
          <button id="editMappingRefresh" type="button" class="btn-ghost btn-sm" ${editForm.modelsLoading ? "disabled" : ""}>${editForm.modelsLoading ? "刷新中…" : "重新拉取"}</button>
        </div>
        <p class="edit-mapping-hint">${escapeHtml(mappingHint)}</p>
        <div class="edit-mapping-rows">
          ${editMappingRow("default", "默认", editForm.modelMapping.defaultModel, editForm.modelMapping.defaultExtraBody, "默认走全局")}
          ${editMappingRow("opus", "Opus", editForm.modelMapping.opusModel, editForm.modelMapping.opusExtraBody, "走默认")}
          ${editMappingRow("sonnet", "Sonnet", editForm.modelMapping.sonnetModel, editForm.modelMapping.sonnetExtraBody, "走默认")}
          ${editMappingRow("haiku", "Haiku", editForm.modelMapping.haikuModel, editForm.modelMapping.haikuExtraBody, "走默认")}
        </div>
      </div>
    </div>
    <footer class="edit-modal-actions">
      <button id="editCancel" class="btn-ghost" type="button">取消</button>
      <button id="editSave" class="btn-primary" type="button" data-edit-save="${escapeHtml(k.id)}">保存</button>
    </footer>
  `;
}

type MappingSlotKey = "default" | "opus" | "sonnet" | "haiku";

/// One row in the per-key mapping editor: the upstream model
/// combobox plus a collapsible "高级" disclosure that reveals an
/// `extra_body` JSON textarea. Rendered as a vertical stack rather
/// than the old 2-column grid because the textarea needs the full
/// modal width to be useful, and tucking each one under its own row
/// is the most compact way to keep the model-to-extras mapping
/// visually 1:1.
function editMappingRow(
  slot: MappingSlotKey,
  label: string,
  modelValue: string,
  extraBodyValue: string,
  fallback: string,
): string {
  const inputId = `edit${slot[0].toUpperCase() + slot.slice(1)}Model`;
  const textareaId = `edit${slot[0].toUpperCase() + slot.slice(1)}ExtraBody`;
  const expanded = editForm.advancedExpanded[slot];
  const hasExtras = extraBodyValue.trim().length > 0;
  return `
    <div class="edit-mapping-row">
      <label class="field">
        <span>${escapeHtml(label)}</span>
        <div class="model-combobox">
          <input id="${inputId}" class="model-combobox-input" autocomplete="off"
            placeholder="留空 → ${escapeHtml(fallback)}"
            value="${escapeHtml(modelValue)}"
            data-mapping-field="${inputId}" />
          <div class="model-combobox-dropdown" hidden></div>
        </div>
      </label>
      <button type="button"
        class="edit-mapping-advanced-toggle ${expanded ? "is-open" : ""} ${hasExtras ? "has-content" : ""}"
        data-advanced-toggle="${slot}"
        aria-expanded="${expanded ? "true" : "false"}"
        aria-controls="${textareaId}-wrap"
        title="${hasExtras ? "已配置 extra_body" : "为该映射配置 extra_body"}">
        <span class="edit-mapping-advanced-label">高级 / extra_body${hasExtras ? " ●" : ""}</span>
        <svg class="caret" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="6 9 12 15 18 9"/></svg>
      </button>
      <div class="edit-mapping-advanced ${expanded ? "is-open" : ""}" id="${textareaId}-wrap" ${expanded ? "" : "hidden"}>
        <textarea id="${textareaId}" class="extra-body-input" spellcheck="false" autocomplete="off"
          data-extra-body-field="${slot}"
          placeholder='{
  "temperature": 0.7,
  "top_p": 0.95,
  "chat_template_kwargs": { "thinking": true }
}'>${escapeHtml(extraBodyValue)}</textarea>
        <p class="extra-body-hint">JSON 对象。命中该映射的请求会把这里的字段深度合并到上游请求体里，<strong>覆盖</strong>客户端传入的同名字段。留空表示不注入。</p>
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
  // When every configured key is Anthropic-compat we can't auto-fetch
  // a catalog (no /v1/models endpoint upstream), so the dropdown
  // becomes a free-text input. Make that explicit so users don't
  // wonder why nothing's loading.
  const onlyAnthropic =
    hasKeys &&
    config.nim_api_keys.every(
      (k) => ((k.provider ?? "nim") as ProviderKind) === "anthropic_compat",
    );
  let modelsHint: string;
  if (models.length > 0) {
    modelsHint = `已加载 ${models.length} 个上游模型，每 30 分钟自动刷新一次。可点击下拉或直接输入过滤。Anthropic 兼容端点不会出现在此处，请直接手动输入模型 ID。`;
  } else if (!hasKeys) {
    modelsHint = "尚未配置 API Key，无法拉取模型列表。先在「API Keys」页面添加 Key 后软件会自动获取。";
  } else if (onlyAnthropic) {
    modelsHint =
      "当前仅配置 Anthropic 兼容 Key — 这类上游不暴露模型目录，请手动填写模型 ID（例如 claude-sonnet-4-5、glm-4.6、deepseek-chat 等）。";
  } else {
    modelsHint = "正在自动拉取上游模型列表（首次启动可能需要数秒）。也可以手动输入模型 ID。";
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
 <button id="openClaudeDesktop" class="btn-secondary" ${proxyStatus?.running ? "" : "disabled title='请先启动代理'"}>配置 Claude Desktop</button>
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

  if (activeView === "dashboard") {
    // Patch just the dynamic dashboard sections in-place so the user's
    // scroll position is preserved across the 3 s polling tick. The
    // `<header>` controls (start / stop / 检查更新) are static given the
    // running state we already short-circuited on above, so they don't
    // need re-binding here.
    refreshDashboardLiveSections();
    return;
  }
  if (activeView === "keys") {
    const keyList = document.getElementById("keyList");
    if (!keyList) return;
    // The edit modal is rendered separately at the page root, so the
    // live card refresh below leaves it untouched even mid-edit.
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
  byId<HTMLButtonElement>("tokToggle")?.addEventListener("click", () => {
    showToken = !showToken;
    render();
  });

  bindKeysPage();
  bindIdePage();
  bindUpdateUi();

  bindCopyButtons();
  setupComboboxes();
  // The edit modal lives in its own root (`#editModalRoot`) — its
  // bindings (`bindEditModal`, `bindEditMappingComboboxes`) are
  // installed by `renderEditModalRoot` after every modal (re)render
  // and are intentionally NOT re-run here, so a full app re-render
  // doesn't pile duplicate listeners onto modal elements.
}

/// Per-edit-card mapping inputs. We can't reuse the global `combobox()`
/// helper because the source list is per-key (`editForm.availableModels`)
/// and changes whenever `refreshEditAvailableModels` lands. Each field
/// also has to write through to `editForm.modelMapping` so it survives
/// re-renders triggered by provider switches or model-fetch settles.
function bindEditMappingComboboxes() {
  const inputs = document.querySelectorAll<HTMLInputElement>(
    "input.model-combobox-input[data-mapping-field]",
  );
  if (inputs.length === 0) return;
  inputs.forEach((input) => {
    const dropdown = input.parentElement?.querySelector<HTMLDivElement>(
      ".model-combobox-dropdown",
    );
    if (!dropdown) return;

    const writeBack = (value: string) => {
      switch (input.id) {
        case "editDefaultModel":
          editForm.modelMapping.defaultModel = value;
          break;
        case "editOpusModel":
          editForm.modelMapping.opusModel = value;
          break;
        case "editSonnetModel":
          editForm.modelMapping.sonnetModel = value;
          break;
        case "editHaikuModel":
          editForm.modelMapping.haikuModel = value;
          break;
      }
    };

    const renderDropdown = () => {
      const query = input.value.trim().toLowerCase();
      const source = editForm.availableModels;
      const filtered = (
        query ? source.filter((m) => m.toLowerCase().includes(query)) : source
      ).slice(0, 30);
      if (filtered.length === 0) {
        dropdown.hidden = true;
        dropdown.innerHTML = "";
        return;
      }
      dropdown.innerHTML = filtered
        .map(
          (m) =>
            `<div class="model-combobox-item" data-value="${escapeHtml(m)}" title="${escapeHtml(m)}">${escapeHtml(m)}</div>`,
        )
        .join("");
      dropdown.hidden = false;
    };

    input.addEventListener("input", () => {
      writeBack(input.value);
      renderDropdown();
    });
    input.addEventListener("focus", renderDropdown);

    dropdown.addEventListener("mousedown", (event) => {
      const item = (event.target as HTMLElement).closest<HTMLDivElement>(
        ".model-combobox-item",
      );
      if (!item) return;
      event.preventDefault();
      const value = item.dataset.value ?? "";
      input.value = value;
      writeBack(value);
      dropdown.hidden = true;
    });

    input.addEventListener("blur", () => {
      window.setTimeout(() => {
        dropdown.hidden = true;
      }, 150);
    });
  });
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
  byId<HTMLInputElement>("addBaseUrl")?.addEventListener("input", (e) => {
    singleAdd.baseUrl = (e.target as HTMLInputElement).value;
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
  byId<HTMLInputElement>("addRateLimit")?.addEventListener("input", (e) => {
    singleAdd.rateLimit = (e.target as HTMLInputElement).value;
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
  byId<HTMLInputElement>("batchBaseUrl")?.addEventListener("input", (e) => {
    batchAdd.baseUrl = (e.target as HTMLInputElement).value;
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
  byId<HTMLInputElement>("batchRateLimit")?.addEventListener("input", (e) => {
    batchAdd.rateLimit = (e.target as HTMLInputElement).value;
  });
  byId<HTMLButtonElement>("batchCancel")?.addEventListener("click", closeAddPanel);
  byId<HTMLButtonElement>("batchSubmit")?.addEventListener("click", () => {
    void submitBatchImport();
  });

  // Provider pill bindings for the add panels. The modal's pills
  // (`data-provider-pick="editProvider"`) are wired up by
  // `bindEditModal` instead, so a click there only re-renders the
  // modal — the rest of the app shell stays untouched.
  document.querySelectorAll<HTMLButtonElement>("button[data-provider-pick]").forEach((btn) => {
    const target = btn.dataset.providerPick;
    if (target === "editProvider") return;
    btn.addEventListener("click", () => {
      const provider = btn.dataset.provider as ProviderKind | undefined;
      if (!target || !provider) return;
      if (target === "addProvider") {
        singleAdd.provider = provider;
        // If the user hadn't typed a custom URL, prefill the new
        // provider's default so the form is usable in one click.
        if (!singleAdd.baseUrl.trim()) {
          singleAdd.baseUrl = "";
        }
      } else if (target === "batchProvider") {
        batchAdd.provider = provider;
        if (!batchAdd.baseUrl.trim()) {
          batchAdd.baseUrl = "";
        }
      }
      render();
    });
  });

  bindManagedKeyControls();
}

/// Binds in-card buttons that live inside the keys grid (edit/delete
/// triggers + the enable/disable toggle). Safe to re-run after a
/// `keyList.innerHTML = ...` refresh because it only touches elements
/// rendered by `renderManagedKeyCard` — not the modal.
function bindManagedKeyControls() {
  document.querySelectorAll<HTMLButtonElement>("button[data-edit-id]").forEach((btn) => {
    btn.addEventListener("click", () => beginEditKey(btn.dataset.editId ?? ""));
  });
  document.querySelectorAll<HTMLButtonElement>("button[data-delete-id]").forEach((btn) => {
    btn.addEventListener("click", () => {
      void deleteKey(btn.dataset.deleteId ?? "");
    });
  });
  document.querySelectorAll<HTMLInputElement>("input.key-enabled-toggle").forEach((toggle) => {
    toggle.addEventListener("change", async () => {
      const id = toggle.dataset.toggleId ?? "";
      await toggleKeyEnabled(id, toggle.checked);
    });
  });
}

/// Wires up everything *inside* the edit modal: form inputs, header
/// close button, footer Cancel/Save, the per-key provider pills, the
/// per-key model-mapping comboboxes, and the password visibility
/// toggle. Called from `renderEditModalRoot` after every (re)render of
/// the modal contents.
///
/// Page-level listeners (Esc, backdrop click) are intentionally
/// installed by `attachEditModalGlobalListeners` instead — they only
/// need to be attached once per modal session, regardless of how many
/// times the inner contents are re-rendered.
function bindEditModal() {
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

  byId<HTMLInputElement>("editValue")?.addEventListener("input", (e) => {
    editForm.value = (e.target as HTMLInputElement).value;
  });
  byId<HTMLInputElement>("editBaseUrl")?.addEventListener("input", (e) => {
    editForm.baseUrl = (e.target as HTMLInputElement).value;
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
  byId<HTMLInputElement>("editRateLimit")?.addEventListener("input", (e) => {
    editForm.rateLimit = (e.target as HTMLInputElement).value;
  });

  // Password visibility: flip the input type and swap the icon
  // *in place* — no DOM rewrite, no re-render. Re-rendering the modal
  // here would steal focus from the input the user is currently
  // typing in, which is exactly the "shaking" the user reported.
  byId<HTMLButtonElement>("editToggle")?.addEventListener("click", () => {
    showToken = !showToken;
    const valueInput = byId<HTMLInputElement>("editValue");
    if (valueInput) valueInput.type = showToken ? "text" : "password";
    const toggleBtn = byId<HTMLButtonElement>("editToggle");
    if (toggleBtn) {
      toggleBtn.innerHTML = showToken ? ICONS.eyeOff : ICONS.eye;
      toggleBtn.title = showToken ? "隐藏" : "显示";
    }
  });

  byId<HTMLButtonElement>("editCancel")?.addEventListener("click", cancelEdit);
  byId<HTMLButtonElement>("editClose")?.addEventListener("click", cancelEdit);
  byId<HTMLButtonElement>("editSave")?.addEventListener("click", (e) => {
    const id = (e.currentTarget as HTMLButtonElement).dataset.editSave ?? "";
    void submitEditKey(id);
  });
  byId<HTMLButtonElement>("editMappingRefresh")?.addEventListener("click", () => {
    if (!editingKeyId) return;
    void refreshEditAvailableModels(editingKeyId);
  });

  // Per-key provider pills. Switching the provider re-renders only
  // the modal (focus-preserved) so the URL placeholder, description,
  // and rate-limit hint follow the new selection without the rest of
  // the app shell flickering.
  editModalRoot
    .querySelectorAll<HTMLButtonElement>('button[data-provider-pick="editProvider"]')
    .forEach((btn) => {
      btn.addEventListener("click", () => {
        const provider = btn.dataset.provider as ProviderKind | undefined;
        if (!provider || provider === editForm.provider) return;
        editForm.provider = provider;
        renderEditModalRoot();
      });
    });

  // The mapping inputs use a per-key model catalog (not the global
  // `models` array), so wire their dropdowns up here every time the
  // modal contents are (re-)rendered.
  bindEditMappingComboboxes();
  bindEditMappingAdvanced();
}

/// Wire up the per-slot "高级 / extra_body" disclosure toggles and
/// JSON textareas. Toggling a slot updates `editForm.advancedExpanded`
/// and animates the panel open/closed in place — no full re-render,
/// so the user's caret/scroll position in any other textarea is
/// preserved. The textareas write through to `editForm.modelMapping`
/// on every keystroke; parsing/validation happens later, at save
/// time, inside `buildPerKeyModelMapping`.
function bindEditMappingAdvanced() {
  editModalRoot
    .querySelectorAll<HTMLButtonElement>("button[data-advanced-toggle]")
    .forEach((btn) => {
      const slot = btn.dataset.advancedToggle as MappingSlotKey | undefined;
      if (!slot) return;
      btn.addEventListener("click", () => {
        editForm.advancedExpanded[slot] = !editForm.advancedExpanded[slot];
        const expanded = editForm.advancedExpanded[slot];
        // Toggle in place without a re-render so the textarea's
        // scroll/cursor state survives across collapse/expand.
        btn.classList.toggle("is-open", expanded);
        btn.setAttribute("aria-expanded", expanded ? "true" : "false");
        const wrapId = `${btn.getAttribute("aria-controls") ?? ""}`;
        const panel = wrapId ? document.getElementById(wrapId) : null;
        if (panel) {
          panel.classList.toggle("is-open", expanded);
          panel.hidden = !expanded;
          if (expanded) {
            const ta = panel.querySelector<HTMLTextAreaElement>(".extra-body-input");
            ta?.focus();
          }
        }
      });
    });

  editModalRoot
    .querySelectorAll<HTMLTextAreaElement>("textarea[data-extra-body-field]")
    .forEach((ta) => {
      const slot = ta.dataset.extraBodyField as MappingSlotKey | undefined;
      if (!slot) return;
      const writeBack = () => {
        const v = ta.value;
        switch (slot) {
          case "default":
            editForm.modelMapping.defaultExtraBody = v;
            break;
          case "opus":
            editForm.modelMapping.opusExtraBody = v;
            break;
          case "sonnet":
            editForm.modelMapping.sonnetExtraBody = v;
            break;
          case "haiku":
            editForm.modelMapping.haikuExtraBody = v;
            break;
        }
        // Cheap on-the-fly JSON validation: red border when the
        // textarea has content but parsing fails. Pure visual hint —
        // the authoritative validation runs at save time.
        const trimmed = v.trim();
        if (trimmed.length === 0) {
          ta.classList.remove("is-invalid", "is-valid");
          return;
        }
        try {
          const parsed = JSON.parse(trimmed);
          const isObject =
            parsed !== null && typeof parsed === "object" && !Array.isArray(parsed);
          ta.classList.toggle("is-invalid", !isObject);
          ta.classList.toggle("is-valid", isObject);
        } catch {
          ta.classList.add("is-invalid");
          ta.classList.remove("is-valid");
        }
      };
      ta.addEventListener("input", writeBack);
      // Run once on bind so the existing value gets its initial
      // valid/invalid styling without waiting for a keystroke.
      writeBack();
    });
}

/// Attach the page-level listeners the modal needs (Esc and
/// click-on-backdrop). Idempotent: detaches any previous Esc handler
/// before installing a fresh one so repeated re-mounts don't pile
/// listeners up. The backdrop click listener lives on `editBackdrop`,
/// which is only mounted once per modal session, so it doesn't need
/// the same defensive cleanup.
function attachEditModalGlobalListeners() {
  if (editModalEscHandler) {
    document.removeEventListener("keydown", editModalEscHandler, true);
    editModalEscHandler = null;
  }
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape" && editingKeyId !== null) {
      ev.preventDefault();
      cancelEdit();
    }
  };
  editModalEscHandler = onKey;
  document.addEventListener("keydown", onKey, true);

  const backdrop = document.getElementById("editBackdrop");
  backdrop?.addEventListener("click", (e) => {
    if (e.target === e.currentTarget) cancelEdit();
  });
}

function detachEditModalGlobalListeners() {
  if (editModalEscHandler) {
    document.removeEventListener("keydown", editModalEscHandler, true);
    editModalEscHandler = null;
  }
}

load();
