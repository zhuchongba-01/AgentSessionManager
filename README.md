# Agent 会话管理器

[GitHub 项目主页](https://github.com/zhuchongba-01/AgentSessionManager)

面向本地编码 Agent 的独立会话浏览、分类、恢复与安全删除工具。

当前支持扫描 Codex、Claude Code、OpenCode、ZCode、Pi 与 Grok Build 的本地会话。项目归类优先遵循各 Agent 自身保存的项目绑定；删除真实工作目录时会检查同目录下的其他 Agent 会话，并将目录移入系统回收站。

## 开发

```powershell
pnpm install
pnpm tauri dev
```

## 验证

```powershell
pnpm typecheck
pnpm test:unit
cargo test --manifest-path src-tauri/Cargo.toml session_manager
```
