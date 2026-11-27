# 会话查看与清理的安全边界

## 删除前

- 完全退出对应 Agent，包括桌面窗口、编辑器插件后台服务（如 `codex-app-server`）和终端中的 Agent 任务。管理器在 Windows 上按当前桌面会话检查进程，在 Linux 上直接读取 `/proc`（避免 `ps comm` 15 字节截断与空格歧义），并识别已知原生二进制、平台后缀进程、Claude 会话锁文件及 Node / Bun 启动参数；检查失败或发现运行中的 Agent 时拒绝清理，不会自动结束任何进程。
- 手动备份重要的会话数据。会话文件和数据库记录的删除是永久操作，不进入回收站。
- 会话记录不等于项目文件。管理器已移除“同时删除项目目录”选项，后端也拒绝此类请求。需要清理项目文件时，请在文件管理器中单独确认和处理。

批量删除仅在本批次内复用同一 Agent 一秒以内的成功进程检查；超过一秒或此前检查失败会重新检查，不跨批次缓存。

进程检查是保护措施，不是与原 Agent 的互斥协议。无法保证识别重命名的进程、自定义运行器，也无法阻止检查后又启动的 Agent。清理期间请勿重新启动对应 Agent，或使用多个管理器实例同时清理。

## 清理范围与失败重试

