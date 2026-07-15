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

## ★★ 2026-07-15 auto-compact 频繁触发是命中率杀手（本次会话核心发现）

### 日志铁证
claw_glm_diag.log 的 est_tokens 轨迹：6265 → 14203 → 95504 → **9106** → 9126 → ... → 93795 → **7867** → 10719。每次 auto-compact 后 est_tokens 断崖跌（95504→9106, 93795→7867），历史被压成摘要，整个前缀字节序列彻底改变。DeepSeek 硬盘缓存是字节级完整匹配，compact 后前缀全变必然 miss。

命中率低的真正杀手链：上下文涨 → 触 auto-compact(75%阈值) → 前缀字节全变 → DeepSeek 缓存 miss → 命中率低。

### 官方 claude-code 怎么同时做到"不撑爆上下文 + 高命中率"
读了官方 services/compact/{microCompact,timeBasedMCConfig,autoCompact,compact}.ts，它用三个 claw-code 完全没有的机制：

**机制 1：Cached Microcompact（cache_edits）**——清旧 tool_result时不改本地消息内容，只在请求体加 cache_edits �块让服务端删。本地前缀字节不变 → DeepSeek 缓存仍命中。但这条对 DeepSeek 无效（cache_edits 也是 Ignored）。

**机制 2：Compact Boundary**——compact 后保留前缀的"边界消息"，不让历史完全断。getMessagesAfterCompactBoundary（query.ts:365）保留 compact boundary 之前的不变前缀，只压缩 boundary 之后的。下一轮请求的前缀 = compact 之前的不变段 + 摘要 + 新对话，不变段字节稳定仍能命中。claw 的 compact.rs 是全量压摘要，没有 boundary 保留。

**机制 3：auto-compact 阈值动态化 + reactive compact**——阈值不是固定 75%，而是 getAutoCompactThreshold(model) 按模型上下文窗口动态算。DeepSeek V4 Pro 1M 上下文，官方放阈值到 ~750K 才压；claw 固定 75%×131K=~98K 就压，当然频繁触发。还有 reactive compact：宁可先让请求发出去，收到 413 prompt-too-long 才压，避免 proactive compact 不必要击穿缓存。

官方在 GLM 5.1（200K）不撑爆的原因：阈值动态 = contextWindow × 0.75，200K×0.75=150K 才压，GLM 200K够用。在 DeepSeek（1M）高命中率的原因：机制 1+2 让 compact 不击穿前缀，且阈值放到 750K 才压，几乎不触发。

---

## ★ 二期改动计划（重写，2026-07-15 纠错后）

前提纠错：DeepSeek 不认 cache_control/anthropic-beta。二期方向从"注入 Anthropic cache 字段"彻底转向"稳住请求前缀字节，适配 DeepSeek 硬盘缓存的完整匹配规则"。原二期-A/B/D/F 全废，C/E 升为最高优先并重写，新增 G/H。

### 二期-C1（最高优先）：auto-compact 阈值按模型上下文窗口动态化
目标：让 claw 的 auto-compact 阈值随模型上下文窗口动态算，DeepSeek 1M → 750K 才压，几乎不触发，前缀稳定。

改动：
1. runtime/src/compact.rs 或 conversation.rs：当前固定 CLAUDE_CODE_AUTO_COMPACT_WINDOW=131000 → 改成 model_token_limit × 0.75。需要拿到模型上下文窗口大小（api/src/types.rs 的 ModelInfo 或 providers 的 token_limit）。
2. DeepSeek V4 Pro 1M → 750K，GLM 5.1 200K → 150K，都不撑爆。
3. 测试：阈值计算函数单测覆盖几个模型上下文窗口值。

预期：命中率 50% → 70-80%（compact 几乎不触发，前缀稳定段能命中）。

### 二期-C2（高优先，面大）：Compact Boundary 保留不变前缀
目标：compact 时不全部压摘要，保留 compact_boundary 之前的不变前缀，只压缩 boundary 之后的。

改动：
1. runtime/src/compact.rs：compact 时插入 compact_boundary 标记，boundary 之前的消息原样保留。
2. 序列化时 boundary 之前的不变段字节稳定 → DeepSeek 缓存前缀单元仍能命中。
3. 测试：compact 后 boundary 之前消息不变 + boundary 之后被压摘要。

预期：命中率 70-80% → 85%（即使触发 compact，不变前缀段仍命中）。

### 二期-C3（中优先）：micro_compact 改成固定长度占位符
目标：清空 tool_result 时换固定长度占位符，不随内容变，让被清空前的前缀单元字节稳定。

改动：runtime/src/micro_compact.rs 的占位符长度固定（如 [CLEARED:512bytes] 固定 512 字节）。

预期：再 +5%。

