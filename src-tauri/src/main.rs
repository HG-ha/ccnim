#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod ide_scan;

use std::path::PathBuf;
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
            server
                .key_pool()
                .update_keys(key_pool_entries(&config.nim_api_keys));
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
    // they have configured.
    let guard = state.server.lock().map_err(|_| "server lock poisoned")?;
    let (running, keys) = match guard.as_ref() {
        Some(server) => (true, server.key_pool().snapshots()),
        None => {
            let pool = KeyPool::new(
                key_pool_entries(&config.nim_api_keys),
                config.rate_limit_per_key,
                std::time::Duration::from_secs(config.rate_window_secs),
            );
            (false, pool.snapshots())
        }
    };
    drop(guard);

    Ok(ProxyStatus {
        running,
        listen_url: format!("http://{}:{}", config.host, config.port),
        default_model: config.model_mapping.default_model,
        keys,
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
fn read_diagnostic_log() -> Result<String, String> {
    let path = AppConfig::diagnostic_log_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))
}

#[tauri::command]
async fn fetch_nim_models() -> Result<proxy_core::NimModelList, String> {
    let config = AppConfig::load_or_default().map_err(|e| e.to_string())?;
    let key_pool = KeyPool::new(
        key_pool_entries(&config.nim_api_keys),
        config.rate_limit_per_key,
        std::time::Duration::from_secs(config.rate_window_secs),
    );
    let client = NimClient::new(key_pool).map_err(|e| e.to_string())?;
    client.list_models().await.map_err(|e| e.to_string())
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

fn open_terminal_with_claude(cwd: &PathBuf, base_url: &str, token: &str) -> Result<(), String> {
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
            open_claude_terminal,
            read_diagnostic_log,
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
