#![cfg_attr(
    all(not(debug_assertions), any(target_os = "windows", target_os = "macos")),
    windows_subsystem = "windows"
)]

mod ide_scan;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use app_config::AppConfig;
use ide_scan::{IdeApplyReport, IdeProfile};
use nim_client::{KeyPool, NimClient};
use proxy_server::{key_pool_entries, start_server, ProxyStatus, RunningServer};
use tauri::{Manager, RunEvent, State};

#[cfg(target_os = "windows")]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

struct AppState {
    server: Mutex<Option<RunningServer>>,
    /// Set the first time the user requests an exit. Subsequent
    /// `ExitRequested` events (for example because Tauri re-fires the
    /// event after our async cleanup task calls `app_handle.exit(0)`)
    /// must NOT call `prevent_exit` again — that would loop forever.
    shutting_down: AtomicBool,
}

#[tauri::command]
fn load_config() -> Result<AppConfig, String> {
    AppConfig::load_or_default().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config(config: AppConfig, state: State<'_, AppState>) -> Result<(), String> {
    config.save().map_err(|e| e.to_string())?;
    // If the proxy is currently running, hot-swap the key set so label/expiry
    // edits and add/remove operations take effect without a restart. Live
    // counters (inflight, recent_requests, failure_count) are preserved for
    // keys whose secret value is unchanged — see KeyPool::update_keys.
    if let Ok(guard) = state.server.lock() {
        if let Some(server) = guard.as_ref() {
            server.key_pool().update_keys(key_pool_entries(
                &config.nim_api_keys,
                config.rate_limit_per_key,
            ));
        }
    }
    Ok(())
}

#[tauri::command]
async fn start_proxy(state: State<'_, AppState>) -> Result<String, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let server = start_server(config).await.map_err(|e| e.to_string())?;
    let url = format!("http://{}", server.addr());
    let mut guard = state.server.lock().map_err(|_| "server lock poisoned")?;
    if let Some(existing) = guard.take() {
        tokio::spawn(existing.stop());
    }
    *guard = Some(server);
    Ok(url)
}

#[tauri::command]
async fn stop_proxy(state: State<'_, AppState>) -> Result<(), String> {
    let server = {
        let mut guard = state.server.lock().map_err(|_| "server lock poisoned")?;
        guard.take()
    };
    if let Some(server) = server {
        server.stop().await;
    }
    Ok(())
}

#[tauri::command]
fn proxy_status(state: State<'_, AppState>) -> Result<ProxyStatus, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;

    // When the proxy is up, surface snapshots from the *live* KeyPool so the
    // dashboard reflects real inflight/recent counts and health transitions.
    // When stopped, fall back to a fresh pool so the user still sees the keys
    // they have configured. The metrics registry only exists while the
    // proxy is running (counters are intentionally per-run), so we send
    // `None` in the stopped case and the dashboard hides the live cards.
    let guard = state.server.lock().map_err(|_| "server lock poisoned")?;
    let (running, keys, metrics) = match guard.as_ref() {
        Some(server) => (
            true,
            server.key_pool().snapshots(),
            Some(server.metrics_snapshot()),
        ),
        None => {
            let pool = KeyPool::new(
                key_pool_entries(&config.nim_api_keys, config.rate_limit_per_key),
                std::time::Duration::from_secs(config.rate_window_secs),
            );
            (false, pool.snapshots(), None)
        }
    };
    drop(guard);

    Ok(ProxyStatus {
        running,
        listen_url: format!("http://{}:{}", config.host, config.port),
        default_model: config.model_mapping.default_model,
        keys,
        metrics,
    })
}

#[tauri::command]
fn scan_ides() -> Vec<IdeProfile> {
    ide_scan::scan_ides()
}

#[tauri::command]
fn apply_ide_settings(ide_id: String) -> Result<IdeApplyReport, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let base_url = format!("http://{}:{}", config.host, config.port);
    let report = ide_scan::apply_settings(&ide_id, &base_url, &config.auth_token)?;
    tracing::info!(
        ide = %ide_id,
        path = %report.settings_path,
        backup = ?report.backup_path,
        comments_stripped = report.comments_stripped,
        created = report.created,
        "wrote claudeCode.environmentVariables"
    );
    Ok(report)
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
async fn fetch_nim_models(
    provider: Option<proxy_core::ProviderKind>,
) -> Result<proxy_core::NimModelList, String> {
    let provider = provider.unwrap_or(proxy_core::ProviderKind::Nim);
    if matches!(provider, proxy_core::ProviderKind::AnthropicCompat) {
        return Err("Anthropic 兼容上游不暴露模型目录，请在端点配置中手动填写模型名".to_string());
    }
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let key_pool = KeyPool::new(
        key_pool_entries(&config.nim_api_keys, config.rate_limit_per_key),
        std::time::Duration::from_secs(config.rate_window_secs),
    );
    let client = NimClient::new(key_pool).map_err(|e| e.to_string())?;
    client
        .list_models(provider)
        .await
        .map_err(|e| e.to_string())
}

/// Fetch the model catalog for a single configured key, addressed by
/// its stable `id`. Lets the GUI populate the autocomplete dropdown
/// inside the edit-key card with models that *that specific upstream*
/// actually exposes, instead of mixing entries from every configured
/// key together. Anthropic-compatible keys (which have no `/models`
/// endpoint) return an explicit, user-readable error.
#[tauri::command]
async fn fetch_models_for_key(key_id: String) -> Result<proxy_core::NimModelList, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let key = config
        .nim_api_keys
        .iter()
        .find(|k| k.id == key_id)
        .ok_or_else(|| format!("找不到 ID 为 {key_id} 的 Key"))?;
    if matches!(key.provider, proxy_core::ProviderKind::AnthropicCompat) {
        return Err("Anthropic 兼容上游不暴露 /models，请直接手动填写模型名".to_string());
    }
    // We always create a fresh, throw-away pool here — the call must not
    // rely on the live runtime pool because the user may be editing a
    // key whose value just changed (live pool would still hold the
    // previous credential), or the proxy may not be running at all.
    let key_pool = KeyPool::new(Vec::new(), std::time::Duration::from_secs(60));
    let client = NimClient::new(key_pool).map_err(|e| e.to_string())?;
    client
        .list_models_direct(key.provider, &key.effective_base_url(), &key.value)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_claude_terminal(cwd: String) -> Result<(), String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let base_url = format!("http://{}:{}", config.host, config.port);
    let cwd = if cwd.trim().is_empty() {
        user_home().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    open_terminal_with_claude(&cwd, &base_url, &config.auth_token)
}
#[tauri::command]
fn open_claude_desktop(state: State<'_, AppState>) -> Result<bool, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let proxy_url = format!("http://{}:{}", config.host, config.port);
    let auth_token = config.auth_token.clone();

    // 检查代理是否正在运行
    let guard = state.server.lock().map_err(|_| "server lock poisoned")?;
    let proxy_running = guard.is_some();
    drop(guard);

    if !proxy_running {
        return Err("请先启动本地代理，再配置 Claude Desktop".to_string());
    }

    // 配置 Claude Desktop 的免登录设置
    configure_claude_desktop_free(&proxy_url, &auth_token)?;

    // 然后打开应用
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .arg("/c")
            .arg("start")
            .arg("\"Claude\"")
            .spawn()
            .map_err(|e| format!("无法启动 Claude Desktop: {e}"))?;
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("-a")
            .arg("Claude")
            .spawn()
            .map_err(|e| format!("无法启动 Claude Desktop: {e}"))?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg("claude://")
            .spawn()
            .map_err(|e| format!("无法启动 Claude Desktop: {e}"))?;
    }

    Ok(true)
}

