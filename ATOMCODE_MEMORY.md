# claw-code 项目架构记忆

> **这是给 AtomCode 自己看的工作记忆文档。** 下次接手本项目时，**首先读这个文件**，可以快速还原项目全貌、已知坑点和修复历史。
> 最后更新：2026-06-30

---

## 一句话定位

**claw-code** 是一个用 Rust 重写的 Claude Code 兼容 CLI（`claw.exe`），通过 Anthropic Messages API 协议接入大模型，目标是能在**联通云 GLM-5.1** 这类 OpenAI-compat 后端上跑通完整的 agentic 编码工作流。项目由用户 henry 维护，分支 `henry-dev`。

---

## 仓库结构（只有 `rust/` 是活的）

```
claw-code/
├── rust/                      ← ★ 唯一活跃实现，Rust workspace
│   ├── Cargo.toml             workspace 根，members = crates/*
│   └── crates/
│       ├── api/               HTTP client + Anthropic 协议层（types/sse/providers）
│       ├── runtime/           ★★ 核心：工具实现、会话、权限、compact、MCP
│       ├── rusty-claude-cli/  ★ 二进制入口 main.rs（15000+ 行，含 CLI 渲染）
│       ├── commands/          slash 命令
│       ├── tools/             工具 trait/聚合（薄层）
│       ├── plugins/           hook 插件系统
│       ├── mock-anthropic-service/  测试用 mock SSE 服务
│       ├── compat-harness/    mock parity 测试框架
│       └── telemetry/         遥测（薄）
├── src/                       Python 遗留代码，基本不动，与 Rust 实现并存
├── tests/                     与 src/ 配套的验证面
├── CLAUDE.md                  项目指引（Rust 栈，rust/ 下验证）
├── ARCHITECTURE.md            ★ 之前的修复记录（GLM SSE 兼容、权限尾部斜杠）——读它补全历史
└── scripts/fmt.sh             格式化/检查脚本
```

**记忆锚点**：所有真正的工作都在 `rust/crates/runtime/src/` 和 `rust/crates/api/src/`。`rusty-claude-cli/src/main.rs` 巨大且包含所有工具结果的终端渲染逻辑。

---

## 关键模块速查（runtime/src/）

| 文件 | 作用 | 修改时注意 |
|------|------|-----------|
| `file_ops.rs` | **read_file / write_file / edit_file / glob_search / grep_search** 全部在这 | ★ 路径解析、目录过滤、工作区边界都在此；改动需同步看 `tools/` 和 `main.rs` 渲染 |
| `micro_compact.rs` | 轻量压缩：每次 API 调用前清空旧 tool result 内容 | 常量 `MIN_OUTPUT_LENGTH_FOR_CLEAR=5000`、`PROTECT_RECENT_TOOL_RESULTS=8`；改小会导致"降智"。⚠️ 文件内注释称"GLM has no server-side cache_edits"——**此为注释层断言，无独立证据**（见下方 2026-07-09 核实记录） |
| `compact.rs` | 全量 auto-compact（达到阈值时） | 触发阈值由 env `CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE=75` / `WINDOW=131000` 控制 |
| `session.rs` | `Session` / `ConversationMessage` / `ContentBlock` 数据结构 | micro_compact 依赖此处的类型 |
| `permissions.rs` | 权限规则匹配、`allowed_path_prefixes` | 之前修过尾部斜杠 bug（见 ARCHITECTURE.md） |
| `sse.rs` | runtime 侧 SSE 解析 | 与 `api/src/sse.rs` 不同 |
| `mcp_stdio.rs` / `mcp_tool_bridge.rs` | MCP 服务器进程管理 | ⚠️ 用了 `std::os::unix::PermissionsExt`，**Windows 上编译 lib test target 会失败**（预存问题，非本次引入） |

---

## 关键模块速查（api/src/）

| 文件 | 作用 |
|------|------|
| `types.rs` | Anthropic 协议类型；`MessageResponse.kind`/`role` 已改为 `Option` 以兼容 GLM |
| `sse.rs` | ★ SSE 流解析；有 `parse_stream_event` helper 把上游 `{"type":"error"}` 转成 `ApiError::Api{status:500}`，避免 `unknown variant` 崩溃 |
| `providers/anthropic.rs` | ★ GLM 实际走这条路径；含 `log_request_size` 埋点 |
| `providers/openai_compat.rs` | OpenAI-compat 路径；GLM **不走**这条（GLM 走 anthropic.rs） |
| `client.rs` / `http_client.rs` | HTTP 客户端 |

---

## 运行时配置（用户的 .claw.json，关键 env）

