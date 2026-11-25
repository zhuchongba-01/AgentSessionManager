(() => {
  try {
    const params = new URLSearchParams(window.location.search);
    const forcedTheme = params.get("theme");
    if (forcedTheme === "light" || forcedTheme === "dark" || forcedTheme === "system") {
      window.localStorage.setItem("agent-session-manager-theme", forcedTheme);
      const isDark =
        forcedTheme === "dark" ||
        (forcedTheme === "system" &&
          window.matchMedia("(prefers-color-scheme: dark)").matches);
      document.documentElement.classList.toggle("dark", isDark);
      document.documentElement.classList.toggle("light", !isDark);
    } else if (!window.localStorage.getItem("agent-session-manager-theme")) {
      window.localStorage.setItem("agent-session-manager-theme", "dark");
      document.documentElement.classList.add("dark");
    }
    if (!window.localStorage.getItem("agent-session-manager.sidebarWidth")) {
      window.localStorage.setItem("agent-session-manager.sidebarWidth", "348");
    }
    if (!window.localStorage.getItem("agent-session-manager.listViewMode")) {
      window.localStorage.setItem("agent-session-manager.listViewMode", "grouped");
    }
  } catch {
    // ignore storage errors in private mode
  }

  const cbs = new Map();
  let nextId = 1;
  const now = Date.now();

  function createInitialSessions() {
    return [
      {
        providerId: "claude",
        sessionId: "claude-jwt-fix-01",
        title: "排查网关 JWKS 密钥轮换偶发 401 并修复缓存刷新",
        summary: "已在遇到未知 kid 时强制触发单飞回源刷新，单测全部通过。",
        projectDir: "E:/workspace/edge-gateway",
        projectName: "edge-gateway",
        createdAt: now - 420_000,
        lastActiveAt: now - 90_000,
        sourcePath: "C:/Users/demo/.claude/projects/edge-gateway/claude-jwt-fix-01.jsonl",
        resumeCommand: "claude --resume claude-jwt-fix-01",
      },
      {
        providerId: "claude",
        sessionId: "claude-pkce-02",
        title: "收紧 OAuth2 PKCE 回调参数与 state 防重放校验",
        summary: "已将一次性授权码挑战绑定到 Redis 原子消费脚本。",
        projectDir: "E:/workspace/edge-gateway",
        projectName: "edge-gateway",
        createdAt: now - 5_400_000,
        lastActiveAt: now - 2_400_000,
        sourcePath: "C:/Users/demo/.claude/projects/edge-gateway/claude-pkce-02.jsonl",
        resumeCommand: "claude --resume claude-pkce-02",
      },
      {
        providerId: "deepseek",
        sessionId: "ds-subagent-03",
        parentSessionId: "claude-jwt-fix-01",
        title: "核对 RFC 7517 kid 轮换并发单飞边界条件",
        summary: "验证高并发未知 kid 请求共享同一个刷新 Promise，不击穿上游。",
        projectDir: "E:/workspace/edge-gateway",
        projectName: "edge-gateway",
        residual: true,
        createdAt: now - 360_000,
        lastActiveAt: now - 210_000,
        sourcePath: "C:/Users/demo/.dsh/sessions/ds-subagent-03/session.v3.jsonl",
      },
      {
        providerId: "codex",
        sessionId: "codex-fts5-92",
        title: "升级 SQLite FTS5 全文检索索引与并发取消机制",
        summary: "将全文检索升级到 INDEX_VERSION = 2，仅索引非注入正文块。",
        projectDir: "E:/workspace/session-hub",
        projectName: "session-hub",
        createdAt: now - 7_200_000,
        lastActiveAt: now - 3_200_000,
        sourcePath: "C:/Users/demo/.codex/sessions/2026/11/25/rollout-92.jsonl",
        resumeCommand: "codex resume codex-fts5-92",
      },
      {
        providerId: "codex",
        sessionId: "codex-stale-19",
        title: "清理过期调试会话快照与边车索引记录",
        summary: "已归档旧版实验分支调试会话。",
        projectDir: "E:/workspace/session-hub",
        projectName: "session-hub",
        residual: true,
        archived: true,
        createdAt: now - 36_000_000,
        lastActiveAt: now - 18_000_000,
        sourcePath: "C:/Users/demo/.codex/archived_sessions/rollout-19.jsonl",
      },
      {
        providerId: "gemini",
        sessionId: "gemini-billing-04",
        title: "账单对账任务幂等锁与死信队列指数退避策略",
        projectDir: "E:/workspace/billing-worker",
        projectName: "billing-worker",
        createdAt: now - 92_000_000,
        lastActiveAt: now - 86_400_000,
        sourcePath: "C:/Users/demo/.gemini/tmp/billing-worker/chats/session-04.jsonl",
        resumeCommand: "gemini --resume gemini-billing-04",
      },
      {
        providerId: "copilot",
        sessionId: "copilot-ci-matrix-05",
        title: "修复 GitHub Actions Windows 与 Ubuntu 双平台构建缓存",
        projectDir: "E:/workspace/session-hub",
        projectName: "session-hub",
        createdAt: now - 110_000_000,
        lastActiveAt: now - 96_000_000,
        sourcePath: "C:/Users/demo/.copilot/session-state/copilot-ci-matrix-05/events.jsonl",
        resumeCommand: "gh copilot --resume copilot-ci-matrix-05",
      },
      {
        providerId: "workbuddy",
        sessionId: "wb-sta-dialog-06",
        title: "修复 Windows COM IFileSaveDialog 独立 STA 线程另存为弹窗",
        projectDir: "E:/workspace/session-hub",
        projectName: "session-hub",
        createdAt: now - 135_000_000,
        lastActiveAt: now - 120_000_000,
        sourcePath: "C:/Users/demo/.workbuddy/projects/session-hub/wb-sta-dialog-06.jsonl",
        resumeCommand: "workbuddy --resume wb-sta-dialog-06",
      },
      {
        providerId: "codebuddy",
        sessionId: "cb-dag-rollback-07",
        title: "解析 CodeBuddy DAG 消息链与文件回滚边车记录",
        projectDir: "E:/workspace/session-hub",
        projectName: "session-hub",
        createdAt: now - 180_000_000,
        lastActiveAt: now - 165_000_000,
        sourcePath: "C:/Users/demo/.codebuddy/projects/session-hub/cb-dag-rollback-07.jsonl",
        resumeCommand: "codebuddy --resume cb-dag-rollback-07",
      },
      {
        providerId: "opencode",
        sessionId: "ses_opencode_08",
        title: "优化虚拟列表帧率与无箭头悬浮胶囊滚动条样式",
        projectDir: "E:/workspace/web-console",
        projectName: "web-console",
        createdAt: now - 240_000_000,
        lastActiveAt: now - 210_000_000,
        sourcePath: "sqlite:C:/Users/demo/.local/share/opencode/opencode.db:ses_opencode_08",
        resumeCommand: "opencode -s ses_opencode_08",
      },
      {
        providerId: "zcode",
        sessionId: "zcode-task-09",
        title: "校验 ZCode 双库任务谱系保护与工作区兄弟会话隔离",
        projectDir: "E:/workspace/web-console",
        projectName: "web-console",
        createdAt: now - 310_000_000,
        lastActiveAt: now - 290_000_000,
        sourcePath: "sqlite-zcode:C:/Users/demo/.zcode/cli/db/db.sqlite:zcode-task-09",
      },
      {
        providerId: "kimi",
        sessionId: "kimi-wire-10",
        title: "对齐 Kimi CLI wire.jsonl 流式工具参数与耗时统计",
        projectDir: "E:/workspace/ai-adapters",
        projectName: "ai-adapters",
        createdAt: now - 390_000_000,
        lastActiveAt: now - 360_000_000,
        sourcePath: "C:/Users/demo/.kimi/sessions/kimi-wire-10/context.jsonl",
        resumeCommand: "kimi --session kimi-wire-10",
      },
      {
        providerId: "pi",
        sessionId: "pi-branch-11",
        title: "支持 Pi 树状分支会话最新主干路径提取",
        projectDir: "E:/workspace/ai-adapters",
        projectName: "ai-adapters",
        createdAt: now - 450_000_000,
        lastActiveAt: now - 420_000_000,
        sourcePath: "C:/Users/demo/.pi/agent/sessions/pi-branch-11.jsonl",
        resumeCommand: "pi --session pi-branch-11",
      },
      {
        providerId: "openclaw",
        sessionId: "openclaw-agent-12",
        title: "接入 OpenClaw 多代理目录扫描与状态徽标映射",
        projectDir: "E:/workspace/ai-adapters",
        projectName: "ai-adapters",
        createdAt: now - 520_000_000,
        lastActiveAt: now - 490_000_000,
        sourcePath: "C:/Users/demo/.openclaw/agents/coder/sessions/openclaw-agent-12.jsonl",
      },
      {
        providerId: "grokbuild",
        sessionId: "grok-zstd-13",
        title: "流式解压 Grok Build 超大 zstd 历史归档记录并限制内存峰值",
        projectDir: "E:/workspace/edge-gateway",
        projectName: "edge-gateway",
        residual: true,
        createdAt: now - 610_000_000,
        lastActiveAt: now - 580_000_000,
        sourcePath: "C:/Users/demo/.grok/archived_sessions/grok-zstd-13.jsonl.zst",
      },
    ];
  }

  let sessions = createInitialSessions();

  let cachedDiagramDataUrl = null;
  function getDemoDiagramDataUrl() {
    if (cachedDiagramDataUrl) return cachedDiagramDataUrl;
    try {
      const canvas = document.createElement("canvas");
      canvas.width = 680;
      canvas.height = 240;
      const ctx = canvas.getContext("2d");
      if (ctx) {
        ctx.fillStyle = "#0f172a";
        ctx.fillRect(0, 0, 680, 240);
        ctx.strokeStyle = "#334155";
        ctx.lineWidth = 2;
        ctx.strokeRect(12, 12, 656, 216);

        const drawBox = (x, y, w, h, border, title, sub) => {
          ctx.fillStyle = "#1e293b";
          ctx.fillRect(x, y, w, h);
          ctx.strokeStyle = border;
          ctx.lineWidth = 2;
          ctx.strokeRect(x, y, w, h);
          ctx.fillStyle = "#f8fafc";
          ctx.font = "bold 14px system-ui, sans-serif";
          ctx.fillText(title, x + 14, y + 30);
          ctx.fillStyle = "#94a3b8";
          ctx.font = "12px monospace";
          ctx.fillText(sub, x + 14, y + 54);
        };

        drawBox(36, 78, 170, 80, "#3b82f6", "1. Edge Gateway", "kid=kid-2026-11");
        drawBox(255, 78, 175, 80, "#a855f7", "2. Singleflight", "inflightRefresh");
        drawBox(480, 78, 164, 80, "#10b981", "3. JWKS Origin", "200 OK · New Key");

        ctx.strokeStyle = "#60a5fa";
        ctx.lineWidth = 2.5;
        ctx.beginPath();
        ctx.moveTo(206, 118);
        ctx.lineTo(255, 118);
        ctx.moveTo(430, 118);
        ctx.lineTo(480, 118);
        ctx.stroke();

        ctx.fillStyle = "#cbd5e1";
        ctx.font = "bold 13px system-ui, sans-serif";
        ctx.fillText("JWKS 未知 kid 单飞强制回源刷新时序图（点击可打开灯箱缩放或复制图片）", 36, 44);
        ctx.fillStyle = "#64748b";
        ctx.font = "12px system-ui, sans-serif";
        ctx.fillText("5s 最小冷却保护 · 高并发共享同一 Promise · 彻底消除 15m 轮换盲区", 36, 196);
        cachedDiagramDataUrl = canvas.toDataURL("image/png");
        return cachedDiagramDataUrl;
      }
    } catch {
      // fallback 1x1 png
    }
    return "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
  }

  const demoImageRef = {
    source: { kind: "local_file", path: "E:/workspace/edge-gateway/docs/jwks-singleflight.png" },
    mediaType: "image/png",
    size: 18432,
    alt: "JWKS 单飞回源刷新时序图",
  };

  const transcriptsBySession = {
    "claude-jwt-fix-01": [
      {
        role: "user",
        content: "",
        ts: now - 400_000,
        blocks: [
          {
            type: "text",
            text: "排查为什么网关 JWT 鉴权在轮换密钥后偶发 401，并修复相关代码：\n```txt\nHTTP/1.1 401 Unauthorized: unknown kid=kid-2026-11\n```",
          },
        ],
      },
      {
        role: "assistant",
        content: "",
        ts: now - 320_000,
        meta: {
          model: "claude-opus-4-6",
          inputTokens: 18420,
          outputTokens: 1260,
          cacheReadTokens: 12000,
          costUsd: 0.084,
          durationMs: 6800,
        },
        blocks: [
          {
            type: "thinking",
            summary: "核对 JWKS 缓存 TTL 与未知 kid 回源刷新时机",
            text: "先对比签发服务的 kid 轮换窗口与网关本地缓存策略，定位未知 kid 是否被直接拒绝而未触发回源拉取…",
            durationMs: 3200,
            full: { kind: "jsonl", offset: 100, len: 520, pointer: "/claude/thinking" },
          },
          {
            type: "text",
            text: "先读取网关鉴权中间件并检索 `refreshJwks` 调用链。",
          },
          {
            type: "tool_call",
            id: "call_r1",
            rawName: "Read",
            kind: "read",
            title: "E:/workspace/edge-gateway/src/auth/jwt.ts",
            inputPreview: '{"file_path":"E:/workspace/edge-gateway/src/auth/jwt.ts"}',
            inputTotalLen: 58,
          },
          {
            type: "tool_result",
            callId: "call_r1",
            status: "success",
            preview: "const key = jwksCache.get(header.kid);\nif (!key) throw new UnauthorizedError('unknown kid');",
            totalLen: 92,
            lineCount: 2,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "call_s1",
            rawName: "Grep",
            kind: "search",
            title: "refreshJwks",
            inputPreview: '{"pattern":"refreshJwks"}',
            inputTotalLen: 27,
          },
          {
            type: "tool_result",
            callId: "call_s1",
            status: "success",
            preview: "src/auth/jwks.ts:19:export async function refreshJwks(opts?: { force?: boolean })",
            totalLen: 81,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "call_e1",
            rawName: "Edit",
            kind: "edit",
            title: "E:/workspace/edge-gateway/src/auth/jwt.ts",
            inputPreview: '{"file_path":"E:/workspace/edge-gateway/src/auth/jwt.ts"}',
            inputTotalLen: 58,
            diff: {
              files: [
                {
                  path: "E:/workspace/edge-gateway/src/auth/jwt.ts",
                  op: "update",
                  added: 4,
                  removed: 1,
                },
              ],
              added: 4,
              removed: 1,
              full: { kind: "jsonl", offset: 700, len: 420, pointer: "/claude/diff" },
            },
          },
          {
            type: "tool_result",
            callId: "call_e1",
            status: "success",
            preview: "Updated src/auth/jwt.ts",
            totalLen: 23,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "call_b1",
            rawName: "Bash",
            kind: "shell",
            title: "pnpm test src/auth/jwt.test.ts",
            inputPreview: '{"command":"pnpm test src/auth/jwt.test.ts"}',
            inputTotalLen: 44,
          },
          {
            type: "tool_result",
            callId: "call_b1",
            status: "success",
            preview: "PASS src/auth/jwt.test.ts (4 tests) 142ms",
            totalLen: 41,
            lineCount: 1,
            truncated: false,
            exitCode: 0,
            durationMs: 820,
          },
          {
            type: "text",
            text: "### 根因与修复总结\n\n当签发端轮换到新密钥 `kid-2026-11` 时，`src/auth/jwt.ts:48` 在本地缓存未命中时直接抛出了 `401`，未触发回源刷新。现已参照 [RFC 7517 规范](https://datatracker.ietf.org/doc/html/rfc7517) 补充单飞强制刷新逻辑：\n\n| 场景 | 修复前 | 修复后 |\n| --- | --- | --- |\n| 已知 `kid` | 命中内存缓存 | 命中内存缓存 |\n| 轮换新 `kid` | 偶发 401（最长持续 15m） | 单飞强制刷新 JWKS 后立即放行 |\n\n```ts\nlet key = jwksCache.get(header.kid);\nif (!key) {\n  await refreshJwks({ force: true });\n  key = jwksCache.get(header.kid);\n}\n```",
          },
        ],
      },
      {
        role: "user",
        content: "",
        ts: now - 180_000,
        blocks: [
          {
            type: "text",
            text: "高并发下同时遇到未知 kid 会不会重复打爆上游 JWKS 端点？附上刷新时序图确认一下。",
          },
        ],
      },
      {
        role: "assistant",
        content: "",
        ts: now - 90_000,
        meta: {
          model: "claude-opus-4-6",
          inputTokens: 19800,
          outputTokens: 420,
          costUsd: 0.042,
          durationMs: 2100,
        },
        blocks: [
          {
            type: "thinking",
            summary: "核验 inflightRefresh 单飞锁与 5 秒最小冷却间隔",
            text: "并发请求在 inflightRefresh 非空时会直接 await 同一个 Promise，且失败或无效 kid 受 5s 冷却窗口保护…",
            durationMs: 950,
          },
          {
            type: "image",
            image: demoImageRef,
          },
          {
            type: "text",
            text: "不会打爆上游。`src/auth/jwks.ts:19` 复用了 `inflightRefresh` 单飞锁与 **5 秒最小冷却窗口**，瞬时上千并发也只会发出 **1 次** 上游请求（点击上方时序图可打开图片灯箱体验缩放与复制）。",
          },
        ],
      },
    ],
    "codex-fts5-92": [
      {
        role: "user",
        content: "# AGENTS.md instructions for E:/workspace/session-hub\n\n- Run cargo clippy -- -D warnings before committing.",
        injected: true,
        blocks: [
          {
            type: "text",
            text: "# AGENTS.md instructions for E:/workspace/session-hub\n\n- Run cargo clippy -- -D warnings before committing.",
          },
        ],
      },
      {
        role: "system",
        content: "",
        blocks: [
          {
            type: "event",
            kind: "compaction",
            text: "已压缩前序 14 轮上下文摘要：完成 13 家提供商结构化 SessionBlock 解析。",
          },
          {
            type: "event",
            kind: "pr_link",
            text: "PR #184",
            url: "https://github.com/zhuchongba-01/AgentSessionManager/releases",
          },
        ],
      },
      {
        role: "user",
        content: "",
        ts: now - 3_600_000,
        blocks: [
          {
            type: "text",
            text: "把 SQLite FTS5 正文索引升级到 INDEX_VERSION = 2，只索引非注入 Text 块，避免工具巨型 JSON 污染搜索结果。",
          },
        ],
      },
      {
        role: "assistant",
        content: "",
        ts: now - 3_200_000,
        meta: {
          model: "gpt-5-codex",
          inputTokens: 24600,
          outputTokens: 1580,
          reasoningTokens: 2100,
          costUsd: 0.096,
          durationMs: 8400,
        },
        blocks: [
          {
            type: "thinking",
            summary: "**规划 INDEX_VERSION = 2 迁移与旧索引平滑重建**",
            text: "升级版本号后检测到 user_version 不匹配时自动重建 FTS5 虚拟表，仅提取 !msg.injected 的 Text 块写入索引…",
            durationMs: 4100,
            full: { kind: "jsonl", offset: 120, len: 480, pointer: "/codex/thinking" },
          },
          {
            type: "tool_call",
            id: "cx_sh_fail",
            rawName: "exec_command",
            kind: "shell",
            title: "cargo test search_index",
            inputPreview: '{"cmd":"cargo test search_index"}',
            inputTotalLen: 32,
          },
          {
            type: "tool_result",
            callId: "cx_sh_fail",
            status: "error",
            preview: "FAILED search_index::tests::skips_injected_context_blocks\nassertion `left == right` failed: expected 0 hits, got 1",
            totalLen: 115,
            lineCount: 2,
            truncated: false,
            exitCode: 101,
          },
          {
            type: "tool_call",
            id: "cx_patch",
            rawName: "apply_patch",
            kind: "edit",
            title: "E:/workspace/session-hub/src-tauri/src/session_manager/search_index.rs",
            inputPreview: '{"path":"src-tauri/src/session_manager/search_index.rs"}',
            inputTotalLen: 56,
            diff: {
              files: [
                {
                  path: "E:/workspace/session-hub/src-tauri/src/session_manager/search_index.rs",
                  op: "update",
                  added: 5,
                  removed: 2,
                },
              ],
              added: 5,
              removed: 2,
              full: { kind: "jsonl", offset: 800, len: 500, pointer: "/codex/diff" },
            },
          },
          {
            type: "tool_result",
            callId: "cx_patch",
            status: "success",
            preview: "Success. Updated 1 file.",
            totalLen: 24,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "cx_sh_ok",
            rawName: "exec_command",
            kind: "shell",
            title: "cargo test search_index -- --nocapture",
            inputPreview: '{"cmd":"cargo test search_index -- --nocapture"}',
            inputTotalLen: 48,
          },
          {
            type: "tool_result",
            callId: "cx_sh_ok",
            status: "success",
            preview: "test result: ok. 12 passed; 0 failed; finished in 0.48s",
            totalLen: 55,
            lineCount: 1,
            truncated: false,
            exitCode: 0,
            durationMs: 1120,
          },
          {
            type: "text",
            text: "已完成 `src-tauri/src/session_manager/search_index.rs:84` 的 `INDEX_VERSION = 2` 升级：\n\n1. 过滤 `message.injected == true` 的上下文消息，避免 `# AGENTS.md` 与 `<environment_context>` 造成误命中；\n2. 仅提取 `SessionBlock::Text` 建立 `trigram` 索引，体积下降约 **68%**；\n3. 注意本轮折叠状态下会自动置顶上方第一次失败的 `cargo test` 步骤，方便快速复盘排错过程（也可点击右上角「显示注入」查看隐藏的 `AGENTS.md`）。",
          },
        ],
      },
    ],
  };

  function buildFallbackMessages(session) {
    return [
      {
        role: "user",
        content: "",
        ts: (session.createdAt || now - 300_000),
        blocks: [
          {
            type: "text",
            text: "请执行任务：「" + session.title + "」，并在完成后运行回归校验。",
          },
        ],
      },
      {
        role: "assistant",
        content: "",
        ts: (session.lastActiveAt || now - 60_000),
        meta: {
          model: session.providerId + "-flagship",
          inputTokens: 14200,
          outputTokens: 890,
          costUsd: 0.038,
          durationMs: 3900,
        },
        blocks: [
          {
            type: "thinking",
            summary: "分析 " + (session.projectName || "项目") + " 存储结构与边界条件",
            text: "已扫描目标模块依赖关系与单测覆盖范围，准备应用最小侵入式改动并验证构建结果…",
            durationMs: 1850,
          },
          {
            type: "tool_call",
            id: "gen_r_" + session.sessionId,
            rawName: "Read",
            kind: "read",
            title: (session.projectDir || "E:/workspace/app") + "/src/index.ts",
            inputPreview: "{}",
            inputTotalLen: 2,
          },
          {
            type: "tool_result",
            callId: "gen_r_" + session.sessionId,
            status: "success",
            preview: "// module entry verified",
            totalLen: 24,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "gen_e_" + session.sessionId,
            rawName: "Edit",
            kind: "edit",
            title: (session.projectDir || "E:/workspace/app") + "/src/handler.ts",
            inputPreview: "{}",
            inputTotalLen: 2,
            diff: {
              files: [
                {
                  path: (session.projectDir || "E:/workspace/app") + "/src/handler.ts",
                  op: "update",
                  added: 3,
                  removed: 1,
                },
              ],
              added: 3,
              removed: 1,
              full: { kind: "jsonl", offset: 0, len: 180, pointer: "/generic/diff" },
            },
          },
          {
            type: "tool_result",
            callId: "gen_e_" + session.sessionId,
            status: "success",
            preview: "Updated src/handler.ts",
            totalLen: 22,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "text",
            text:
              "已完成 **" +
              session.title +
              "**。\n\n- 修改文件：`src/handler.ts:18`\n- 会话来源：`" +
              (session.sourcePath || "") +
              "`\n" +
              (session.summary ? "\n> " + session.summary : ""),
          },
        ],
      },
    ];
  }

  function getMessagesForSource(providerId, sourcePath) {
    const matched = sessions.find(
      (s) => s.providerId === providerId && s.sourcePath === sourcePath
    );
    const id = matched?.sessionId;
    if (id && transcriptsBySession[id]) {
      return transcriptsBySession[id];
    }
    if (matched) {
      return buildFallbackMessages(matched);
    }
    return transcriptsBySession["claude-jwt-fix-01"];
  }

  async function saveMarkdownInBrowser(defaultFilename, content) {
    const safeName = (defaultFilename || "session.md").endsWith(".md")
      ? defaultFilename
      : (defaultFilename || "session") + ".md";
    if (typeof window.showSaveFilePicker === "function") {
      try {
        const handle = await window.showSaveFilePicker({
          suggestedName: safeName,
          types: [
            {
              description: "Markdown 文档 (*.md)",
              accept: { "text/markdown": [".md"] },
            },
          ],
        });
        const writable = await handle.createWritable();
        await writable.write(content);
        await writable.close();
        return handle.name || safeName;
      } catch (err) {
        if (err && err.name === "AbortError") return null;
      }
    }
    const blob = new Blob([content], { type: "text/markdown;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = safeName;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    return safeName;
  }

  window.addEventListener("message", (event) => {
    if (event.data === "asm-demo-reset") {
      sessions = createInitialSessions();
      window.location.reload();
    }
  });

  window.__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { label: "main" },
    },
    transformCallback(callback, once) {
      const id = nextId++;
      cbs.set(id, (payload) => {
        if (once) cbs.delete(id);
        callback(payload);
      });
      return id;
    },
    unregisterCallback(id) {
      cbs.delete(id);
    },
    async invoke(cmd, args = {}) {
      if (cmd === "list_sessions") {
        if (sessions.length === 0) {
          sessions = createInitialSessions();
        }
        return { sessions: sessions.slice(), warnings: [] };
      }
      if (cmd === "get_pi_session_discovery") {
        return { status: "available" };
      }
      if (cmd === "set_window_theme") {
        return null;
      }
      if (cmd === "plugin:window|toggle_maximize") {
        try {
          if (document.fullscreenElement) {
            await document.exitFullscreen();
          } else {
            await document.documentElement.requestFullscreen();
          }
        } catch {
          // ignore if blocked by iframe permissions
        }
        return null;
      }
      if (cmd === "plugin:window|minimize" || cmd === "plugin:window|close") {
        return null;
      }
      if (cmd === "open_external_url") {
        if (args.url) {
          window.open(args.url, "_blank", "noopener,noreferrer");
        }
        return null;
      }
      if (cmd === "reveal_session_path") {
        throw "在线交互预览运行于浏览器沙盒中；安装桌面版即可在资源管理器定位：" + (args.path || "");
      }
      if (cmd === "save_markdown_export") {
        return await saveMarkdownInBrowser(args.defaultFilename, args.content || "");
      }
      if (cmd === "delete_session") {
        sessions = sessions.filter(
          (s) =>
            !(
              s.providerId === args.providerId &&
              s.sessionId === args.sessionId &&
              s.sourcePath === args.sourcePath
            )
        );
        return { status: "deleted" };
      }
      if (cmd === "delete_sessions") {
        const items = Array.isArray(args.items) ? args.items : [];
        return items.map((item) => {
          sessions = sessions.filter(
            (s) =>
              !(
                s.providerId === item.providerId &&
                s.sessionId === item.sessionId &&
                s.sourcePath === item.sourcePath
              )
          );
          return { ...item, success: true, warnings: [] };
        });
      }
      if (cmd === "sync_session_index") {
        return { running: false, processed: sessions.length, total: sessions.length };
      }
      if (cmd === "clear_session_index" || cmd === "cancel_session_content_search") {
        return null;
      }
      if (cmd === "search_session_messages") {
        const msgs = getMessagesForSource(args.providerId, args.sourcePath);
        const terms = String(args.query || "")
          .trim()
          .toLowerCase()
          .split(/\s+/)
          .filter(Boolean);
        if (!terms.length) return [];
        const matched = [];
        msgs.forEach((m, i) => {
          const text = (m.blocks || [])
            .filter((b) => b.type === "text")
            .map((b) => b.text)
            .join("\n")
            .toLowerCase();
          if (terms.every((t) => text.includes(t))) matched.push(i);
        });
        return matched;
      }
      if (cmd === "search_session_content") {
        const terms = String(args.query || "")
          .trim()
          .toLowerCase()
          .split(/\s+/)
          .filter(Boolean);
        const hits = sessions
          .filter((s) => !args.providerId || s.providerId === args.providerId)
          .flatMap((s) => {
            if (!terms.length || !s.sourcePath) return [];
            const msgs = getMessagesForSource(s.providerId, s.sourcePath);
            const snippets = [];
            let matchCount = 0;
            msgs.forEach((m, idx) => {
              if (m.injected) return;
              const text = (m.blocks || [])
                .filter((b) => b.type === "text")
                .map((b) => b.text)
                .join("\n\n");
              const lower = text.toLowerCase();
              if (!text || !terms.every((t) => lower.includes(t))) return;
              matchCount += 1;
              if (snippets.length < 3) {
                const pos = lower.indexOf(terms[0]);
                snippets.push({
                  messageIndex: idx,
                  text,
                  highlights: pos >= 0 ? [{ start: pos, end: pos + terms[0].length }] : [],
                });
              }
            });
            return matchCount > 0
              ? [{ providerId: s.providerId, sourcePath: s.sourcePath, snippets, matchCount }]
              : [];
          });
        return {
          hits,
          status: { running: false, processed: sessions.length, total: sessions.length },
        };
      }
      if (cmd === "get_session_messages") {
        return getMessagesForSource(args.providerId, args.sourcePath);
      }
      if (cmd === "stream_session_messages") {
        const raw = args.onChunk;
        const cbId =
          typeof raw === "string" && raw.startsWith("__CHANNEL__:")
            ? Number(raw.slice(12))
            : raw?.id;
        const send = cbs.get(cbId);
        const msgs = getMessagesForSource(args.providerId, args.sourcePath);
        if (send) {
          send({ index: 0, message: { kind: "header", total: msgs.length, approxBytes: 8192 } });
          send({ index: 1, message: { kind: "messages", start: 0, messages: msgs } });
          send({ index: 2, message: { kind: "done", delivered: msgs.length } });
          send({ index: 3, end: true });
        }
        return null;
      }
      if (cmd === "get_session_image") {
        return getDemoDiagramDataUrl();
      }
      if (cmd === "get_session_block_content") {
        const ptr = args.contentRef?.pointer || "";
        if (ptr === "/claude/diff") {
          return {
            text: "diff --git a/src/auth/jwt.ts b/src/auth/jwt.ts\n--- a/src/auth/jwt.ts\n+++ b/src/auth/jwt.ts\n@@ -46,3 +46,6 @@\n-const key = jwksCache.get(header.kid);\n-if (!key) throw new UnauthorizedError('unknown kid');\n+let key = jwksCache.get(header.kid);\n+if (!key) {\n+  await refreshJwks({ force: true });\n+  key = jwksCache.get(header.kid);\n+}\n",
            totalLen: 260,
            truncated: false,
          };
        }
        if (ptr === "/codex/diff") {
          return {
            text: "*** Begin Patch\n*** Update File: E:/workspace/session-hub/src-tauri/src/session_manager/search_index.rs\n@@ -81,4 +81,7 @@\n-const INDEX_VERSION: i32 = 1;\n-for msg in &transcript.messages {\n+const INDEX_VERSION: i32 = 2;\n+for msg in transcript.messages.iter().filter(|m| !m.injected) {\n+    let body = extract_text_blocks_only(msg);\n+    if body.is_empty() { continue; }\n*** End Patch\n",
            totalLen: 340,
            truncated: false,
          };
        }
        if (ptr === "/generic/diff") {
          return {
            text: "diff --git a/src/handler.ts b/src/handler.ts\n--- a/src/handler.ts\n+++ b/src/handler.ts\n@@ -16,2 +16,4 @@\n-export const ready = false;\n+export const ready = true;\n+export const checkedAt = Date.now();\n+export const version = '1.8.1';\n",
            totalLen: 210,
            truncated: false,
          };
        }
        if (ptr === "/codex/thinking") {
          return {
            text: "完整推理链（Codex）：\n1. 旧版 FTS5 将整条消息原文写入 trigram 表，导致大型 tool_result JSON 与注入的 AGENTS.md 占用了超过 70% 的索引体积。\n2. 将 PRAGMA user_version 提升至 INDEX_VERSION = 2，在打开索引库时若版本不一致自动 DROP 并重建虚拟表。\n3. 索引器严格跳过 msg.injected，且仅拼接 SessionBlock::Text，使跨会话正文搜索既快又准。",
            totalLen: 238,
            truncated: false,
          };
        }
        return {
          text: "完整思考链（Claude Code）：\n1. 签发服务每 24 小时轮换一次 RSA/Ed25519 密钥对（新 kid=kid-2026-11）。\n2. 边缘网关虽然缓存了 JWKS（TTL=15m），但 jwt.ts 第 48 行遇到本地未知 kid 时直接抛出了 401，没有触发回源拉取。\n3. 在未知 kid 分支调用带 inflightRefresh 单飞锁与 5s 冷却窗口的 refreshJwks({ force: true })，即可彻底消除轮换窗口期的偶发 401，同时防止恶意伪造随机 kid 击穿上游。",
          totalLen: 256,
          truncated: false,
        };
      }
      return null;
    },
  };
})();
