<div align="center">

# CCNim

基于 **NVIDIA NIM** 的 Claude Code / VSCode 扩展免费本地代理。

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](LICENSE)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24c8db.svg?style=for-the-badge&logo=tauri)](https://tauri.app)
[![Release](https://img.shields.io/github/v/release/HG-ha/ccnim?style=for-the-badge&label=latest)](https://github.com/HG-ha/ccnim/releases/latest)

简体中文 · [English](README.en.md)

一个 Rust + Tauri 桌面应用：在本机起一个 Anthropic 兼容代理，
上游对接 NVIDIA NIM（每个 Key 免费 40 次/分钟），自动多 Key 轮询，
一键把 Claude Code CLI、Continue、Cline、RooCode 配置到本地端点。

<img src="pic.png" width="700" alt="CCNim 截图">

</div>

## 快速开始

1. 在 [build.nvidia.com/settings/api-keys](https://build.nvidia.com/settings/api-keys)
   申请一个免费的 NIM Key（以 `nvapi-` 开头）。
2. 到 **[Releases 页面](https://github.com/HG-ha/ccnim/releases/latest)**
   下载最新的 Windows 安装包（`CCNim_*_x64-setup.exe`），双击安装。
3. 打开应用，把 Key 粘进去，点 **启动代理**。
4. 切到 **IDE 接入** 页一键写入 VSCode / Cursor / Windsurf 的配置，
   或者点 **打开预配置终端** 直接跑 `claude`。

应用启动后会在后台静默检查更新，发现新版会在仪表盘顶部弹横幅；
也可以随时点右上角的 **检查更新** 手动触发，下载完成后一键重启即可。

## 主要功能

- **本地 Anthropic 兼容代理**：完整支持 `/v1/messages`（含 SSE 流式
  输出）、`/v1/messages/count_tokens`、`/v1/models`。Claude Code、
  VSCode 官方插件等只要把 `ANTHROPIC_BASE_URL` 指过来就能用。
- **多 Key 轮询 + 限流防护**：每个 Key 免费 40 次/分钟，应用按健康
  状态、并发数、最近窗口用量自动选 Key；遇到 429 / 401 自动冷却或
  禁用，仪表盘实时显示每个 Key 的状态。
- **模型映射**：把 Anthropic 的 Opus / Sonnet / Haiku 映射到任意
  NIM 模型，留空走默认模型。模型列表每 30 分钟从 NIM 自动拉取。
- **IDE 一键接入**：自动扫描本机 VSCode 系列 IDE（VSCode / Cursor /
  Windsurf 等）的 `settings.json`，一键写入
  `claudeCode.environmentVariables`。
- **应用内自动更新**：通过 GitHub Releases + 签名校验下发，应用启动
  时静默检查，有新版本时一键下载安装并重启。
- **配置 / 密钥隔离**：普通配置写到 `%APPDATA%\dev\ccnim\CCNim\config\config.json`，
  密钥写到同目录下独立的 `secrets.json`（仅当前用户可读，Unix 自动
  `chmod 600`），仓库里不留任何敏感信息。

## 配置文件位置

所有设置都可以在界面里改，落盘位置：

| 文件 | 内容 |
| --- | --- |
| `%APPDATA%\dev\ccnim\CCNim\config\config.json` | 主机/端口、模型映射、限流参数等非敏感配置 |
| `%APPDATA%\dev\ccnim\CCNim\config\secrets.json` | Auth Token + NIM API Keys，仅当前用户可读 |

两个文件都不会进入项目目录，也不会被上传。

## 从源码运行

如果你想魔改或自己 build 一份：

```powershell
npm install
npm run tauri dev          # 开发调试
npm run tauri build        # 打 NSIS 安装包（Windows）
```

构建依赖：Rust stable、Node.js ≥ 20，以及对应平台的
[Tauri 2 系统依赖](https://tauri.app/start/prerequisites/)。

## 架构

详见 [PLAN.md](PLAN.md)（workspace 布局、crate 边界、依赖规则）。

## License

MIT，详见 [LICENSE](LICENSE)。