用户工作目录 `E:\NW工程\资料库\html`，配置摘要：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://aigw-gzgy2.cucloud.cn:8443",  // 联通云网关
    "ANTHROPIC_MODEL": "glm-5.1",
    "CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE": 75,
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": 131000
  },
  "hooks": {
    "PreToolUse":  ["python .claw/hooks/keyword_restorer.py"],  // 写入前还原脱敏词
    "PostToolUse": ["python .claw/hooks/keyword_redactor.py"]   // 读取后脱敏
  },
  "permissions": {
    "allow": [
      "grep_search(E:/内网工程/资料库AI版/sirchmunk/.../:*)",
      "read_file(E:/内网工程/.../:*)",
      "glob_search(E:/内网工程/.../:*)",
      "bash(ls E:/内网工程/.../:*)"
    ],
    "defaultMode": "dontAsk"
  },
  "plugins": { "maxOutputTokens": 32000 }
}
```

**关键点**：
- 用户的代码常在 `E:\NW工程\资料库\html`（PHP 资料库前端）和 `E:\内网工程\资料库AI版\sirchmunk\`（Python AI 检索后端）两处。**两条路径都含中文**。
- 用户的 sirchmunk 项目含 `.venv/Lib/site-packages/`（数万文件）——grep 必须过滤。
- 关键词脱敏 hook 会替换文件内容里的敏感词为占位符（`词23_4dc1478b` 形式）；**不要尝试读取或绕过 `.claw/hooks/`**。

---

## 诊断日志体系（已埋点）

日志文件：`E:\NW工程\资料库\html\claw_glm_diag.log`（claw 进程 cwd 下）。三种事件：

| 事件 | 含义 | 来源 |
|------|------|------|
| `claw_glm_diag` | 每次 API 请求：status、error、完整 request_body | `api/src/providers/anthropic.rs` |
| `claw_request_size` | 每次请求字节数 + est_tokens | `anthropic.rs` 的 `log_request_size`（GLM 实际生效）；`openai_compat.rs` 有同名但 GLM 不走 |
| `claw_microcompact` | 每次清空：cleared 数、chars_freed、protected | `runtime/src/micro_compact.rs` |

读日志用 PowerShell 脚本（cmd.exe 下直接传 PS 命令会被引号吞掉，**必须写成 .ps1 文件再 `powershell -File`**）。仓库根有几个现成脚本：`analyze_log.ps1`、`analyze_log2.ps1`、`read_log_tail.ps1`、`count_log.ps1`。

---

## ★ 已修复问题全记录（按时间倒序）

### 2026-06-30 grep 工具三轮修复（本次会话）

全部在 `rust/crates/runtime/src/file_ops.rs`。

| 轮 | 症状 | 根因 | 修复 |
|----|------|------|------|
| 1 | 中文路径 `os error 2`，grep 直接报错 | `normalize_path` → `canonicalize()` 对正斜杠+中文路径失败 | `canonicalize` 失败时回退：先试 parent canonicalize + join filename，再不行返回原始路径 |
| 2 | grep "0 matches across N files"（明明有匹配） | 默认 `files_with_matches` 模式下 `num_matches` 是 `None`，CLI 渲染 `unwrap_or(0)` | 默认分支和 content 分支都填 `Some(total_matches)` |
| 3 | grep 卡死不动 | 路径通了后 `collect_search_files` 无过滤遍历含 `.venv` 的项目（数万文件逐个 read_to_string+regex） | `collect_search_files` 加 `should_skip_glob_dir` 过滤；扩充 `GLOB_SEARCH_IGNORED_DIRS` |
| 4 | `Context window blocked`（1M tokens 爆炸退出） | grep 搜到 `.claw/sessions/*.jsonl`（含历史 read_file 完整内容），结果灌入上下文 | 忽略列表加 `.claw` |

**最终 `GLOB_SEARCH_IGNORED_DIRS` 列表**：`.git, node_modules, .build, target, dist, coverage, .claw, .venv, venv, __pycache__, .tox, .mypy_cache, .pytest_cache, .cache, cargo-target, .gradle, .mvn`

### 2026-06-28（之前会话，见 ARCHITECTURE.md）

1. **权限规则尾部斜杠**：`allowed_path_prefixes()` 和 `validate_workspace_boundary_impl()` 统一处理尾部斜杠
2. **GLM SSE 兼容**：`MessageResponse.kind/role` 改 `Option`；`sse.rs` 兼容纯 JSON 格式
3. **micro_compact 调优**：阈值 500→5000，保护窗口 2 轮→8 个 tool result（按 ID 去重，HashSet）
4. **SSE error 事件**：`parse_stream_event` 把 `{"type":"error"}` 转成 `ApiError::Api{status:500}` 而非崩溃
5. **request_size 埋点**：`anthropic.rs`（GLM 实际生效）和 `openai_compat.rs`（GLM 死代码）各一份 `log_request_size`

### 2026-07-09 核实记录（本次会话）

**背景**：2026-06-28 第 3 条"micro_compact 调优"时，`micro_compact.rs:56/86` 注释断言"GLM has no server-side cache_edits"作为保护窗口调到 8 的理由。本轮被用户追问该断言依据，进行核实。

**核实结论**：
1. **claw-code 请求体从不包含 Anthropic 的 `cache_control` 字段**。`MessageRequest`（`api/src/types.rs:7-44`）、`InputMessage`、`InputContentBlock` 的全部变体（Text/Thinking/ToolUse/ToolResult）均无 `cache_control`/`CacheControl`/`ephemeral` 子字段。
2. **全 `rust/` 树 grep `cache_control|CacheControl|"ephemeral"|cache_edits` 零命中**（除 `micro_compact.rs` 那两行注释自身）。`cache_creation_input_tokens`/`cache_read_input_tokens` 仅出现在 `Usage` 响应结构体里，是回执 token 计数，不是请求字段。
3. **`api/src/prompt_cache.rs` 的 `PromptCache` 是纯本地磁盘完成缓存**（存储路径 `$HOME/.claude/cache/prompt-cache/`，FNV-1a 哈希请求体命中即跳过 API 直接返回旧响应），**不是** Anthropic server-side prompt caching，不能作为 GLM 是否支持 server-side cache 的证据。
4. 因此"GLM 无 server-side cache_edits"**在 claw-code 语境下无法被证伪**——client 从未构造过该字段去测试任何后端。注释反映的更可能是用户 henry 的实战经验判断（清掉的内容找不回来），而非经技术验证的结论。

**实际影响**：调参后果（保护窗口 8）对 claw-code 是**永远安全**的策略——client 从不依赖任何 server-side cache，无论 GLM 是否支持都无差别。但断言本身不应被当作"已验证事实"引用。

**已同步更新**：本文件第 46 行 `micro_compact.rs` 那条的"修改时注意"列已加脚注指向本记录。

---

## ⚠️ 已知预存问题（不要在本会话修，但要知道）

1. **`mcp_stdio.rs` / `mcp_tool_bridge.rs` 用 `std::os::unix::PermissionsExt`** —— Windows 下 `cargo test -p runtime --lib` 编译失败（8 个 E0433/E0599）。`cargo check` 和 release build 不受影响。要跑 file_ops 测试需 WSL/Linux，或给这两个文件加 `#[cfg(unix)]` 守卫。
2. **`plugins/src/hooks.rs` 有 `unused import std::path::Path`** —— 无害 warning。
3. **`claw_request_size` 在 `openai_compat.rs` 是死代码**（GLM 走 anthropic.rs）—— 可后续抽到共享模块。
4. **`src/` Python 遗留** —— 与 Rust 实现并存，CLAUDE.md 要求两边保持一致，但实际 Rust 是唯一活跃路径。

---

## 开发与验证流程

```bash
# 格式检查
scripts/fmt.sh --check

# 快速类型检查（Windows 可用，~3s）
cd rust && cargo check --workspace

# Release 构建（~40s）—— 注意：claw.exe 运行时会被占用，构建前先关进程
cd rust && cargo build --release
# 产物：rust/target/release/claw.exe

# Clippy（Windows 下 lib test target 会因 mcp_stdio 失败）
cd rust && cargo clippy --workspace --all-targets -- -D warnings

# 测试（同上，Windows 下 runtime lib test 编译不过）
cd rust && cargo test --workspace
```

**Windows 坑点**：
- shell 是 cmd.exe，PowerShell 命令要通过 `powershell -NoProfile -ExecutionPolicy Bypass -File xxx.ps1` 调用，直接传 `-Command` 会被 cmd 引号转义吞掉变量
- 没有 `tail`/`wc`，用 PowerShell `Get-Content -Tail N` 或 `[System.IO.File]::ReadAllLines()`
- 中文路径在 `Path::new` 能创建，但 `canonicalize()` 不稳定 —— 这就是 normalize_path 回退逻辑的由来

---

## Git 状态备忘

- 分支：`henry-dev`，ahead of origin by 1 commit（用户自己的 `c17d2d7` micro_compact 提交）
- **我（AtomCode）的所有改动都是 unstaged**，从未自动 commit/push
- 修改过的文件（截至本次会话）：`file_ops.rs`、`anthropic.rs`、`.gitignore`，外加几个 untracked 的 `.ps1` 诊断脚本
- 提交时按用户指示，commit message 末尾加：
  ```
  Co-Authored-By: AtomCode (GLM-5.2) <noreply@atomgit.com>
  ```

---

## 下次接手清单

1. **先读本文件** 还原全貌
2. **读 ARCHITECTURE.md** 补 2026-06-28 的修复细节
3. **`git status` + `git log --oneline -5`** 看当前状态
4. **`cargo check --workspace`** 确认编译基线
5. 如果用户报新 bug，**先看 `claw_glm_diag.log` 尾部**（用 .ps1 脚本读），再读代码
6. 改 `file_ops.rs` 时记得同步看 `rusty-claude-cli/src/main.rs` 里的 `format_grep_result` / `format_glob_result` 渲染
7. 改路径/权限相关逻辑时，**务必用中文路径测试**（`E:/内网工程/...` 是现成的测试用例）
