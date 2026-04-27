<div align="center">

# CCNim

A free local proxy for Claude Code / VSCode extensions, powered by **NVIDIA NIM**.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](LICENSE)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24c8db.svg?style=for-the-badge&logo=tauri)](https://tauri.app)
[![Release](https://img.shields.io/github/v/release/HG-ha/ccnim?style=for-the-badge&label=latest)](https://github.com/HG-ha/ccnim/releases/latest)

[简体中文](README.md) · English

A small Rust + Tauri desktop app that runs a local Anthropic-compatible
proxy on top of NVIDIA NIM (40 req/min free per key). Manages multiple
keys with automatic rotation, and one-click configures Claude Code CLI,
Continue, Cline, and RooCode against the local endpoint.

<img src="pic.png" width="700" alt="CCNim screenshot">

</div>

## Quick start

1. Get a free NIM key (starts with `nvapi-`) at
   [build.nvidia.com/settings/api-keys](https://build.nvidia.com/settings/api-keys).
2. Grab the latest Windows installer
   (`CCNim_*_x64-setup.exe`) from the
   **[Releases page](https://github.com/HG-ha/ccnim/releases/latest)**
   and double-click to install.
3. Launch the app, paste the key, click **启动代理 (Start Proxy)**.
4. Open the **IDE 接入 (IDE Setup)** tab to one-click configure VSCode /
   Cursor / Windsurf, or hit **打开预配置终端 (Open Preconfigured
   Terminal)** to run `claude` directly.

The app silently checks for updates in the background after launch and
surfaces a banner on the dashboard whenever a new release is available;
you can also hit **检查更新 (Check for Updates)** manually any time, and
relaunch with one click once the download finishes.

## Features

- **Local Anthropic-compatible proxy**: full support for
  `/v1/messages` (incl. SSE streaming),
  `/v1/messages/count_tokens`, and `/v1/models`. Anything that points
  `ANTHROPIC_BASE_URL` at the local endpoint — Claude Code, the
  official VSCode extension, etc. — just works.
- **Multi-key rotation + rate-limit safety**: each NIM key gets 40
  req/min free. The app picks a key per request based on health,
  in-flight count, and rolling-window usage; 429 / 401 responses
  automatically cool the offending key down or disable it. Live
  per-key state shows up on the dashboard.
- **Model mapping**: map Anthropic Opus / Sonnet / Haiku to any NIM
  model; leave blank to fall back to the default. The catalog
  refreshes from NIM every 30 minutes.
- **One-click IDE integration**: scans your installed VSCode-family
  IDEs (VSCode / Cursor / Windsurf …) and writes
  `claudeCode.environmentVariables` into their `settings.json`.
- **In-app auto-updater**: signed releases via GitHub Releases. The
  app probes for updates on launch and offers one-click
  download-install-relaunch.
- **Config / secrets isolation**: normal config is stored in
  `%APPDATA%\dev\ccnim\CCNim\config\config.json`; secrets land in a
  sibling `secrets.json` (current-user-only, `chmod 600` on Unix).
  Neither file ever enters the project tree.

## Where settings live

Everything is editable in the GUI; persisted state lives at:

| File | Holds |
| --- | --- |
| `%APPDATA%\dev\ccnim\CCNim\config\config.json` | Non-secret config (host, port, model mapping, rate limits, …) |
| `%APPDATA%\dev\ccnim\CCNim\config\secrets.json` | Auth token + NIM API keys, current-user-only |

## Running from source

If you want to hack on it or build your own:

```powershell
npm install
npm run tauri dev          # development
npm run tauri build        # release NSIS installer (Windows)
```

Build requirements: Rust stable, Node.js ≥ 20, plus the platform-specific
[Tauri 2 prerequisites](https://tauri.app/start/prerequisites/).

## Architecture

See [PLAN.md](PLAN.md) — workspace layout, crate boundaries, and
dependency rules.

## License

MIT, see [LICENSE](LICENSE).
