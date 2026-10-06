<div align="center">

<img src="src-tauri/icons/icon.png" width="128" alt="Agent 会话管理器图标"/>

# Agent 会话管理器

**Codex、Claude Code、DeepSeek Harness 等本地编码 Agent 的会话管理工具**

一款轻量的桌面应用，统一浏览、分类与安全清理本地编码 Agent 的会话记录。

<sub>Desktop session manager for local coding agents — browse, group and safely clean up Codex, Claude Code, DeepSeek Harness, OpenCode, ZCode, Pi and Grok Build sessions on Windows and Linux.</sub>

[![Release](https://img.shields.io/github/v/release/zhuchongba-01/AgentSessionManager?style=flat-square)](https://github.com/zhuchongba-01/AgentSessionManager/releases)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64%20%7C%20Linux%20amd64-blue?style=flat-square)](https://github.com/zhuchongba-01/AgentSessionManager/releases)

![主界面](docs/images/main-window.png)

*主界面：左侧按 Agent 与项目分组的会话列表，中间会话缩略导航，右侧完整会话详情。*

</div>

## 这是什么

面向本地编码 Agent 的独立会话浏览、分类与安全清理工具。当前版本不负责启动 Agent 或恢复会话。

当前支持扫描 **Codex、DeepSeek Harness、Claude Code、OpenCode、ZCode、Pi 与 Grok Build** 七种 Agent 的本地会话。项目归类优先遵循各 Agent 自身保存的项目绑定。

## 主要特性

- 🔍 **七种 Agent，一个界面**：Codex、DeepSeek Harness、Claude Code、OpenCode、ZCode、Pi、Grok Build 的本地会话一站式浏览。
- 🗂️ **项目分组 + 缩略导航**：遵循 Agent 的项目绑定与 Codex 自定义侧边栏分区，紧凑型列表配合缩略导航快速定位。
- 📖 **完整会话详情**：按时间线阅读本地 Agent 的对话内容，快速定位历史消息。
- 🧹 **安全清理**：识别残留、归档和仅剩索引的任务，支持批量删除与失败后继续清理；会话清理绝不删除项目工作目录。
- 🛡️ **明确失败而非假装为空**：扫描不完整或消息读取失败时显示原因，并限制超大会话的单次读取量。
- 📚 **长历史可控**：会话列表每页最多显示 100 条，消息按可视区域渲染。
- 💻 **会话数据留在本机**：扫描、展示与摘要全部在本机完成，不上传会话内容；启动时仅访问 GitHub API 检查新版本。

删除前请完全退出对应 Agent（包括后台进程、终端任务），并自行备份重要会话。详细边界见 [SAFETY.md](./SAFETY.md)。

## 下载安装

前往 [Releases](https://github.com/zhuchongba-01/AgentSessionManager/releases) 下载安装包，无需预装运行时。

**Windows x64**：下载 `AgentSessionManager-Setup-vX.Y.Z.exe`，关闭旧版管理器后运行安装包。

**Linux amd64**（Ubuntu 24.04 已测试）：下载 `AgentSessionManager-X.Y.Z-amd64.deb` 后安装。

```bash
sudo apt install ./AgentSessionManager-X.Y.Z-amd64.deb
```

> [!IMPORTANT]
> 本程序暂未进行商业代码签名。Edge 首次下载时可能提示“不常下载”，请选择“保留”；Windows SmartScreen 如显示“Windows 已保护你的电脑”，请点击“更多信息” → “仍要运行”。请只从本仓库的 Releases 页面下载安装包。

## 开发

```powershell
pnpm install
pnpm tauri dev
```

Ubuntu 24.04 下开发与打包需先安装系统依赖：

```bash
sudo apt install -y build-essential curl file wget \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  librsvg2-dev libxdo-dev
pnpm install --frozen-lockfile
```

## 验证

```powershell
pnpm version:check
pnpm test:version
pnpm typecheck
pnpm format:check
pnpm test:unit
pnpm build:renderer
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

GitHub Actions 在 Windows 上执行上述检查，并在 Ubuntu 24.04 上运行 `pnpm build:linux` 产出 `.deb` 与 AppImage 制品。Node 与 Rust 版本分别读取 `.node-version` 和 `rust-toolchain.toml`，pnpm 版本读取 `package.json` 的 `packageManager`。CI 不创建 Release。

## 更新版本与打包

```powershell
# 仅接受 X.Y.Z 正式版号；不创建 Git 提交或标签
pnpm version:set 1.3.17
pnpm version:check
pnpm build:installer   # Windows：Tauri 构建 + Inno Setup
pnpm build:linux       # Linux：输出 .deb 与 AppImage
```

`version:set` 同步更新 package.json、Cargo.toml、Cargo.lock 中的本项目版本、Tauri 版本和窗口标题、Inno Setup 版本，不修改依赖版本。脚本先检查所有目标字段，再写入变更；请在无并发编辑时运行，并检查 Git diff。`version:check` 以 package.json 为准，只读检查版本一致性。标准 `pnpm build` 和 Inno Setup 打包前也会检查版本。

版本脚本不依赖 node_modules，也可直接运行 `node scripts/version.mjs --check` 或 `node scripts/version.mjs 1.3.17`。版本脚本测试使用临时目录，不修改项目版本文件。

## 致谢

特别感谢 [LINUX DO](https://linux.do) 社区！！更多信息请参阅 [ACKNOWLEDGMENTS.md](./ACKNOWLEDGMENTS.md)。

## 开源许可

本项目采用 [MIT License](./LICENSE) 开源。
