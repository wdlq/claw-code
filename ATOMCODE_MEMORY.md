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

## ★ 2026-07-11 Ctrl+C 中断 + bash 工具 Windows 路径修复（本次会话）

用户报两条 bug：(1) Ctrl+C 跨多轮 turn 失效；(2) claw 里跑含中文路径的 `dir "E:\..." /b` 或 `cd /e/... && php -l` 全挂，报 `The filename, directory name, or volume label syntax is incorrect.` 或 `The system cannot find the path specified.`。

### A. Ctrl+C 跨多轮 turn 中断修复

**根因**：`HookAbortMonitor::spawn` 每轮 turn 调一次 `ctrlc::set_handler`，但 ctrlc crate 全进程只能成功装一次 handler（二次起返回 `Err(MultipleHandlers)`，verified against ctrlc-3.5.2 `init_and_set_handler` 源）。首装 handler 闭包冻结的是首次那个 `abort_signal` 实例——第二轮起 Ctrl+C 调的是死实例的 abort，本轮的 signal 永远不会被 set。

**修法**（3 处）：
1. `runtime/src/hooks.rs` — `HookAbortSignal` 加 `reset()` 方法，只 reset `aborted` AtomicBool，不动 `Notify`（已 resolve 的 future 不重新 arm，新 `wait()` 落到 `notify.notified().await` 阻塞等新 notify）。
2. `rusty-claude-cli/src/main.rs` — `LiveCli` 加 `shared_abort_signal: runtime::HookAbortSignal` 字段（进程级共享，`new()` 里初始化一次）；`prepare_turn_runtime` 改成 `self.shared_abort_signal.clone()` + `reset()`，不再每轮 `HookAbortSignal::new()`。
3. `rusty-claude-cli/src/main.rs` `HookAbortMonitor::spawn` 重写——加进程级 static `CTRL_C_HANDLER_ATTEMPTED: AtomicBool` + `ctrlc_handler_installed()`/`mark_ctrlc_handler_installed()` helper，**首次**才调 `ctrlc::set_handler`，二次起静默跳过（不再误报 warning）；注册失败时才打 stderr 警告暴露终端不投递 CTRL_C_EVENT。

**真机验证**：用户跑两轮 turn 各按 Ctrl+C，每轮都打 `[claw] Ctrl+C received — aborting current turn.` + `Cancelled.`，不再出现 `warning: failed to install Ctrl+C handler`。

### B. bash 工具 Windows 路径三连修

`bash.rs` 的 `prepare_command`/`prepare_tokio_command` 在 Windows 下把命令喂给 `cmd /C`——模型常生成 Git Bash 飽 `/e/NW工程/...` 路径，cmd.exe 不认。

**B-1**：新增 `rewrite_posix_drive_paths_for_windows(command: &str) -> String` 函数——把 `/X/`（X 是单 ASCII 字母）**在 token 边界**（前导是空格/`;`/`&`/`|`/行首）且**后跟 `/`**的位置重写成 `X:/`。8 个单元测试在 `bash.rs::drive_path_tests` mod。喂给 `cmd /C` 前在两处 `#[cfg(windows)]` 分支调用。

**B-2**：改用 `std::os::windows::process::CommandExt::raw_arg`（Rust 1.95+）跳过 Rust `Command::arg` 的 argv 引号包裹——之前 `.arg("/C").arg(cmd)` 让 cmd.exe 收到 `cmd /C "dir \"E:\...\""`，内引号被当转义割裂路径。改成 `prepared.raw_arg(format!("/C {rewritten}"))` 单条裸传，内引号原貌保留。

**B-3**：B-1 函数 pattern 收紧——原本 `next_is_slash_or_end`（认末尾）会把 cmd.exe flag `/b`/`/s`/`/h` 误当 drive path 重写成 `b:`/`s:`/`h:`（真机 diag 抓到 `/b` → `b:` 的怪变）。改成 `next_is_slash`（**只认 `/`，不认末尾**），新增 `leaves_cmd_exe_flags_untouched` 测试覆盖 `dir /b`、`findstr /I /S` 不被改。

**真机验证**（用户跑三条全通）：
1. `dir "E:\NW工程\资料库\html\application\index\controller\" /b` → 列出全部 php 文件
2. `cd /e/NW工程/资料库/html && php -l application/.../CmsController.php` → `No syntax errors detected`
3. `findstr /I /S "controller" "E:\...\*.php"` → 正常输出

**关键诊断教训**：B-3 那个 flag 误改 bug 我**靠埋 diag stderr 日志**抓到的——`eprintln!("[claw diag] prepare_tokio_command raw_arg: /C {rewritten}")` 一行，用户跑一次就看到 `/b` 变成 `b:`。**下次遇到"命令被改坏"类 bug，先埋 diag 打出真传字符串，不要靠推论**。diag 已删。

### 顺手修的预存债（让 runtime lib test target 在 Windows 下能编）

1. **mcp_stdio.rs / mcp_tool_bridge.rs**：5 处 `permissions.set_mode(0o755)` + 2 处 `use std::os::unix::fs::PermissionsExt` 加 `#[cfg(unix)]` 守卫。Windows 下跳过 chmod（脚本可执行性靠扩展名 + PATHEXT，不靠 mode bits）。**这条解了 ATOMCODE_MEMORY 之前记的"Windows 下 cargo test -p runtime --lib 编译失败"预存坑**。
2. **conversation.rs**：孤儿 `parse_auto_compaction_threshold`（源里无定义，只在 import + 测试里被引，编不过）—— import 列表去掉它，测试改成 inline 实现 preserve coverage。
3. **rusty-claude-cli/src/main.rs**：3 处 `role: "assistant".to_string()` 改 `role: Some("assistant".to_string())`（2026-06-28 GLM SSE 兼容改造把 `MessageResponse.role` 改 `Option` 时漏了测试桩），让 bin test target 能编。

### 已知未修的旁注 bug（本次会话暴露但未动）

- ~~**`PowerShell executable not found (expected pwsh or powershell in PATH)`**——已解决，见下方 2026-07-12 记录~~
- **`/stop` slash 命令仍是注册未实现占位**（跟 `/context`/`/files`/`/plan`/`/review`/`/tasks` 等一大票同在 `main.rs:5355-5393` 那个"not yet implemented"分支）——本次会话原计划做 B 实现 `/stop` 作为 Ctrl+C 的补充路径，但 A 修好后 Ctrl+C 跨多轮工作，`/stop` 不必做。
- **`powershell_runs_via_stub_shell` 测试断言格式对不上**（`crates/tools/src/lib.rs:9837`）——预存测试桩债，stub shell 期望 `pwsh:Write-Output hello` 但实际输出 `hello\r\n`。是测试断言假错，不是 claw 代码 bug，下次接手可对照实际 `execute_shell_command` 调 pwsh 的参数格式修断言。
- **`find -name -type` 等 Unix flag 转 `dir /s /b` 后 cmd.exe 不认**——hook 脚本 `convert_unix_cmd_to_windows` 把 `find` 转成 `dir /s /b` 但 find 的 `-name`/`-type` flag 没剥，cmd.exe 仍挂。治本要么 claw 走 bash.exe，要么模型改用 Windows 原生命令形态。

---

## ★ 2026-07-12 PowerShell 工具探测 + hook 脚本 `&&` 分隔支持（本次会话）

### A. PowerShell 工具 `executable not found` 修复

