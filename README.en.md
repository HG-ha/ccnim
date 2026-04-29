<div align="center">

# CCNim

A local multi-endpoint proxy for Claude Code / VSCode extensions:
defaults to free **NVIDIA NIM**, also supports any **OpenAI-compatible**
host (DeepSeek, Moonshot, OpenRouter, self-hosted vLLM…) and
**Anthropic-compatible** host (Claude official, Zhipu BigModel,
DeepSeek's anthropic API…).

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](LICENSE)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24c8db.svg?style=for-the-badge&logo=tauri)](https://tauri.app)
[![Release](https://img.shields.io/github/v/release/HG-ha/ccnim?style=for-the-badge&label=latest)](https://github.com/HG-ha/ccnim/releases/latest)

[简体中文](README.md) · English

A small Rust + Tauri desktop app that runs a local Anthropic-compatible
proxy. Any number of upstream keys go into a single pool, rotated by
health and rate-limit headroom; one-click configures Claude Code CLI,
Continue, Cline, and RooCode against the local endpoint.

<img src="pic.png" width="700" alt="CCNim screenshot">

</div>

## Quick start

1. Grab at least one upstream key:
   - **NVIDIA NIM** (40 req/min free): get a key starting with
     `nvapi-` from
     [build.nvidia.com/settings/api-keys](https://build.nvidia.com/settings/api-keys);
   - **OpenAI-compatible**: DeepSeek, Moonshot, OpenRouter, Groq,
     self-hosted vLLM, etc.;
   - **Anthropic-compatible**: Claude official, Zhipu BigModel
     anthropic API, etc.
2. Grab the latest Windows installer
   (`CCNim_*_x64-setup.exe`) from the
   **[Releases page](https://github.com/HG-ha/ccnim/releases/latest)**
   and double-click to install.
3. Launch the app. On the **API Keys** tab, pick the endpoint type
   (NIM / OpenAI-compat / Anthropic-compat), fill in the base URL and
   paste the key, then click **启动代理 (Start Proxy)**.
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
- **Multi-endpoint, multi-key dispatch**: every key picks its own
  endpoint type and base URL; all keys share one rotation pool.
  NIM / OpenAI-compat traffic uses `chat/completions` plus an
  Anthropic SSE adapter; Anthropic-compat traffic is forwarded
  verbatim, preserving native features (thinking, tool_use, …).
- **Rate-limit safety**: each key defaults to 40 req/min. The app
  picks a key per request based on health, in-flight count, and
  rolling-window usage; 429 / 401 responses automatically cool the
  offending key down or disable it. Live per-key state, endpoint
  type and base URL show up on the dashboard.
- **Model mapping**: map Anthropic Opus / Sonnet / Haiku to any
  upstream model; leave blank to fall back to the default. The
  catalog refreshes every 30 minutes for NIM / OpenAI-compat hosts;
  Anthropic-compat hosts don't expose `/v1/models`, so type the
  model ID directly.
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
| `%APPDATA%\dev\ccnim\CCNim\config\secrets.json` | Auth token + every upstream API key (with endpoint type and base URL), current-user-only |

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