/// 配置 Claude Desktop 免登录模式
///
/// 原理：Claude Desktop 内部的 Claude Code 会读取 ~/.claude/settings.json
use std::io::Read;
/// 中的 claudeCode.environmentVariables，通过注入 ANTHROPIC_BASE_URL
/// 和 ANTHROPIC_AUTH_TOKEN 来绕过官方认证。
fn configure_claude_desktop_free(proxy_url: &str, auth_token: &str) -> Result<(), String> {
    use std::fs;
    use std::io::Write;

    // 获取 Claude Desktop 配置目录
    #[cfg(target_os = "windows")]
    let config_dir = PathBuf::from(std::env::var("APPDATA").map_err(|_| "无法获取 APPDATA")?);
    #[cfg(not(target_os = "windows"))]
    let config_dir = {
        let home = user_home().ok_or("无法获取 HOME 目录")?;
        home.join(".claude")
    };

    let settings_path = config_dir.join("settings.json");

    // 确保目录存在
    fs::create_dir_all(&config_dir).map_err(|e| format!("无法创建配置目录：{e}"))?;

    // 读取现有配置或创建新配置
    let mut config: serde_json::Value = if settings_path.exists() {
        let mut file = fs::File::open(&settings_path).map_err(|e| format!("无法读取配置：{e}"))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|e| format!("无法读取配置：{e}"))?;
        serde_json::from_str(&contents).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // 注入环境变量，让 Claude Desktop 内部的 Claude Code 使用本地代理
    // 这是免登录的核心：ANTHROPIC_BASE_URL 指向本地代理，ANTHROPIC_AUTH_TOKEN 使用配置的 token
    if let Some(env) = config
        .get_mut("claudeCode")
        .and_then(|v| v.as_object_mut())
        .and_then(|o| o.get_mut("environmentVariables"))
        .and_then(|v| v.as_array_mut())
    {
        // 更新或添加 ANTHROPIC_BASE_URL
        if let Some(item) = env
            .iter_mut()
            .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("ANTHROPIC_BASE_URL"))
        {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("value".to_string(), serde_json::json!(proxy_url));
            }
        } else {
            env.push(serde_json::json!({
                "name": "ANTHROPIC_BASE_URL",
                "value": proxy_url
            }));
        }

        // 更新或添加 ANTHROPIC_AUTH_TOKEN
        if let Some(item) = env
            .iter_mut()
            .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("ANTHROPIC_AUTH_TOKEN"))
        {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("value".to_string(), serde_json::json!(auth_token));
            }
        } else {
            env.push(serde_json::json!({
                "name": "ANTHROPIC_AUTH_TOKEN",
                "value": auth_token
            }));
        }
    } else {
        // 创建新的 claudeCode 配置
        if let Some(obj) = config.as_object_mut() {
            obj.insert(
                "claudeCode".to_string(),
                serde_json::json!({
                    "environmentVariables": [
                        { "name": "ANTHROPIC_BASE_URL", "value": proxy_url },
                        { "name": "ANTHROPIC_AUTH_TOKEN", "value": auth_token }
                    ]
                }),
            );
        }

        // 同时注入 environment（Claude Desktop 应用本身使用）
        if let Some(env) = config
            .get_mut("environment")
            .and_then(|v| v.as_object_mut())
        {
            env.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                serde_json::json!(proxy_url),
            );
            env.insert(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                serde_json::json!(auth_token),
            );
        } else {
            config["environment"] = serde_json::json!({
                "ANTHROPIC_BASE_URL": proxy_url,
                "ANTHROPIC_AUTH_TOKEN": auth_token
            });
        }
    }

    // 写回配置文件
    let mut file = fs::File::create(&settings_path).map_err(|e| format!("无法写入配置：{e}"))?;
    let contents = serde_json::to_string_pretty(&config).map_err(|e| format!("序列化失败：{e}"))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| format!("写入失败：{e}"))?;

    // 关键：创建 Claude-3p/claude_desktop_config.json 以启用第三方推理模式
    // 这是让 Claude Desktop 跳过登录的关键
    #[cfg(not(target_os = "windows"))]
    {
        let home = user_home().ok_or("无法获取 HOME 目录")?;
        let claude3p_dir = home.join(".claude").parent().unwrap().join("Claude-3p");
        fs::create_dir_all(&claude3p_dir).map_err(|e| format!("无法创建目录：{e}"))?;

        let claude3p_config_path = claude3p_dir.join("claude_desktop_config.json");
        let claude3p_config = serde_json::json!({
            "deploymentMode": "3p",
            "preferences": {
                "coworkWebSearchEnabled": true,
                "coworkScheduledTasksEnabled": true,
                "ccdScheduledTasksEnabled": false
            }
        });
        let contents3p = serde_json::to_string_pretty(&claude3p_config)
            .map_err(|e| format!("序列化失败：{e}"))?;
        fs::write(&claude3p_config_path, contents3p).map_err(|e| format!("写入失败：{e}"))?;
    }

    Ok(())
}