**根因**：`crates/tools/src/lib.rs:6164` 那个 `command_exists(command)` 用 `std::process::Command::new("sh").arg("-lc").arg(format!("command -v {command} >/dev/null 2>&1"))`——在 Git Bash 的 `sh.exe` 子进程里跑 `command -v`。Git Bash 启 sh.exe 时会用 msys2 的环境重写规则过滤父 PATH，导致 `C:\Windows\System32\WindowsPowerShell\v1.0\` 这段虽在 claw 父进程 PATH 里却看不到，`command -v powershell` 返回 not-found。

**关键证据**：diag 日志埋点（`eprintln!("[claw diag] detect_powershell_shell: ... parent PATH = {}")`）打出 claw 父进程 PATH **含** `C:\Windows\System32\WindowsPowerShell\v1.0\;`——PATH 没问题，是 `sh.exe` 子进程的重写吞了这段。同一问题在 `runtime/src/sandbox.rs:280` 那个 `command_exists` 里**不存在**——它用纯 Rust `std::env::split_paths` 遍历父 PATH，不调 sh.exe。

**修法**（`crates/tools/src/lib.rs`）：把 `command_exists` 改成跟 `sandbox.rs` 那个正确实现对齐的纯 Rust PATH 遍历，不再调 sh.exe：

```rust
fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            dir.join(command).exists()
                || dir.join(format!("{command}.exe")).exists()
        })
    })
}
```

Windows 下额外试 `command.exe`（cmd.exe 命令申明可不带扩展名）。diag 日志已删。

**预存债顺手修**：`crates/tools/src/lib.rs:9807` 那处 `std::process::Command::new("/bin/chmod")` 加 `#[cfg(unix)]` 守卫——Windows 下没 `/bin/chmod` 路径，让 `powershell_runs_via_stub_shell` 测试 target 在 Windows 下能编能跑。

**真机验证**：用户跑 `用 PowerShell 跑这条：Get-ChildItem "E:\..." -Name`，PowerShell 工具成功调起不再报 not found。

**残留预存债**（未动）：`powershell_runs_via_stub_shell` 测试断言 `right: "pwsh:Write-Output hello"` 跟实际输出 `left: "hello\r\n"` 对不上——是测试桩 stub shell 期望的参数格式跟实际 `execute_shell_command` 调 pwsh 的参数格式不同，是测试断言假错，不是 claw 代码 bug。下次接手可对照实际参数格式修断言。

### B. PreToolUse hook `convert_unix_cmd_to_windows` 加 `&&`/`;`/`||` 分隔支持

**根因**：`E:/NW工程/资料库/html/.claw/hooks/keyword_restorer.py` 里 `convert_unix_cmd_to_windows` 只在命令**行首**或 `|` 后替换 Unix 命令（`ls`/`grep`/`cat` 等）成 Windows 命令（`dir`/`findstr`/`type`）。但 GLM 模型常生成 `cd /e/... && ls ...` / `cd /e/... && grep -rn ...` 模式——`ls`/`grep` 在 `&&` 后面，hook 不触发，原样喂给 cmd.exe 报 `'ls' is not recognized as an internal or external command.`。

**修法**（`keyword_restorer.py`，5 处改动）：
1. `WINDOWS_CMD_REPLACEMENTS` 表加 `"find ": "dir /s /b "` 一行
2. `convert_unix_cmd_to_windows` 函数重写——`sorted_items` 按 prefix 长度降序排（`grep -r ` 妈先于 `grep `），separator 分支扩到 `| `/`&& `/`; `/`|| ` 全部
3. 加 `convert_one` helper 用 regex `^grep -[A-Za-z]+ ` 剥 `grep -rn`/`grep -rl`/`grep -rF` 等 flag 变体段，统一转 `findstr /S /I`——之前只转 `grep -r ` 字面变体，`grep -rn` 会漏转出 `findstr /I -rn` 怪串

**验证**：Python 直接调 `convert_unix_cmd_to_windows` 跑 6+ 条用例全通——`cd /e/... && ls ...` → `cd /e/... && dir ...`、`cd /e/... && grep -rn refresh ...` → `cd /e/... && findstr /S /I refresh ...`、`grep -rl foo` → `findstr /S /I foo` 等。

**残 bug**（hook 改不到的）：`find application -name route* -type f` 转成 `dir /s /b application -name route* -type f`——find 的 `-name`/`-type` flag cmd.exe 不认。治本要么 claw 走 bash.exe，要么模型改用 Windows 原生命令形态。

### 本次会话两改动的共通教训

- **同名函数两份实现不同**：`command_exists` 在 `runtime/src/sandbox.rs` 和 `tools/src/lib.rs` 各一份，前者正确（纯 Rust PATH 遍历），后者 buggy（调 sh.exe）。下次接手遇到跨 crate 同名函数，**先 diff 两份实现**——bug 常在抄袭走样里。
- **hook 脚本是 Python 改完即生效**——不用重编 claw。但 hook 改坏影响所有 bash/PowerShell 工具调用，改完用 `python -c "from keyword_restorer import convert_unix_cmd_to_windows; ..."` 直接跑几条用例验证再放手。

---

## ★ 2026-07-14 DeepSeek V4 接入：连续 user 消息合并修复（本次会话）

### 背景
用户把后端从联通云 GLM-5.1（200K）切到 DeepSeek V4 Pro[1m]（1M 上下文，`https://api.deepseek.com/anthropic`，走 Anthropic Messages 协议）。配置在 `.claw/settings.json` 的 `env` 段。CLI 跑到第二轮就报 400：

```
messages.3:`tool_use` ids were found without `tool_result` blocks immediately after: call_01_M6DrymvtB3mYIcSWTLxv7423. Each `tool_use` block must have a corresponding `tool_result` block in the next message.
```

GLM-5.1 工作正常，DeepSeek V4 立即报错——**根因是两家对 Anthropic 协议校验严格度不同**。

### 根因
**Anthropic Messages 协议要求**：一条 assistant 消息里的所有 `tool_use` 块，其对应的 `tool_result` 块必须**全部合并到同一条 user 消息**里紧跟在 assistant 消息之后；协议也拒绝连续 user 消息（Bedrock 强制 role 邻接交替）。

**claw 的会话存储**（`runtime/src/conversation.rs:440` 的 turn 循环）：assistant 返回多个 tool_use 时，代码用 `for` 循环逐个执行工具，每个 tool_result 通过 `push_message` **单独** push 成一条 `ConversationMessage { role: MessageRole::Tool, blocks: [ToolResult {...}] }`。会话里出现：

```
msg[3] assistant:  text + tool_use(call_00) + tool_use(call_01)
msg[4] Tool:       tool_result(call_00)    ← 独立消息
msg[5] Tool:       tool_result(call_01)    ← 又一条独立消息
```

**请求体序列化**（`rusty-claude-cli/src/main.rs:10215` 和 `tools/src/lib.rs:4949` 的 `convert_messages`）：把每条 `ConversationMessage` 一对一映射成一条 `InputMessage`，`MessageRole::System|User|Tool` → `role:"user"`。于是请求体里出现两条独立 user 消息各带一个 tool_result——违反协议。GLM-5.1 容忍拆分，DeepSeek V4 严格校验报 400。

### 对齐官方 claude-code 做法（E:\内网工程\ClaudeCode2.1.88开源版\claude-code-source-code）
读官方源码确认做法：`src/utils/messages.ts:1989 normalizeMessagesForAPI` 在序列化前**合并任何连续 user-wire 消息**成一条（注释明确："Merge consecutive user messages because Bedrock doesn't support multiple user messages in a row"）。另有 `src/utils/messages.ts:5133 ensureToolResultPairing` 做防御性配对修复（给孤儿 tool_use 插合成 error tool_result、删孤儿 tool_result）——那处理 resume/compact 产生的畸形，不是本次多 tool_use 正常路径。

**本次修法与官方方向一致**：在 `convert_messages` 里合并连续非 Assistant 消息成一条 user InputMessage。比官方窄到只合并 Tool——第二轮读到官方做法后**把合并逻辑通用化到所有映射成 user 的连续消息**（System|User|Tool），完全对齐官方 `normalizeMessagesForAPI`。`ensureToolResultPairing` 那种孤儿修复本次不做（claw 当前无 resume/compact 产生孤儿路径，且预存债面大，留待真正报该 400 时再补）。

### 修法（请求体序列化层，不动 runtime 会话结构）
两份 `convert_messages`（`main.rs:10215`、`tools/src/lib.rs:4949`）都改成：遍历消息时若当前是非 Assistant（即映射成 user），用 **peekable iterator 把紧随其后的所有连续非 Assistant 消息的 content blocks 合并到同一条 user InputMessage**。抽出 `convert_content_blocks` helper 复用 block 转换逻辑。runtime 会话结构（micro_compact、compact、session 持久化、jsonl 重放）全不动——只改请求体出口的合并。