- 删除当前会话时，也会清理扫描到的同一 Agent、同一原生会话 ID 的历史副本。单独删除历史副本时，仅处理该副本；不会删除其他会话或整个项目。
- Codex 清理自动发现最高代 `state_<N>.sqlite` 与 `thread_history_<N>.sqlite`（兼容 `sqlite_home` 与 `CODEX_SQLITE_HOME`），同步清理伴生库（`logs_<N>.sqlite`、`memories_<N>.sqlite` 等）及关联隐藏 subagent rollout，再删除会话文件；归档会话不再仅因未出现在活跃列表中就被当作垃圾。历史副本不会清除仍指向另一份文件的有效索引。
- DeepSeek Harness（遵循 `DSH_HOME`，默认 `~/.dsh`）清理要求先完全退出 `dsh`。管理器先原子移除 `workspace.json` 中的精确会话引用，再删除对应会话目录、投影缓存与 `session-search.db` 全文索引记录；活动主机定时任务（兼容 `schedule.json` v1 与 v2）、会话内活动定时任务、损坏的父子关系及仍有子会话依赖的记录会拒绝删除。Harness 内部子 Agent 会话以“残留”显示，列表行标出它的归属（「某某会话的子会话」），删除父会话时确认框会询问是否连同全部子会话一起删除（默认勾选，可取消）；勾选后按“子会话先于父会话”的顺序一次完成，无需再手动逐条清理。取消勾选时若父会话仍被依赖，删除会被拒绝并说明原因，不会留下孤儿。
- DeepSeek Harness 的依赖扫描遇到目录读取错误、未跟随的目录链接/联接点、会话文件类型异常或最高代文件冲突时，允许展示已成功扫描的记录并报告警告，但拒绝据此执行删除。删除及重试均要求当前代文件与原目标路径一致；发现另一代文件时不会顺带删除，需重新扫描确认。
- DeepSeek Harness 的依赖检查和批量排序仅限量读取会话头（4 MiB），每项删除前仍重新枚举并读取依赖信息，不使用跨请求缓存。目标会话仍完整读取并校验事件与活动定时任务（128 MiB 上限）；工作区索引读取限制为 16 MiB。此优化不取消退出 Agent 和禁止并发写入的要求。
- ZCode（遵循 `ZCODE_DATA_BASE_DIR` 与 `ZCODE_STORAGE_DIR`，默认 `~/.zcode`）清理会同时核对主数据库的 `session.parent_id`、`session_task_link` 与桌面任务索引的 `forked_from_task_id`（仅对仍存在的会话计入依赖，已软删除记录标为残留）。仍被子会话或活动自动化任务引用的父会话会拒绝删除；界面展示子会话归属并支持勾选「同时删除子会话」，按先子后父顺序清理并同步移除 `session_task_link` 关联行。
- Pi 清理兼容 `@mariozechner` 与 `@earendil-works` 发行的 `pi-coding-agent` 进程守卫，并递归识别同目录原生分支及 `forks` 目录中的独立分支会话。`parentSession` 表示来源，分支自身保存复制的正文，不是运行时依赖：单独删除父会话不会隐式删除或阻止独立分支；父文件已缺失的分支仍作为普通会话保留。删除会话文件后若其所在项目子目录（或 `forks` 子目录）已无剩余文件，管理器会在会话存储根边界内自动回收空目录（不触碰非空目录或符号链接）。扫描范围内可解析的父子关系（自动兼容软链接与 Windows 短路径/大小写）用于界面归属显示；选择「同时删除子会话」时仍按确认清单处理，批量选择时先处理子分支。
- OpenCode 清理同时兼容 2.0+ `session_v2` / `session_message` 与旧版 `session` SQLite 表及旧版 JSON 存储，核对父会话字段。子会话会作为可清理的隐藏/残留记录展示，并标出所属父会话；仍有子会话时拒绝删除父会话，确认框可勾选「同时删除子会话」一次清理整条链，批量选择父子会话时先处理子会话。旧版 JSON 清理还会一并移除对应的 `session_diff`、`todo` 与 `share` 边车文件。
- Claude Code 的 `/branch`/`--fork-session` 会复制出拥有独立 ID 的完整会话，不依赖原会话，因此不设置父会话删除阻止。扫描仅限 `<projects>/<project>/<session>.jsonl` 层级，并识别 `sessions-index.json` 中缺失源文件的幽灵索引项；删除会话时校验安全会话 ID 与目录层级，同步更新 `sessions-index.json`，并一并清理同名 sidecar 目录（`subagents`、`tool-results`）、同项目归属该会话的扁平 `agent-*.jsonl`，以及 `~/.claude/`（`CLAUDE_CONFIG_DIR`）下同 `sessionId` 的 `file-history/`（编辑前文件快照）、`todos/`、`tasks/`、`session-env/`、`debug/` 与 `history.jsonl` 对应记录（清理后无法再通过 `/rewind` 使用这些历史文件快照）。
- Grok Build（遵循 `GROK_HOME` 与 `GROK_CONFIG_DIR`，默认 `~/.grok`）清理会读取 `summary.json` 的 `parent_session_id` 与可选 `session_kind`，识别 subagent 与归档副本。仍有子会话依赖的活动父会话会拒绝删除（主会话仍在时允许单独清理归档历史副本），确认框可勾选「同时删除子会话」一次清理整条链，批量选择时先删子会话。
- WorkBuddy（遵循 `WORKBUDDY_HOME`，默认同时扫描 `~/.workbuddy-ai` 与 `~/.workbuddy`）扫描合并 `workbuddy.db`（识别 `deleted_at > 0` 的已软删除残留会话）、`projects/<project>/<session>.jsonl` 对话正文与 `workspace/sessions/<uuid>` 孤立文件快照残留。清理前会检查 `automation_runtime_state` 与 `automation_delivery_outbox`，拒绝删除正在运行或仍有待投递结果的自动化会话；确认删除后一并移除 `workbuddy.db`（`sessions`、`session_usage`、`automation_runs`、`cloud_conversations`）、`edge-sync-mapping*.db`、`sidebar-list-snapshot.json` 缓存项、项目 `.jsonl` 与 `.file-rollback.ndjson`、同名 subagents/tool-results 边车目录，以及 `workspace/sessions/`、`file-history/`、`changes-index/`、`changes-detail/`、`artifact-index/`、`file-tree-manifests/`、`tasks/` 与过期会话锁文件。
- Gemini CLI（遵循 `GEMINI_CLI_HOME`，默认 `~/.gemini/tmp`）、GitHub Copilot CLI（遵循 `COPILOT_HOME`，默认 `~/.copilot/session-state`）、CodeBuddy CLI（遵循 `CODEBUDDY_CONFIG_DIR`，默认 `~/.codebuddy/projects`）、Kimi CLI（遵循 `KIMI_SHARE_DIR` / `KIMI_CODE_HOME`，默认 `~/.kimi/sessions`）与 OpenClaw（遵循 `OPENCLAW_STATE_DIR`，默认 `~/.openclaw/agents`）均严格校验存储根下的相对层级、安全会话 ID 与符号链接 / 重解析点边界：CodeBuddy 同步清理 `.file-rollback.ndjson` 与同名边车目录，Gemini 同步清理同 stem 的 `.jsonl` / `.json` 副本，Copilot 与 Kimi 清理对应会话目录，OpenClaw 原子移除 `sessions.json` 中指向该会话的索引项并删除会话文件。
- 路径派生操作校验存储根目录、会话 / 消息 ID 以及符号链接与 Windows NTFS 目录联接点（Reparse Point）边界；ZCode 清理限于所选会话，不按项目级联删除其他任务。OpenCode 旧版 JSON 清理在任何删除之前，核对消息 ID 与文件名、`sessionID` 与所选会话归属；缺失、冲突、读取失败或目录链接均拒绝执行，消息元数据合计读取上限为 32 MiB。
- 开始修改前保存清理清单。索引、附属文件或清理记录未能处理完毕时，返回“待清理”，不显示为完整删除成功。即使原始会话记录已经消失，管理器仍保留可重试的入口。
- 重试使用原始清理清单，不把后来出现的同 ID 副本追加到已有操作中。先解决提示中的锁定、权限或数据格式问题，再点击“继续清理”。清单里已经删掉的条目（例如主会话已删、同 ID 历史副本清理失败）在重试时会跳过预检，只处理仍然存在的副本，否则重试会永久卡在“文件已不存在”上。
- 路径派生操作只摘掉符号链接或目录联接点本身，不跟随链接：目标位置若是一个链接，删除不会触及链接指向的真实数据。链接本身或中间目录链接越界时同样拒绝。
- 会话存储位置来自 `CODEX_HOME`、`CODEX_SQLITE_HOME`、`CLAUDE_CONFIG_DIR`、`DSH_HOME`、`WORKBUDDY_HOME`、`GROK_HOME`（兼容 `GROK_CONFIG_DIR`）、`XDG_DATA_HOME`、`OPENCODE_DB`、`ZCODE_DATA_BASE_DIR`、`ZCODE_STORAGE_DIR`、`PI_CODING_AGENT_DIR`、`PI_CODING_AGENT_SESSION_DIR` 等环境变量。清理要求存储根是绝对路径；相对路径（`PI_CODING_AGENT_DIR` 在浏览时同样拒绝相对值）会相对进程工作目录解析，因此清理会拒绝执行并提示改为绝对路径。