fn user_home() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn open_terminal_with_claude(cwd: &Path, base_url: &str, token: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // 单引号字面量避免转义噩梦；值里若有单引号按 PS 规则双写。
        // 不自动运行 claude——只设环境变量、打印连接信息，留 prompt 给用户自行 cd / claude。
        let command = format!(
            "$env:ANTHROPIC_AUTH_TOKEN='{token}'; \
             $env:ANTHROPIC_BASE_URL='{url}'; \
             Write-Host ''; \
             Write-Host '  CCNim  ' -ForegroundColor Black -BackgroundColor Cyan; \
             Write-Host '  ANTHROPIC_BASE_URL = ' -NoNewline -ForegroundColor DarkGray; Write-Host $env:ANTHROPIC_BASE_URL -ForegroundColor Cyan; \
             Write-Host '  ANTHROPIC_AUTH_TOKEN = ' -NoNewline -ForegroundColor DarkGray; Write-Host '*** (已注入)' -ForegroundColor Cyan; \
             Write-Host ''; \
             if (Get-Command claude -ErrorAction SilentlyContinue) {{ \
                 Write-Host '  cd <你的项目目录> 然后输入 ' -NoNewline -ForegroundColor DarkGray; Write-Host 'claude' -ForegroundColor Green -NoNewline; Write-Host ' 启动。' -ForegroundColor DarkGray \
             }} else {{ \
                 Write-Host '  未检测到 claude 命令，可执行：' -ForegroundColor Yellow; \
                 Write-Host '  npm install -g @anthropic-ai/claude-code' -ForegroundColor Cyan \
             }}; \
             Write-Host ''",
            token = ps_single_quote(token),
            url = ps_single_quote(base_url)
        );
        Command::new("powershell.exe")
            .arg("-NoExit")
            .arg("-Command")
            .arg(&command)
            .current_dir(cwd)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map_err(|e| format!("无法启动 PowerShell: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "cd {cwd:?}; export ANTHROPIC_AUTH_TOKEN='{token}'; export ANTHROPIC_BASE_URL='{url}'; clear; echo 'CCNim  →  '$ANTHROPIC_BASE_URL; echo 'cd <project> then run: claude'",
            cwd = cwd.display().to_string(),
            token = token.replace('\'', "'\\''"),
            url = base_url
        );
        Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "tell application \"Terminal\" to do script \"{}\"",
                script.replace('\\', "\\\\").replace('"', "\\\"")
            ))
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let inner = format!(
            "cd '{}'; export ANTHROPIC_AUTH_TOKEN='{}'; export ANTHROPIC_BASE_URL='{}'; clear; echo 'CCNim -> '$ANTHROPIC_BASE_URL; echo 'cd <project> then run: claude'; exec $SHELL",
            cwd.display(),
            token.replace('\'', "'\\''"),
            base_url
        );
        let candidates = [
            ("x-terminal-emulator", vec!["-e", "sh", "-lc"]),
            ("gnome-terminal", vec!["--", "sh", "-lc"]),
            ("konsole", vec!["-e", "sh", "-lc"]),
            ("xterm", vec!["-e", "sh", "-lc"]),
        ];
        let mut spawned = false;
        for (cmd, args) in &candidates {
            let mut c = Command::new(cmd);
            for a in args {
                c.arg(a);
            }
            c.arg(&inner);
            if c.spawn().is_ok() {
                spawned = true;
                break;
            }
        }
        if !spawned {
            return Err(
                "未找到可用的终端模拟器 (x-terminal-emulator / gnome-terminal / konsole / xterm)"
                    .into(),
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn ps_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(AppState {
            server: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            start_proxy,
            stop_proxy,
            proxy_status,
            fetch_nim_models,
            fetch_models_for_key,
            open_claude_terminal,
            open_claude_desktop,
            scan_ides,
            apply_ide_settings,
            app_version,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // The user can leave the proxy running and just close the window,
    // which on the only-window setup we ship today funnels into
    // `RunEvent::ExitRequested`. If we let that go through unchanged, the
    // process tears down while the embedded axum task is still inside
    // `axum::serve(...)`, so the listening socket sits in `LISTEN` /
    // `TIME_WAIT` for up to 2 minutes and the user sees "port still in
    // use" the next time they relaunch.
    //
    // The fix is a two-phase exit:
    //   1. First `ExitRequested` → `api.prevent_exit()`, then spawn a
    //      Tauri async task that runs `RunningServer::stop` (which itself
    //      caps graceful-shutdown wait time and falls back to `abort()`
    //      so the listener is *guaranteed* to be dropped). When that
    //      task finishes we re-issue `app_handle.exit(0)`.
    //   2. Second `ExitRequested` (re-fired by the `exit(0)` above) →
    //      let it through. We gate this on an `AtomicBool` so we never
    //      loop forever.
    //
    // We deliberately do NOT call `block_on` from the `ExitRequested`
    // callback — that callback runs on the main GUI thread, and blocking
    // it during shutdown is unsafe (the tokio runtime is itself winding
    // down, and on macOS this is known to crash).
    app.run(|app_handle, event| {
        if let RunEvent::ExitRequested { api, .. } = &event {
            let state = app_handle.state::<AppState>();
            if state.shutting_down.swap(true, Ordering::SeqCst) {
                return;
            }
            let server = state.server.lock().ok().and_then(|mut guard| guard.take());
            if let Some(server) = server {
                api.prevent_exit();
                tracing::info!("shutting down proxy server before exit");
                let handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    server.stop().await;
                    handle.exit(0);
                });
            }
        }
    });
}