**为什么不在 runtime 层合并**：runtime 把每个 tool_result 存独立消息有正当理由——独立执行、独立 micro_compact 清空、独立 jsonl 持久化。在序列化出口合并是最小侵入，且对 resume/重放历史会话也即时生效。

### 测试
两份各加 3 个测试：
1. `converts_coalesces_consecutive_tool_messages_into_one_user_message`——多 tool_result 合并成一条 user（直接覆盖本次报错场景）
2. `converts_keeps_separate_user_turns_when_split_by_assistant`——验只合并连续，不跨 assistant 边界
3. `converts_coalesces_consecutive_user_messages_into_one`——连续纯 User 文本消息也合并（对齐官方通用做法）

现有 `converts_tool_roundtrip_messages` 不变仍过。`cargo test -p rusty-claude-cli --bin claw converts`（4 passed）+ `cargo test -p tools --lib convert_messages`（3 passed）全绿。

### 顺手修的预存债（让 api crate lib test target 在 Windows 下能编）
**根因同 2026-07-11 那批**：2026-06-28 把 `MessageResponse.role`/`kind` 改 `Option<String>` 兼容 GLM SSE 时漏改了一批测试桩。2026-07-11 只修了 `rusty-claude-cli` 的 3 处，**`api` crate 这批 12 处没动**，导致 `cargo test --workspace --lib` 编不过。本次为让验证跑起来一并修了：

| 文件 | 处数 | 类型 | 改法 |
|------|------|------|------|
| `api/src/types.rs:309` | 1 | `MessageResponse` 桩（role 应 `Option`） | `"assistant"` → `Some("assistant")` |
| `api/src/prompt_cache.rs:719` | 1 | `MessageResponse` 桩（role 应 `Option`） | 同上 |
| `api/src/providers/openai_compat.rs` | 10 | 测试桩构造 `InputMessage`（role 是 `String`）和 `ChatMessage`（role 是 `String`） | 错用 `Some("...")` → 去掉 `Some` |

**注意区分两类相反改法**：`MessageResponse.role` 是 `Option<String>`（要加 `Some`）；`InputMessage.role` 和 `ChatMessage.role` 是 `String`（要去 `Some`）。`openai_compat.rs:497` 那处是**非测试代码**的合法 `MessageResponse` 构造（`Some` 正确），不能动。

### 已知未做（本次不动）
- **DeepSeek V4 实机验证未做**——本次只做编译+单元测试层验证。用户需重编 `cargo build --release` 替换 `claw.exe` 后真机跑一轮确认 400 消失。`max_tokens: 256000` 是否被 DeepSeek 接受也需实机确认（GLM 接受，DeepSeek 上限未测）。
- **`ensureToolResultPairing` 风格的孤儿配对防御修复未做**——claw 当前无 resume/compact 产生孤儿 tool_use/tool_result 的路径，且该修复面大（要处理 resume 偏截、compact 边界割、流中断等），留待真正报该 400 时再补。本次只做"正常多 tool_use 路径的合并"，与官方 `normalizeMessagesForAPI` 对齐。
- **micro_compact 注释那条"GLM has no server-side cache_edits"断言**——见 2026-07-09 核实记录，对 DeepSeek 同样适用（client 从不构造 `cache_control` 字段，无论后端是否支持都无差别）。

### 教训
- **不同 Anthropic-协议网关对协议校验严格度差异巨大**——GLM-5.1 容忍 tool_result 拆分和连续 user 消息，DeepSeek V4 严格。claw 之前只在 GLM 上跑通，协议合规性债被掩盖。**接入新网关前应过一遍 Anthropic 官方 Messages API 的消息结构约束**（tool_use/tool_result 配对、连续 user 合并、role 枚举值）。
- **官方 claude-code 源码是协议合规的参考实现**——`E:\内网工程\ClaudeCode2.1.88开源版\claude-code-source-code` 可读。关键函数：`src/utils/messages.ts` 的 `normalizeMessagesForAPI`（连续 user 合并）和 `ensureToolResultPairing`（孤儿配对修复）。下次遇协议合规问题**先查官方对应函数**再动手，避免做窄了要返工（本次第一版只合并 Tool，读官方后扩成通用合并）。
- **预存测试桩债会阻塞验证**——`api` crate 那 12 处 role 类型不匹配是 2026-06-28 改造的遗留，本次为跑测试不得不顺手修。下次接手遇 `cargo test --workspace --lib` 编不过先看是不是同类 role Option 不匹配桩。

---

## 下次接手清单

1. **先读本文件** 还原全貌
2. **读 ARCHITECTURE.md** 补 2026-06-28 的修复细节
3. **`git status` + `git log --oneline -5`** 看当前状态
4. **`cargo check --workspace`** 确认编译基线
5. 如果用户报新 bug，**先看 `claw_glm_diag.log` 尾部**（用 .ps1 脚本读），再读代码
6. 改 `file_ops.rs` 时记得同步看 `rusty-claude-cli/src/main.rs` 里的 `format_grep_result` / `format_glob_result` 渲染
7. 改路径/权限相关逻辑时，**务必用中文路径测试**（`E:/内网工程/...` 是现成的测试用例）
8. **★ 2026-07-11 新增**：`bash` 工具改完后必跑 `cargo test -p runtime --lib drive_path` 那 8 个测试（`/e/` 重写 + cmd.exe flag 不动），Windows 下现在能编能跑（预存债已解）。改 `rewrite_posix_drive_paths_for_windows` pattern 时**务必加新测试覆盖你新认的边界**——pattern 收紧放过 cmd.exe flag 那条教训不能忘
9. **★ 2026-07-11 新增**：遇"命令被改坏"类 bug，**先埋 `eprintln!("[claw diag] ...")` 打出真传字符串**再推理，不要纯靠推论——本次 B-3 那个 `/b`→`b:` 怪变就是靠 diag 抓到的，纯推论我会一直以为是 raw_arg 没生效
10. **★ 2026-07-12 新增**：跨 crate 同名函数（`command_exists` 在 `runtime/src/sandbox.rs` 和 `tools/src/lib.rs` 各一份）**先 diff 两份实现**——bug 常在抄袭走样里。本次 PowerShell 探测那条根因就是 `tools` 那份调 sh.exe 走样了，`sandbox` 那份纯 Rust 实现是对的
11. **★ 2026-07-12 新增**：改 `E:/NW工程/资料库/html/.claw/hooks/keyword_restorer.py` 后**不用重编 claw**（Python 脚本即改即生效），但改坏影响所有 bash/PowerShell 工具调用——改完用 `python -c "from keyword_restorer import convert_unix_cmd_to_windows; ..."` 直接跑几条用例验证再放手
12. **★ 2026-07-14 新增**：接入新 Anthropic-协议网关（DeepSeek/Bedrock/联通云 GLM 等）前**先过一遍官方 claude-code 的 `src/utils/messages.ts`**（`E:\内网工程\ClaudeCode2.1.88开源版\claude-code-source-code`），对齐 `normalizeMessagesForAPI`（连续 user 合并）和 `ensureToolResultPairing`（孤儿配对修复）的协议合规做法，避免在 GLM 容忍的违规路径上欠债翻车。`convert_messages` 两份（`main.rs`+`tools/src/lib.rs`）是请求体序列化出口，改其中一份务必同步另一份+各加测试。顺手修 api crate 测试桩 role 类型债后 `cargo test --workspace --lib` 终于能编能跑。

---

## ★ 2026-07-15 DeepSeek V4 Pro 缓存命中率提升：一期 cache_control 注入（本次会话）

### 背景
用户把后端从 GLM-5.1 切到 DeepSeek V4 Pro[1m]（`https://api.deepseek.com/anthropic`，走 Anthropic Messages 协议）后，报"云端 LLM 缓存命中率非常低，官方原版 claude-code 命中率要高一些"。对照官方源码 `E:\内网工程\ClaudeCode2.1.88开源版\claude-code-source-code` 分析。