Windows 的清理记录位于 `%LOCALAPPDATA%\AgentSessionManager\pending-deletions`；其他平台位于系统本地应用数据目录下的同名子目录。它只保存定位信息和错误，不是会话内容备份；请勿手动删除未完成的清理记录。记录损坏时，正常会话列表仍可浏览，并显示损坏记录的路径与警告；按 Agent 标识的损坏记录仅暂停对应 Agent 的删除，无法归属的损坏文件则全局拒绝删除，避免失去重试依据。

原 Agent 的多个数据库、JSON 索引和文件无法形成一个跨存储事务。待清理记录用于在部分失败后继续收尾，不提供撤销或内容恢复。原 Agent 数据格式升级后仍可能需要更新适配器；如果原 Agent 重启后仍有侧边栏残留，请保留错误信息和清理记录，避免手动批量删库。

## 查看与检索边界

- 列表区分归档、历史副本和待清理状态。管理器不直接启动外部终端进程执行命令；在资源管理器 / 文件管理器中定位路径前会校验目标（或其存在的父目录）为真实本地绝对路径。
- 开启跨会话「正文」检索时，管理器在系统本地应用数据目录 `AgentSessionManager/session-index.db` 维护独立的 SQLite FTS5 全文索引（仅收录用户提问与助手回复纯文本，排除思考块、工具输出与注入上下文，按文件指纹与 WAL 摘要增量同步）；删除会话时同步清理对应索引行，关闭「正文」开关时立即删除索引库及 `-wal`/`-shm` 边车。
- 列表分页每页 100 条；“全选筛选结果”包含其他分页，选择与删除都不受当前页限制。确认框逐条列出待删会话；超过 50 条时清单列不全，未列出的部分会标注数量，并要求勾选“我已核对”才能确认——是知情确认，不是禁止：直接按数量拦掉会让超过一页的选择完全无法删除。子会话在行内与确认框里都标出归属（来自扫描到的父会话 ID，父会话不在当前结果时退回原生 ID 前 8 位）；「同时删除子会话」会连带删除整条子会话链（含隔代），范围逐条列出，取消勾选即只删除已选中的那些会话。级联范围用**完整扫描结果**计算而不是当前筛选/搜索的结果：把子会话藏在筛选之外并不代表它不存在，漏掉它只会让父会话的删除被预检拒绝（失败即拒绝，不会留下孤儿）。单条或批量删除失败时确认框不会消失，保留剩余失败会话供退出 Agent 后直接重试。打开会话时按正文实际依赖文件的修改时间、大小与内容摘要复用上次解析结果：对 SQLite `-wal` 边车，以及体积不超过 16 MiB 且最近 2 秒内修改（或缺失时间戳）的常规源文件额外比对内容摘要，删除会话后正文缓存整体作废。指纹覆盖已核对读取范围的 Agent：Claude Code、Codex、DeepSeek Harness、Grok Build、Pi、WorkBuddy，以及 OpenCode、ZCode 与 WorkBuddy 的 SQLite 源（库文件与 `-wal` 边车）。Codex 的分页历史链还会把链的顺序与每段截断偏移计入缓存身份。OpenCode 旧版消息目录不缓存。正文按每块最多 256 KB 或 150 条流式下发，超过 4096 字符的正文下发前 1500 字符预览，展开或复制代码块时按定位与身份摘要取回全文；会话内 `Ctrl+F` 搜索直接检索后端缓存中的完整未截断正文，模型 `thinking` / `reasoning` 思考过程默认折叠展示并支持一键展开。扫描警告不代表没有会话，读取失败也不等于空会话。
- SQLite 单次消息读取限制为每表 20,000 条 / 32 MiB，OpenCode 旧版 JSON 会话合计读取限制为 32 MiB，整文件消息读取限制为 128 MiB，扫描时单行元数据限制为 4 MiB。超限会提示，不应通过删除会话解决；请用原 Agent 查看。