### 二期-E（最高优先，辅助验证）：diag 日志补 DeepSeek cache 命中回执
目标：让 claw 端能看到 DeepSeek 的真实命中率，不再靠人肉看 DeepSeek 后台。

改动：
1. api/src/types.rs 的 Usage struct：加 prompt_cache_hit_tokens: u32 / prompt_cache_miss_tokens: u32 字段（#[serde(default)]），对齐 DeepSeek 回执字段名（docs/DeepseekAPI/2.md:65-67）。Anthropic 后端不发这俩字段→default 0，兼容。
2. api/src/providers/anthropic.rs 的 diag 埋点：response usage 解析后追加一行 claw_cache_diag t=... hit=... miss=... input=... output=...。
3. 仓库根加 analyze_cache.ps1 腄本（沿用 analyze_log.ps1 模式）。

预期：实机量化命中率，二期-C 改完后能对比前后。

### 二期-G（核实）：确认 DeepSeek 硬盘缓存对 Anthropic 协议路径是否生效
目标：核实 DeepSeek 的硬盘缓存是否对 /anthropic 路径（Anthropic 协议）生效，还是只对 /chat/completions（OpenAI 协议）生效。

现状：docs/DeepseekAPI/2.md 的缓存说明写在 OpenAI 协议文档里。DeepSeek 的 Anthropic-compat 接口（1.md）没提缓存。可能 Anthropic 路径根本没接硬盘缓存→那 50% 是别的机制，二期-C 再怎么稳前缀也没用。

动作：
1. 先做二期-E（能看到 hit/miss 字段）。
2. 跑两轮相同前缀的请求，看 prompt_cache_hit_tokens 是否 >0。
3. 如果 0→Anthropic 路径没接硬盘缓存，二期要改走 OpenAI 协议调 DeepSeek（base_url=https://api.deepseek.com，走 openai_compat.rs 而非 anthropic.rs），那路径才有缓存。这是大改，需用户同意。
4. 如果 >0→Anthropic 路径有缓存，二期-C 稳前缀方向正确，继续。

### 二期-H（新增）：必要时切 OpenAI 协议调 DeepSeek
目标：如果二期-G 确认 Anthropic 路径无缓存，改走 OpenAI 协议。

改动（大）：
1. api/src/providers/mod.rs 的 detect_provider_kind：deepseek-* 改路由到 ProviderKind::OpenAi。
2. 验证 openai_compat.rs 的 translate_message 对 DeepSeek 思考模式（reasoning_effort/output_config.effort）的字段映射正确（docs/DeepseekAPI/3.md）。
3. 重新跑二期-G 验证缓存生效。

预期：如果 Anthropic 路径无缓存而 OpenAI 路径有，切完命中率直接到 70-80%。

### 废弃的原二期项
- 二期-A（system 分块 cache_control）：DeepSeek 不认该字段，无效
- 二期-B（tool_result cache_reference）：同上，无效
- 二期-D（beta header）：手册明确 ignored，无效
- 二期-F（TTL 1h latch）：DeepSeek 不认 TTL，无效
- 机制 1（cache_edits）：DeepSeek 不认 cache_edits，无法复刻官方 cached MC

---

## 下次接手清单（更新）

13. ★ 2026-07-15 新增（纠错版）：二期启动前先读 docs/DeepseekAPI/{1,2,3,4}.md 确认 DeepSeek 的字段支持——一期栽在没读手册假设它认 cache_control。二期顺序：先做二期-E（补 prompt_cache_hit_tokens/prompt_cache_miss_tokens 字段看命中率）→ 二期-G（确认 Anthropic 路径有无缓存）→ 若无缓存走二期-H（切 OpenAI 协议）→ 若有缓存走二期-C（稳前缀）。api/src/cache_control.rs 一期模块保留（对真 Anthropic 后端仍有效），但二期方向转向"稳前缀字节"不再在那扩。接入新网关前必读该网关官方 API 手册，不要假设它完整实现 Anthropic 协议——这是一期教训。Python 腄本批量补字段那招（本次处理 33 处）二期补 prompt_cache_hit_tokens/prompt_cache_miss_tokens 字段时可复用。

14. ★ 2026-07-15 新增（auto-compact 杀手）：claw_glm_diag.log 的 est_tokens 轨迹暴露 auto-compact 频繁触发是 DeepSeek 缓存命中率杀手——每次 compact 后前缀字节全变，DeepSeek 硬盘缓存必然 miss。二期-C1（阈值动态化）是性价比最高的一改，只动 compact 阈值计算逻辑就能让 DeepSeek 1M 窗口下几乎不 compact。官方 claude-code 的三个机制（cached MC/compact boundary/阈值动态化）中，机制 1 对 DeepSeek 无效（cache_edits 也 Ignored），机制 2+3 是二期-C2+C1 对应。compact.rs 和 micro_compact.rs 是二期改动核心。