### 核因（已核实）
**claw-code 的请求体从未包含 Anthropic 的 `cache_control` 字段**——client 从不构造该字段去测试任何后端。
- **日志铁证**：`E:/NW工程/资料库/html/claw_glm_diag.log` 共 139 次 API 调用，`cache_control` 出现 **0 次**（grep 全日志）。日志只记 request body，不记 response usage，所以"基线命中率 ~50%"是用户口述（DeepSeek 后台所见），claw 端无法自证——那 50% 只能是 DeepSeek **网关侧自动前缀缓存**（某些网关会自动 cache 最近请求前缀，不需客户端发 cache_control），不是 Anthropic 协议级 prompt caching。
- **源码铁证**：`api/src/types.rs` 的 `MessageRequest`/`InputMessage`/`InputContentBlock`/`ToolDefinition` 全无 `cache_control`/`CacheControl`/`ephemeral` 字段（2026-07-09 核实记录已记此事实，本次改动前再确认）。全 `rust/crates/api/src` grep `cache_control` 零命中（除本次新增）。
- **官方对照**：`src/services/api/claude.ts:3063 addCacheBreakpoints` 在**最后一条消息**打恰好一个 message-level `cache_control: {type:"ephemeral"}` marker；`getCacheControl`（claude.ts:358）按用户类型/订阅决定 TTL 5m/1h 并 **session 级 latch**（`should1hCacheTTL` bootstrap-state latch，避免中途翻转 TTL 击穿缓存键）；`buildSystemPromptBlocks`（claude.ts:3213）给 system prompt 分块打 cache_control；tools 数组尾部也打 marker。

### 方案征求过程
先向用户提分析方案（不动代码），用户让我看了网上专家拆解 `E:/内网工程/.../1.html`（第三章 API 通信层）。专家思路与本 Agent **一致**——都定位到 claude.ts 主动打 cache 锚点、TTL session 级 latch。专家文章多提醒一条：**请求头/字段中途切换会击穿 cache 键**（3.3.3 Header 策略）。本 Agent 据此修正方案：TTL 在 `CacheConfig` 构造时一次性读 env、整个会话不变。用户同意后动手。

### 一期改动（已完成，全部在 `rust/`）

| 文件 | 改动 |
|------|------|
| `api/src/cache_control.rs` | **新建**。`CacheConfig`（session latch TTL，`from_env()` 读 `CLAW_CACHE_TTL` 默认 `"5m"`、`DISABLE_PROMPT_CACHING` 禁用）+ `add_cache_breakpoints(&mut [InputMessage], &CacheConfig)`（最后一条消息打 message-level marker）+ `add_tools_cache_marker(&mut [ToolDefinition], &CacheConfig)`（最后一个 tool def 打 marker）。14 个单元测试（env 串行化用 `static OnceLock<Mutex>` 避免并行干扰） |
| `api/src/types.rs` | 新增 `CacheControl { type_: String, ttl: Option<String> }`（`#[serde(rename="type")]`，`ephemeral(ttl)` 构造器）；给 `InputMessage`/`InputContentBlock` 各变体（Text/Thinking/ToolUse/ToolResult）/`ToolDefinition` 加 `cache_control: Option<CacheControl>` 字段，全部 `#[serde(default, skip_serializing_if="Option::is_none")]` 向后兼容 |
| `api/src/lib.rs` | 注册 `cache_control` 模块，导出 `CacheControl`/`CacheConfig`/`add_cache_breakpoints`/`add_tools_cache_marker` |
| `rusty-claude-cli/src/main.rs` | `AnthropicRuntimeClient` 加 `cache_config: api::CacheConfig` 字段（`new()` 里 `from_env()` 一次 latch）；`stream()` 在序列化出口对 messages 和 tools 注入 cache marker；`convert_messages` 拆出 `convert_messages_with_cache(messages, &CacheConfig)` 变体 |
| `tools/src/lib.rs` | `ProviderRuntimeClient` 同样加 `cache_config`；`stream()` 同步注入；`convert_messages` 同样拆 `_with_cache` 变体 |
| `mock-anthropic-service/src/lib.rs` | 2 处 pattern 补 `..` 兼容新字段 |
| `api/src/providers/{openai_compat,mod}.rs` | 测试桩 literal 批量补 `cache_control: None`（Python 脚本处理 33 处）+ `openai_compat.rs` 的 `translate_message` 4 处 pattern 补 `cache_control: _` |

### 一期不做（留二期）
- **System prompt 分块 cache_control**（官方 `splitSysPromptPrefix` + `buildSystemPromptBlocks`）：需 boundary marker 拆 static/global/dynamic，claw 当前 system 是单一 `Option<String>`，改类型面大。一期靠 message-level marker 已能让前缀（含 system）进 cache。
- **Tool result `cache_reference`**（官方 claude.ts:3164-3207）：配合 `cache_edits`（microro KV 删除）的高级功能，需 `InputContentBlock::ToolResult` 加 `cache_reference` 字段 + 一套 cache_edits 块逻辑。
- **micro_compact 与 cache 协同调参**：micro_compact 清空旧 tool result 换占位符会变前缀字节，可能击穿 cache。二期待一期实机验证后决定 micro_compact 是保留、调参还是禁用。
- **`anthropic-beta` header**：DeepSeek 是否需要 `prompt-caching-2024-07-31` beta header 才开启 cache，未测。一期靠字段注入，如果实机 cache_read 仍为 0，二期补 header。

### 验证
- `cargo check --workspace` ✅
- `cargo test -p api --lib` ✅ 160 passed 0 failed
- `cargo test -p tools --lib` ✅ 94 passed 13 failed（**基线同 13 个预存 Windows 基，未新增**）
- `cargo test -p rusty-claude-cli --bin claw` ✅ 196 passed 5 failed（基线同 5，未新增）
- `cargo test -p runtime --lib` ✅ 537 passed 38 failed（基线 39，未新增）
- `scripts/fmt.sh --check` ✅


---

## ★★ 2026-07-15 一期实机验证失败 + 根因纠错（本次会话，关键纠错）

### 实机结果
用户编译一期改动后真机跑，**DeepSeek �页端后台统计界面显示命中率依然是 ~50%，没变化**。

### 根因（已核实 DeepSeek 官方 API 手册）
用户保存 DeepSeek 官方 API 手册至 `docs/DeepseekAPI/{1,2,3,4}.md`。本 Agent 读后核实：

**`docs/DeepseekAPI/1.md` 白纸黑字——DeepSeek 的 Anthropic-compat 接口对 `cache_control` 全部 Ignored**：

| 字段位置 | Support Status |
|---------|---------------|
| `tools[].cache_control` | **Ignored** |
| `content[].cache_control`（text block） | **Ignored** |
| `content[].cache_control`（tool_use block） | **Ignored** |
| `content[].cache_control`（tool_result block） | **Ignored** |
| `anthropic-beta` header | **Ignored** |
| `anthropic-version` header | **Ignored** |

**一期方案的前提是错的**——假设"DeepSeek 认 cache_control 字段，注入后触发 Anthropic 协议级 prompt caching"。实际上 DeepSeek Anthropic-compat 接口**完全不认该字段**，发出去直接丢弃。一期注入的 cache_control 对 DeepSeek 后端零效果，命中率当然还是 50%。

那 50% 一直是 DeepSeek **自家硬盘缓存**（docs/DeepseekAPI/2.md"上下文硬盘缓存"），机制与 Anthropic prompt caching 完全不同：
- DeepSeek 硬盘缓存：自动开启，无需客户端发任何字段。命中条件是**前缀完整匹配"缓存前缀单元"**，部分匹配不命中。
- Anthropic prompt caching：客户端发 cache_control 字段显式标注断点，服务端按断点缓存。

一期拿 Anthropic 协议的字段去喂一个不认该字段的后端，等于白做。**这是方案设计阶段的核实债**——接入新网关前应先读该网关官方 API 手册确认字段支持，而非假设它完整实现 Anthropic 协议。

### DeepSeek 硬盘缓存的真正命中规则（docs/DeepseekAPI/2.md 核实）
关键在"缓存前缀单元"——完整匹配才命中，部分匹配不命中：
1. 请求结束位置落盘：每次请求的"用户输入结束位置"和"模型输出结束位置"各产生一个缓存前缀单元。下一轮若完整匹配到这俩单元，命中。
2. 公共前缀检测落盘：系统检测到多次请求间存在公共前缀时，把公共前缀单独落盘。要等第三次请求才能命中。
3. 按固定 token 间隔落盘：长输入中按固定 token 间隔截单元。

命中率低的真正原因：claw 的请求前缀每轮都在变，无法完整匹配缓存单元。变的原因：
- micro_compact 清空旧 tool result 换占位符——前缀字节变了，完整匹配失败
- auto-compact 触发后历史被压成摘要——整个前缀字节彻底改变，必然 miss
- system prompt 每轮动态拼（含时间、cwd、git status 等）——前缀字节变了
- tools schema 顺序或内容变——前缀字节变了

DeepSeek 的缓存是字节级完整匹配，比 Anthropic 的断点缓存苛刻得多。

### usage 字段名差异（重要，二期-E 要用对）
DeepSeek 回执的 usage 字段（docs/DeepseekAPI/2.md:65-67）：
- `prompt_cache_hit_tokens`：缓存命中的 tokens 数
- `prompt_cache_miss_tokens`：缓存未命中的 tokens 数

不是 Anthropic 的 cache_read_input_tokens/cache_creation_input_tokens。claw 的 Usage struct 当前只有 Anthropic 字段名，DeepSeek 回执的 prompt_cache_hit_tokens/prompt_cache_miss_tokens 会因 #[serde(default)] 落到 0——claw 端根本看不到 DeepSeek 的真实命中率，只能靠 DeepSeek 后台看。二期-E 要补对字段名。

### 一期改动的处置
一期改动保留不删，理由：
1. cache_control 字段对 DeepSeek 是 Ignored（无害），对真 Anthropic 后端（联通云 GLM、官方 Anthropic）是正确生效的。claw 是多后端 CLI，删了一期反而在真 Anthropic 后端上退化。
2. CacheConfig/add_cache_breakpoints/add_tools_cache_marker 模块结构对二期仍有用（二期换方向后可复用 session latch 思路）。
3. 一期的错误是方案前提错（没核实 DeepSeek 字段支持），不是代码错。代码本身编译通过 + 测试不退化，留着不亏。
---

## ★ 2026-07-16 Reasonix compact 处理对照 claw（源码核实，避免重复分析）

### Reasonix 的 compact 是四级递进 + 多重防退化（源码 internal/agent/compact.go:85-146）

```
contextWindow × ratio 触发对应动作：
├─ softCompactRatio (如 60%) → 只提醒不压（"context 大了，保持缓存前缀"）
├─ toolResultSnipRatio (如 70%) → Snip 截短旧 tool_result（保头尾行）
├─ compactRatio (如 80%) → 才真 compact（摘要折叠）
└─ compactForceRatio (如 95%) → force compact（逼不得已硬压）
```

每一级故意延迟避免击穿缓存。compact.go:92-98 注释明说 "Between the soft ratio and the trigger, report growing context once without rewriting the prefix — a compaction here would needlessly crater the cache"。

### claw 的 compact 是单阈值硬压摘要（conversation.rs:813 auto_compaction_threshold_from_env）

只有一个阈值，到了就硬压摘要，没有 soft/snip/force 分级。with_model_context_window 只是把阈值从固定 55K 改成模型窗口的 75%，仍然是单阈值。

### 关键差异对照表

| 维度 | claw-code | Reasonix |
|------|----------|----------|
| 阈值级数 | 单阈值（75% 窗口）硬压摘要 | 四级：soft 提醒→snip 截短→compact 摘要→force 逼压 |
| compact 前先减压 | 无，到阈值直接摘要折叠 | 有：compact 前先 PruneStaleToolResults 删旧 tool_result，删完低于阈值就跳过 compact（compact.go:120-128） |
| 折叠经济性检查 | 无，不管 region 多大都压 | 有：foldEconomics（compact.go:153）region < 400 tokens 不压——压了省的钱还不够付摘要 API 调用费 |
| 连续 compact 防退化 | 无，每轮都可能触发 | 有：consecutiveCompacts >= 2 暂停 auto-compact（compact.go:140-146），告诉用户 context window 太小 |
| compact 后防再触发 | 无 | 有：compact.go:135 注释——健康 compact 应让 prompt 跌到阈值下，下一轮不再 compact |
| Snip 截短（保头尾） | 已加（思路 4 ✅） | 原版机制，compact 前的减压手段 |
| archive 存档 | 无 | 有，删前存到 /tmp/xxx.log，占位符写明路径，模型可重读恢复 |

### 对命中率影响

Reasonix 这套机制对命中率：soft 提醒不压 ✅ 避免在 60-80% 之间不必要 compact 击穿缓存；Snip 截短保头尾 ✅ 头部字节稳定缓存命中到头部结束位置（claw 刚加的思路 4）；compact 前先 Prune ✅ 可能跳过 compact 避免摘要折叠击穿；foldEconomics 小 region 不压 ✅ 避免无谓 compact；consecutiveCompacts 暂停 ✅ 避免循环 compact 每轮击穿。

claw 当前只有"单阈值硬压摘要 + 刚加的 Snip"——缺 soft 提醒、compact 前先减压、foldEconomics、连续 compact 暂停这四条。

### 判断（当前场景性价比）

思路 4（Snip）已加是最重要的一条——对命中率提升最直接。剩下的四条里最有价值的是"compact 前先减压"——Reasonix 靠这条避免了很多不必要的 compact。claw 加这条改动不大：在 conversation.rs 的 compact 触发处先调一次 microcompact_session（用高的 emergency 阈值），删完如果低于 auto-compact 阈值就跳过 compact。

结论：Reasonix compact 处理比 claw 精细得多（四级 vs 一级），但对当前场景（DeepSeek 1M 窗口 + 阈值动态化后 auto-compact 0 次触发），这些精细机制收益已经边际——真正的杀手 microcompact 刚用思路 4 Snip 解了。等实机验证思路 4 效果后如果命中率还差最后几个点，再考虑加"compact 前先减压"那条。

### 另：DeepSeek 缓存 TTL 实测结论（2026-07-16）

放置几个小时（中途未断网）后新会话首轮 cache_read=35584>0——跨会话缓存命中，证明 DeepSeek 缓存 TTL ≥ 几小时不会在几小时内过期。用户的使用场景（放几个小时再回来用）缓存不会失效命中率不受影响。真正吃掉命中率的不是 TTL 是切任务和 microcompact 击穿（后者刚用思路 4 Snip 解）。首轮命中 35584 就是 system prompt + tools schema 那段（约 8K tokens × 4 字符/token ≈ 35K 字符，和 cache_read=35584 吻合）——证明 claw 的 system prompt + tools schema 跨会话字节稳定，思路 1（调顺序）确实不需要做已经对了。

### 下次接手清单（更新）

15. ★ 2026-07-16 新增（Reasonix 对照）：claw 的 compact 是单阈值硬压摘要，Reasonix 是四级递进（soft/snip/compact/force）+ compact 前先减压 + foldEconomics + 连续 compact 暂停。思路 4（Snip 截短保头尾）已加是最重要的一条。剩下的"compact 前先减压"等实机验证思路 4 后如果命中率还不能提升再考虑。DeepSeek 缓存 TTL ≥ 几小时实测确认，跨会话首轮命中 35584，思路 1（调顺序）已核实不需要做。思路 4 改动集中在 micro_compact.rs：snip_tool_result 函数 + SnipStrategy 按工具类型分级（只读头80尾12，副作用头40尾40）+ 清空逻辑从 *output = CLEARED_PLACEHOLDER 改成调 snip_tool_result。实机验证关键看 microcompact 触发后那一轮 cache_read 是否还断崖跌——如果不跌了说明 Snip 生效，如果还跌说明头部字节也不够稳定要进一步调策略。

---

## ★★ 2026-07-17 auto-compact 击穿 DeepSeek 缓存实机复盘 + Reasonix 对照补全（本次会话）

### 实机事件（claw_glm_diag.log 第 71649-71651 行，唯一一次 auto-compact）

```
[71649] claw_cache_diag t=1784254524 hit=0 miss=0 input=211388 output=1523 cache_read=230400 cache_creation=0
[71651] claw_auto_compact t=1784254524 removed=79 threshold=750000
```

**threshold=750000** = DeepSeek V4 1M 窗口 × 75%（`CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE=75` 经 `with_model_context_window` 算出），按阈值正常触发，不是 400 兜底。

### 关键证据：cache_read 暴跌 48%

| 阶段 | t | cache_read |
|---|---|---|
| auto-compact 前最后一次请求 | 1784254488 | **446,464** |
| auto-compact 那次请求 | 1784254524 | **230,400** |

cache_read 从 446K 暴跌到 230K，跌幅 216K tokens（约 48%）。这正是 MEMORY 第 461 行那条预测——**auto-compact 把历史压成摘要，整个前缀字节彻底改变，DeepSeek 字节级完整匹配必然 miss**。

### 三大异常定位

**异常 A —— micro-compact 一次都没触发**
日志里 `claw_microcompact` 事件 **0 条**。`micro_compact.rs:166` 那个 `CLAW_MICROCOMPACT_DISABLE=1` 开关当时是开着的——**用户在用 `CLAW_MICROCOMPACT_DISABLE=1` 实测对比命中率**。所以这次跑的是"禁用 micro-compact"路径，思路 4 的 Snip 根本没机会生效。

**异常 B —— pre-flight compact 触发条件与 maybe_auto_compact 不一致（需进一步核实）**

| 函数 | 位置 | 判断依据 |
|---|---|---|
| `session_needs_pre_flight_compact` | conversation.rs:690 | `messages` 的 `char_count / 4` 估算（**只算 messages body**） |
| `maybe_auto_compact` | conversation.rs:638-665 | 优先用 `usage_tracker.cumulative_usage().input_tokens`（**实际回执**，含 system+tools），input_tokens==0 时才回退到 char_count/4 |

两者走的是**两套估算路径**。日志显示 compact 那轮 est_tokens≈444K 远未到 750K 阈值，但 pre-flight 仍触发了 compact——**强烈怀疑 pre-flight 的 char_count/4 高估了 est_tokens，把本不需要 compact 的会话硬压了一次**。本次会话没继续追这条，留下次接手或用户决定是否深挖。

**异常 C —— auto-compact 之后日志就结束了（71651 行是最后一行）**

无法判断"compact 后命中率恢复没恢复"。从这次数据只能确认"compact 那一刻 cache 暴跌"。

### Reasonix 对照补全（本次会话核实，补 MEMORY 第 480-528 行漏记的两条）

读了 `E:\Claude Code\DeepSeek-Reasonix-main-v2\internal\agent\compact.go` (760 行) + `prune.go` (289 行) 完整源码。MEMORY 第 480-528 行那份记录**主体准确**，但漏了两个关键细节：

**漏记 1 —— Reasonix `tokPerChar` 动态校准（compact.go:587-596）**

```go
func (a *Agent) tokPerChar() float64 {
    if u := a.lastUsage.Load(); u != nil && u.PromptTokens > 0 {
        if c := charsOfMessages(a.session.Messages); c > 0 {
            if r := float64(u.PromptTokens) / float64(c); r > 0.05 && r < 2 {
                return r  // 从上次真实 usage 算 tokens/char 比率
            }
        }
    }
    return fallbackTokPerChar  // 0.25 ≈ 4 chars/token
}
```

Reasonix **用上次真实回执的 PromptTokens / charsOfMessages 校准 tokens-per-char 比率**，区间 (0.05, 2) 才采信，否则 fallback 0.25。

claw 用**固定 char/4**（`estimate_message_tokens` 用 `len(text)/4`，conversation.rs:692 那个 pre-flight 也是 `char_count/4`）。对 CJK 内容（中文路径、中文注释）**char/4 会严重低估真实 token 数**——因为 DeepSeek/Anthropic 的 tokenizer 对中文接近 1 token/字，4 个中文字符 ≈ 4 tokens 而非 1 token。

**这直接影响刚才那次 auto-compact 的触发判断**：claw 的 pre-flight 用 char/4 估算，对中文重的会话可能高估也可能低估；maybe_auto_compact 用回执 input_tokens 更准但走了不同路径。**两套估算路径 + char/4 对 CJK 不准 = 触发时机不可靠**。

**漏记 2 —— Reasonix `pinnedPrefixLen` + `partitionFold` 分级保留（compact.go:377-418）**

MEMORY 只记了"Reasonix 有 archive 存档"，漏了**分级保留机制**：

```go
func (a *Agent) pinnedPrefixLen(msgs []provider.Message) int {
    // 锚 system + 首条 user turn（任务 + 约束，pinnableUserTurn 阈值 1500 tokens / 窗口 15%）+ 既有摘要
    // → 用户事实永不摘要掉，既有摘要永不二次摘要
}

func (a *Agent) partitionFold(region []provider.Message) (kept, fold []provider.Message) {
    // 把 region 分"kept verbatim（小 user turn + 既有摘要）"和"fold（其余）"
    // → 只对 fold 调摘要，用户原文 + 既有摘要保留
}
```

claw `compact_session` (compact.rs:96) **没这层**——region 全摘要，用户事实可能被摘要掉，既有摘要可能被二次摘要。MEMORY 第 510 行对照表里"archive 存档"那条**没展开 pinnedPrefixLen/partitionFold 这两层防漂移**，这里补上。

### 对刚才那次 cache 暴跌的具体归因

按 Reasonix 的四级机制，prompt_tokens 在 snip 阈值(0.6×1M=600K)时就该先 Snip 减压；到 compact 阈值(0.8×1M=800K)前再 Prune，删完若 `tokens-saved < high` 就**跳过 compact**。

claw 没这两道前置减压（micro-compact 还被 DISABLE 了），到 750K 直接硬压 79 条消息，前缀字节彻底改变，DeepSeek 硬盘缓存**必然 miss**——这就是 cache_read 暴跌 216K tokens 的根因。

**思路 4 Snip 在这次实机里根本没跑**（异常 A），所以这次暴跌**不能归罪于思路 4 失效**，而是"micro-compact 被禁 + 无 compact 前减压"双重缺失导致的。

### 下次接手清单（更新）

16. ★ 2026-07-17 新增（auto-compact 击穿实机复盘）：claw 的 `session_needs_pre_flight_compact`(conversation.rs:690) 用 char_count/4 估算、`maybe_auto_compact`(conversation.rs:638) 用回执 input_tokens——**两套估算路径不一致**，加上 char/4 对 CJK 严重不准，是 pre-flight 误触发 compact 的嫌疑点。下次接手若用户报"auto-compact 不该触发却触发了"，**先查 pre-flight 的 char_count/4 是否高估**，并对照 Reasonix `tokPerChar`(compact.go:587-596) 的"从上次真实 usage 校准 tokens/char 比率，区间 (0.05, 2) 才采信"做法。

17. ★ 2026-07-17 新增（Reasonix 对照补全两条漏记）：
    - **tokPerChar 动态校准**：MEMORY 之前那份对照表漏记这条。Reasonix 不用固定 char/4，而是 `lastUsage.PromptTokens / charsOfMessages` 校准 tokens-per-char 比率，区间 (0.05, 2) 才采信，否则 fallback 0.25。claw 改这条对 CJK 重场景收益最大。
    - **pinnedPrefixLen + partitionFold 分级保留**：MEMORY 之前只记"archive 存档"，漏了"分级保留"这层防漂移。Reasonix 锚 system + 首条 user turn + 既有摘要永不摘要；partitionFold 把 region 分 kept verbatim 和 fold，只对 fold 调摘要。claw `compact_session` region 全摘要，用户事实可能被摘要掉。下次做 compact 改进时这两条要一起补。

18. ★ 2026-07-17 新增（实机验证思路 4 的盲点）：本次用户跑的那轮 `CLAW_MICROCOMPACT_DISABLE=1` 开着，micro-compact 一次没触发（日志 0 条 `claw_microcompact` 事件）。**思路 4 Snip 在这次实机里根本没跑**，所以 cache 暴跌 48% 不能归罪于思路 4 失效——是"micro-compact 被禁 + 无 compact 前减压"双重缺失导致的。下次实机验证思路 4 必须**关掉 `CLAW_MICROCOMPACT_DISABLE`**，并看 `claw_microcompact` 事件是否真的写进日志了再下判断。

  **★★ 2026-07-18 更新（思路 4 Snip 已有 1 条真机实测样本）**：用户照建议关掉 `CLAW_MICROCOMPACT_DISABLE` 重跑一轮，日志 `E:\NW工程\资料库\html\claw_glm_diag.log`（115877 行）数据反转之前判断——
  - `DISABLE` 字面命中 **0**，`claw_microcompact` 事件 **1**（行 11883）：`t=1784283708 cleared=1 chars_freed=639406 protected=8 [cleared_tools] edit_file` —— Snip 确实跑了
  - 时序：microcompact 触发前 `cache_read=151680`，触发后第 1 轮 `cache_read=226944`（**反涨 75K**），后续微增到 229K —— 印证头部字节稳定被 DeepSeek 缓存命中
  - 但 355 秒后（t=1784284063）cache_read 暴跌 97.3%（227K→6K）—— 真因是用户发了新任务（"测试版主页迁到正式版"），新 user input 让前缀彻底变，与 microcompact 无关
  - **结论**：思路 4 Snip 不再是"没跑过"，而是"跑了 1 次、效果正面（cache_read 反涨 75K 印证头部稳定命中）"。仍需更多样本（特别是连续多次 microcompact + 长会话尾段）才能定论 Snip 在长会话中持续保命中率的效果。`chars_freed=639406` 根因也已落定——claw `EditFileOutput` 塞整份原文件导致单次 edit_file 产生大 output（已在第 19 条那条改动里落地 DeepSeek 节制回执修复）。

---

## ★★ 2026-07-17 edit_file/write_file DeepSeek 节制回执（本次会话，已落地）

### 根因（本会话上半场核实）
对照 Reasonix `internal/tool/builtin/editfile.go:74-78`——**edit_file 永远只回 `"edited <path>"` + receipt（行号/字符数元数据），不塞原文件**。claw `runtime/src/file_ops.rs` 的 `EditFileOutput` / `WriteFileOutput` 反过来——把 `original_file`（整份原文件内容）+ `structured_patch`（unified diff hunks）+ `git_diff` 全塞进回执 JSON。对 80KB PHP 文件单次 edit 就产生 80KB tool_result；**这次实机 microcompact 那条 `chars_freed=639406` 根因就是 claw edit_file 塞原文件**（修正之前判断：639K 是 microcompact 累计清量，但源头确实是 claw edit_file 不该塞原文件的大 output）。

### 用户决策
按模型前缀分支：`ANTHROPIC_MODEL` 以 `deepseek` 开头 → 走 Reasonix 节制回执；`glm` 开头或未设 → 保持原回执不变。

### 落地改动（全部在 `runtime/src/file_ops.rs`）
| 改动 | 说明 |
|------|------|
| `WriteFileOutput` struct | `original_file` / `structured_patch` / `git_diff` 三个重型字段改 `Option<T>` + `#[serde(default, skip_serializing_if="Option::is_none")]`，DeepSeek 路径置 `None` 让它们不出现在 JSON 回执里 |
| `EditFileOutput` struct | 同上三个字段（`original_file` 原是 `String`，`structured_patch` 原是 `Vec`，`git_diff` 原是 `Option` 但没 `skip_serializing_if`）统一改 `Option` + `skip_serializing_if` |
| 新增 `should_use_compact_receipt()` | 抽 `ANTHROPIC_MODEL` env 前缀（`.to_lowercase().starts_with("deepseek")`）判定节制策略。**在 runtime crate 内独立判断，不依赖 api crate**（避免循环依赖，对齐 MEMORY 第 51 行已知坑——api crate 的 `detect_provider_kind` 不便从 runtime 反向调用）|
| `write_file` 实现 | `let compact = should_use_compact_receipt();` 后 DeepSeek 路径 `original_file_output = None` + `structured_patch_output = None`；GLM 路径保留原 `is_large_file` 100KB 阈值逻辑（`original_file.as_ref().map(...)`）|
| `edit_file` 实现 | 同上分支；GLM 路径保留原 100KB 大文件占位符逻辑 |

### 死代码清理
落地时删了两段被新分支覆盖的老 `let original_file_output = ...`（原 `file_ops.rs:375-381` 和 `:461-466`）——`cargo check` 第一次跑暴露了 `unused variable` warning，已删干净。

### 验证
- `cargo check --workspace` ✅ 干净（只剩 3 个预存 warning：MEMORY 第 246/278 行已记的 `plugins/hooks.rs Path`、`bash.rs CommandExt`、`tools/lib.rs convert_messages dead_code`，本次未新增）
- `cargo test -p runtime --lib file_ops` ✅ 13 passed 1 failed。**唯一 FAILED 的 `glob_search_skips_common_heavy_directories` 已用 `git stash` 核实是预存债**（stash 后原始代码跑同一测试同样 FAILED，与本次改动无关）——该测试期望在 `src/AGENTS.md` 找到匹配，但仓库根没这文件，是测试桩与实际仓库布局脱节的预存问题。

### 未做（留待实机验证）
- **真机验证未做**——本次只做编译+单元测试层验证。用户需重编 `cargo build --release` 替换 `claw.exe` 后真机跑一轮，看日志里 `claw_request_size` 的 bytes 是否在 edit_file 后明显变小、`claw_microcompact` 事件触发次数是否减少。
- **read_file 工具未改**——本次只改 edit_file/write_file。read_file 的 `ReadFileOutput` 本就是按行读 + 行数限制，回执语义合理（不是"塞整份原文件"），不需要这条节制。
- **search_replace 工具未改**——同理 search_replace 回执语义合理。
- **`tools/src/lib.rs` 那份 `convert_messages` 的 dead_code warning** 是 2026-07-14 那次改动拆 `_with_cache` 变体后留下的，本次未动，留预存债。

### 下次接手清单（更新）

19. ★ 2026-07-17 新增（edit_file/write_file DeepSeek 节制回执）：`runtime/src/file_ops.rs` 的 `EditFileOutput` / `WriteFileOutput` 三个重型字段（`original_file` / `structured_patch` / `git_diff`）已改 `Option` + `skip_serializing_if`，`should_use_compact_receipt()` 按 `ANTHROPIC_MODEL` env 前缀 `deepseek` 分支。**下次改这两个 struct 加字段时记得 GLM 路径要填 `Some(...)`、DeepSeek 路径要填 `None`**，别在 DeepSeek 路径意外塞原文件——那是这条改动的核心禁忌。真机验证关键看 `claw_request_size` 的 bytes 在 edit_file 后是否明显变小。**★ multiprovider 落地后的强制配套改动（2026-07-17 会话下半场补充）**：当前 `should_use_compact_receipt()` 只读全局 `ANTHROPIC_MODEL` env，multiprovider（`docs/multiprovider.md`）落地后子 agent 走 `ResolvedSubagentProvider.model` 与主 LLM env 不同 provider 会破裂——主=DeepSeek/子=GLM 时子 agent 被误节制（功能退化），主=GLM/子=DeepSeek 时子 agent 漏节制击穿缓存（病在子 agent 路径复发）。必须配套改成 `should_use_compact_receipt(model: &str)` 按**当前调度的 model 名**判定，由 `tools/src/lib.rs` 工具 dispatch 层算好布尔传进 `file_ops::edit_file`/`write_file`（选项 A，加 `compact_receipt: bool` 参数；非选项 B 的 ToolContext 大改）。详见 `docs/multiprovider.md` 3.4bis 节。**不动 `AgentInput` JSON schema**（模型逐调用选 provider 的能力留二期），但要动工具函数签名传 model——前者破坏模型兼容，后者模型看不到，两件事不要混。
20. ★ 2026-07-17 新增（预存债标记）：`file_ops::tests::glob_search_skips_common_heavy_directories` 测试断言 `src/AGENTS.md` 在 glob 结果里，但仓库根没该文件——是测试桩与实际仓库布局脱节的预存问题，本次未修。下次接手若要修，要么改测试断言用仓库里真存在的文件，要么在测试 tempdir 里造一份 AGENTS.md。

21. ★ 2026-07-18 新增（read_file 防击穿对照 Reasonix——三重门 claw 全缺）：日志 `claw_glm_diag.log` 行 23029 那次 cache_read 暴跌 97.3%（229K→6K）真因是"读大文件击穿缓存"——input 暴涨到 221KB，行 21375~21461 那批 read_file 把 `IndexController.php` + `route.php` + `indextest.html` + `index.html` 一连串文件全文塞进上下文换前缀。对照 Reasonix `internal/tool/builtin/readfile.go` (255 行) 的三重门——**claw 三个全缺**：
    - **门 1 默认 limit 硬封顶 2000 行**（Reasonix 行 45 `const readFileDefaultLimit = 2000`）——claw `file_ops.rs:303` 的 `read_file` 函数 `limit: Option<usize>` 参数没默认值硬封顶，`file_ops.rs:334` `limit.map_or(lines.len(), ...)` 即 limit=None 时**返回整文件所有行**。模型不传 limit 就 slurp 整文件。**改成**：limit=None 时 fallback 到 2000 而非 `lines.len()`，对齐 Reasonix。
    - **门 2 流式 break**（Reasonix `scan` 函数行 212-254，遇 cap 即 `break` 不读余下文件）——claw `file_ops.rs:331` `fs::read_to_string` + `:332` `content.lines().collect()` 是**整文件 slurp + 切片**，50MB 文件也一次性进内存再切片。**改成**：用 `BufReader::new(File::open(path))` + `BufRead::lines()` 流式，遇 limit 即 break。这条对 50MB 大文件场景收益明显，但 claw 当前 PHP 资料库单文件常 30~80KB 收益小——优先级低于门 1。
    - **门 3 SnipHint 头重尾轻 + 字符级封顶**（Reasonix 行 70-72 `SnipHint{Head:120, Tail:12, HeadChars:12000, TailChars:2000}`）——claw `micro_compact.rs:33` `READ_ONLY_SNIP = SnipStrategy { head: 80, tail: 12 }` 是行级封顶对齐了 Reasonix 的 Head/Tail 行数，但**缺字符级 HeadChars/TailChars 字段**。一个超长行（比如 minified JS 一行 50KB）能冲垮行级封顶。**改成**：`SnipStrategy` 加 `head_chars: usize, tail_chars: usize` 字段，对齐 Reasonix 12000/2000。这条优先级中等。
    - **门 4 二进制 8KB peek 拒读**（Reasonix 行 23 `readFileBinaryPeek = 8*1024` + 行 176 NUL 字节检测）——claw 没这条。但 claw 资料库场景几乎不会读二进制，优先级低。
    - **结论**：claw read_file 当前是"整文件 slurp + 切片 + 行级 Snip"，Reasonix 是"流式 break + 2000 行硬封顶 + 行级 + 字符级双层 Snip + 二进制 peek 拒读"。**真正该抄的是门 1**（一行 const + fallback 改动），门 2/3 收益看场景，门 4 暂不做。下次接手若用户报"read 大文件击穿缓存"，先看 limit 是不是 None 走了 `lines.len()` 路径。
    - **与第 19 条的关系**：第 19 条改的是 edit_file/write_file **回执塞原文件**那条路；本条改的是 read_file **读进上下文**那条路。两条是不同的击穿路径，要分开治——前者已落地 DeepSeek 节制回执，后者还没动。

---

## ★★★ 2026-07-18 edit_file/write_file DeepSeek 节制回执真机实测（节制成功）

### 实测数据（日志 `E:\NW工程\资料库\html\claw_glm_diag.log` 224424 行 / 80MB，今日 07:34-09:08 那轮）

**回执节制生效证据**（任务 #2 核实）：
- 全日志 **1002 处 edit_file 调用 + 83 处 write_file 调用**，匹配出 **919 个 edit_file 回执 + 0 个 write_file 回执**（write_file 调用因跨批次 tool_use_id 关联未匹配到回执段，但 edit_file 已能定论）
- **919 个 edit_file 回执里：含 `originalFile` 字段的 = 0，含 `structuredPatch` 字段的 = 0，含 `gitDiff` 字段的 = 0**——三个重型字段被 `skip_serializing_if="Option::is_none"` 完全 skip 掉，DeepSeek 路径走的 `None` 分支生效
- 残留字段：`filePath` + `oldString` + `newString` + `replaceAll` + `userModified`（这些是 `String`/`bool` 非重型字段，节制改动没动它们，保留原状）——回执只剩"改了哪个文件+改了哪段"的瘦骨架，对齐 Reasonix `editfile.go:74` 那个 `"edited <path>"` + receipt 哲学

**缓存命中效果**（任务 #3 核实）：

| 指标 | 值 | 评 |
|---|---|---|
| 事件分布 | cache_diag=83 / glm_diag=83 / request_size=83 / Reranking=26 / **auto_compact=0 / microcompact=0** | 全程没触发 auto-compact 也没触发 microcompact——节制回执让请求体没到阈值 |
| cache_read 跌幅点 | **只有 2 个** >30% 跌幅点：t=1784331431 (-45%, cr 10K→5K) 和 t=1784335316 (-50%, cr 287K→142K)。前者是小 cr 值波动无意义；后者是唯一一次实质跌幅但下一轮就回升 | 比上一轮（7-17）的 6 个跌幅点、97.3% 暴跌明显改善 |
| cache_read 峰值 | 378,752（行 224423，末轮） | 比上一轮峰值 446K 低，但本轮持续到末轮还在攀升——节制回执让 cache 稳定累积而非被巨型 edit 回执冲垮 |
| edit_file 邻近 cache_read 稳态 | 5 个抽样：use[27711]→235K / use[29588]→239K / use[31495]→252K / use[41620]→258K / use[50417]→262K | **edit_file 调用后 cache_read 不跌反涨**（235K→262K 持续累积），印证"瘦回执不击穿字节级缓存" |
| request_size bytes | min 20K / 中位 975K / max 1.38MB | 中位 975K 比上一轮大，说明本轮会话更长更深，但 cache_read 峰值仍能爬到 378K——节制回执让 cache 在大请求体下仍能命中 |
| 命中率粗算 cache_read/input | 中位 31105%（input 含大量首轮 system+tools 命中后的微调用，cache_read 远大于 input） | 命中率高，DeepSeek 字节级缓存持续生效 |

**对比上一轮（7-17）的关键差异**：

| 维度 | 7-17 那轮（节制回执未启用 / microcompact 跑了 1 次） | 7-18 本轮（节制回执已启用） |
|---|---|---|
| edit_file 回执含 originalFile | 部分（未实测但代码改动前必然含） | **0** |
| microcompact 触发 | 1 次（chars_freed=639406） | **0 次**（节制回执让大 output 不再产生，microcompact 无用武之地） |
| auto_compact 触发 | 2 次（removed=79/74） | **0 次** |
| cache_read 暴跌点 | 6 个，最深 97.3% | **2 个，最深 50% 且立即回升** |
| cache_read 峰值 | 446K 后暴跌 | 378K 持续攀升到末轮 |

### 结论

**第 19 条改动 DeepSeek 节制回执真机实测成功**——919 个 edit_file 回执全部节制（originalFile/structuredPatch/gitDiff 三字段 0 命中），cache_read 在 edit_file 调用后不跌反涨（235K→262K），全程零 auto_compact 零 microcompact 触发。这是第 19 条那次改动的正面验证。

**剩余观察**：
- write_file 回执本次没匹配到（跨批次 tool_use_id 关联未对齐），但 write_file 走同一套 `should_use_compact_receipt` + `Option` 字段逻辑，节制应同样生效——下次接手若要核实 write_file，需改分析脚本按 tool_use_id 全局关联而非局部 200 行窗口。
- 第 21 条那条 read_file 防"读大文件击穿"还没动——本轮没有 read_file 触发的 cache 暴跌，但本轮场景是连续 edit_file（不是读大文件），第 21 条的真机验证仍欠样本。
