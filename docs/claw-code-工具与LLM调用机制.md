# claw-code 工具与云端 LLM 调用机制详解

> 本文面向编程新手，用通俗的语言解释 claw-code（Rust 版 Claude Code）的"工具"系统，以及 AI 大模型是如何调用这些工具的。

---

## 一、什么是 claw-code？

claw-code 是 **Claude Code** 的 Rust 语言实现版本。Claude Code 是一个**AI 编程助手**——它在终端里和你对话，可以读代码、写文件、执行命令、搜索内容，像一个会编程的同事坐在你旁边。

它的工作模式是：

```
你输入问题 → 发给云端的 AI 大模型（如 Claude）→ 模型决定要调用什么工具 → 执行工具 → 把结果发给模型 → 模型给出回答或继续调用工具 → ... → 直到任务完成
```

---

## 二、claw-code 有哪些"工具"？

"工具"（Tool）是 claw-code 给 AI 大模型准备的"能力清单"。大模型不能直接操作你的电脑，它只能通过**调用工具**来做事。每个工具就像一个函数：有名字、有描述、有输入参数。

以下是 claw-code 定义的所有工具（共 **43 个**），按用途分类：

### 📂 文件操作类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `read_file` | 读取一个文本文件的内容 | 只读 |
| `write_file` | 写入/创建一个文件 | 工作区写入 |
| `edit_file` | 在文件中查找并替换一段文本 | 工作区写入 |

### 🔍 搜索类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `grep_search` | 用正则表达式搜索文件内容 | 只读 |
| `glob_search` | 按文件名模式搜索文件（如 `**/*.rs`） | 只读 |
| `ToolSearch` | 搜索"工具"本身——查找有哪些工具可用 | 只读 |

### 💻 命令执行类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `bash` | 在终端执行 shell 命令 | ⚠️ 危险/完全访问 |
| `PowerShell` | 在 Windows 上执行 PowerShell 命令 | ⚠️ 危险/完全访问 |
| `REPL` | 在一个交互式子进程中执行代码（如 Python） | ⚠️ 危险/完全访问 |

### 🌐 网络类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `WebFetch` | 获取一个 URL 的内容，转为可读文本 | 只读 |
| `WebSearch` | 搜索互联网获取最新信息 | 只读 |
| `RemoteTrigger` | 触发一个远程 webhook 端点 | ⚠️ 危险/完全访问 |

### 📝 任务管理类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `TodoWrite` | 更新当前会话的任务清单 | 工作区写入 |
| `TaskCreate` | 创建一个后台子任务（单独进程执行） | ⚠️ 危险/完全访问 |
| `TaskGet` | 查看后台任务的状态 | 只读 |
| `TaskList` | 列出所有后台任务 | 只读 |
| `TaskStop` | 停止一个后台任务 | ⚠️ 危险/完全访问 |
| `TaskUpdate` | 发送消息给一个后台任务 | ⚠️ 危险/完全访问 |
| `TaskOutput` | 获取后台任务的输出 | 只读 |
| `RunTaskPacket` | 从结构化任务包创建后台任务 | ⚠️ 危险/完全访问 |

### 🤖 子智能体（Worker）类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `WorkerCreate` | 创建一个 Worker（另一个 AI 智能体会话） | ⚠️ 危险/完全访问 |
| `WorkerGet` | 查看 Worker 的状态 | 只读 |
| `WorkerObserve` | 观察 Worker 的终端输出以检测信任门控等 | 只读 |
| `WorkerResolveTrust` | 解决 Worker 的信任提示 | ⚠️ 危险/完全访问 |
| `WorkerAwaitReady` | 等待 Worker 准备就绪 | 只读 |
| `WorkerSendPrompt` | 向已就绪的 Worker 发送任务提示 | ⚠️ 危险/完全访问 |
| `WorkerRestart` | 重启一个失败的 Worker | ⚠️ 危险/完全访问 |
| `WorkerTerminate` | 终止一个 Worker | ⚠️ 危险/完全访问 |
| `WorkerObserveCompletion` | 报告 Worker 完成状态 | ⚠️ 危险/完全访问 |

### 👥 团队与定时任务类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `TeamCreate` | 创建一个团队（并行执行多个子任务） | ⚠️ 危险/完全访问 |
| `TeamDelete` | 删除一个团队 | ⚠️ 危险/完全访问 |
| `CronCreate` | 创建一个定时重复执行的任务 | ⚠️ 危险/完全访问 |
| `CronDelete` | 删除一个定时任务 | ⚠️ 危险/完全访问 |
| `CronList` | 列出所有定时任务 | 只读 |

### 🧩 扩展类（MCP / 插件）

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `MCP` | 调用一个 MCP 服务器提供的工具 | ⚠️ 危险/完全访问 |
| `ListMcpResources` | 列出 MCP 服务器的可用资源 | 只读 |
| `ReadMcpResource` | 读取 MCP 服务器的某个资源 | 只读 |
| `McpAuth` | 对 MCP 服务器进行 OAuth 认证 | ⚠️ 危险/完全访问 |
| `LSP` | 查询语言服务器协议（代码符号、引用、诊断等） | 只读 |

### 🛠 其他辅助类

| 工具名 | 作用 | 权限等级 |
|--------|------|---------|
| `Skill` | 加载本地定义的技能（Skill）文件 | 只读 |
| `Agent` | 启动一个专门的子智能体任务 | ⚠️ 危险/完全访问 |
| `Sleep` | 等待指定的时间（不占用进程） | 只读 |
| `SendUserMessage`（别名 `Brief`） | 发送一条消息给用户 | 只读 |
| `AskUserQuestion` | 向用户提问并等待回答 | 只读 |
| `Config` | 获取或设置 claw-code 的设置 | 工作区写入 |
| `EnterPlanMode` | 进入"计划模式" | 工作区写入 |
| `ExitPlanMode` | 退出"计划模式" | 工作区写入 |
| `StructuredOutput` | 按指定格式返回结构化输出 | 只读 |
| `NotebookEdit` | 编辑 Jupyter notebook 的单元格 | 工作区写入 |
| `TestingPermission` | 测试权限系统（仅开发用） | ⚠️ 危险/完全访问 |

### 🔐 权限等级说明

每个工具都标有一个**权限等级**，决定了 AI 调用该工具时需要得到你的什么程度的授权：

| 权限等级 | 含义 |
|---------|------|
| `ReadOnly` | 只读取信息，不会修改任何东西，最安全 |
| `WorkspaceWrite` | 可以在工作区内修改文件，但不可以执行命令 |
| `DangerFullAccess` | 最高权限，可以执行任意命令、删除文件、联网等 |

---

## 三、云端 LLM 是如何调用这些工具的？

现在到了重点：整个调用的"业务流程"是怎样的？下图展示了完整链路：

```
你
 │
 ▼
┌────────────────────────────────────────────┐
│           claw-code 主循环                   │
│  (rust/crates/tools/src/lib.rs)              │
│                                              │
│  1. 把对话历史 + 工具清单 → 发给云 API        │
│  2. 收到模型的响应（流式 SSE）                 │
│  3. 解析响应中的 ToolUse 块                   │
│  4. 本地执行工具（读文件、跑命令…）             │
│  5. 把工具结果发给模型                          │
│  6. 回到步骤 2，直到模型说"做完了"              │
└────────────────────────────────────────────┘
         │                          ▲
         ▼                          │
┌────────────────────────────────────────────┐
│              云端 AI 大模型                  │
│          (Claude / GPT / 通义千问…)          │
│                                              │
│  模型决定：                                   │
│  - 回答文本 → 直接返回                        │
│  - 调用工具 → 返回 ToolUse 块                 │
│    (告诉你要调用什么工具、传什么参数)           │
└────────────────────────────────────────────┘
```

### 3.1 第一步：发送"请求"给云端 AI

claw-code 把整个对话历史（包括系统提示、用户问题、之前的工具调用结果）加上所有工具的定义，打包成一个 JSON 请求，通过 HTTP 发送到云端的 API。

这个请求包含以下几个关键部分（定义在 `rust/crates/api/src/types.rs` 的 `MessageRequest` 结构体）：

```rust
pub struct MessageRequest {
    pub model: String,                          // 模型名，如 "claude-sonnet-4-20250514"
    pub max_tokens: u32,                        // 最大输出 token 数
    pub messages: Vec<InputMessage>,            // 对话历史
    pub system: Option<String>,                 // 系统提示
    pub tools: Option<Vec<ToolDefinition>>,     // ← 工具定义清单就在这里！
    pub tool_choice: Option<ToolChoice>,        // 工具选择策略
    pub stream: bool,                           // 是否使用流式响应（SSE）
    // ...还有其他参数如 temperature、extra_body 等
}
```

**工具定义（ToolDefinition）** 就是告诉 AI："你能用这些功能"。例如 `read_file` 工具的定义看起来像：

```json
{
  "name": "read_file",
  "description": "Read a text file from the workspace.",
  "input_schema": {
    "type": "object",
    "properties": {
      "path": { "type": "string" },
      "offset": { "type": "integer" },
      "limit": { "type": "integer" }
    },
    "required": ["path"]
  }
}
```

这个格式完全遵循 **Anthropic 的 Tool Use API 规范**，也兼容 OpenAI 的函数调用。

### 3.2 第二步：选择合适的 Provider 客户端

claw-code 支持**三种**云端 API 后端，由模型名自动决定：

```rust
pub enum ProviderClient {
    Anthropic(AnthropicClient),       // 模型名含 "claude-" → 用 Anthropic API
    Xai(OpenAiCompatClient),          // 模型名含 "grok-" → 用 xAI API（兼容 OpenAI 格式）
    OpenAi(OpenAiCompatClient),       // 其他 → 用 OpenAI 兼容 API
}
```

> **OpenAI 兼容**意味着可以对接：OpenAI 官方、通义千问（DashScope）、DeepSeek、Azure OpenAI、本地 Ollama 等等。

### 3.3 第三步：流式接收 AI 的响应（SSE 协议）

claw-code 使用 **SSE（Server-Sent Events，服务器推送事件）** 以流式方式接收 AI 的回复。这意味着响应是一块一块到达的，不需要等全部生成完。

SSE 流的事件类型（定义在 `rust/crates/api/src/types.rs`）：

| 事件类型 | 含义 |
|---------|------|
| `MessageStart` | 消息开始，包含消息 ID |
| `ContentBlockStart` | 内容块开始（可能是一个文本块，也可能是一个工具调用块） |
| `ContentBlockDelta` | 内容块的增量数据 |
| `ContentBlockStop` | 内容块结束 |
| `MessageDelta` | 消息增量（含 stop_reason） |
| `MessageStop` | 消息结束 |

SSE 解析器在 `rust/crates/api/src/sse.rs` 中，它负责把一行行的 SSE 数据解析成上面这些结构化事件。

### 3.4 第四步：AI 决定"调用哪个工具"

当 AI 决定要使用一个工具时，它会返回一个 **ToolUse** 内容块。例如：

```json
{
  "type": "tool_use",
  "id": "toolu_12345",
  "name": "read_file",
  "input": {
    "path": "src/main.rs"
  }
}
```

这个信息被 claw-code 解析为一个 `OutputContentBlock::ToolUse` 枚举值（定义在 `types.rs`）：

```rust
pub enum OutputContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    Thinking { thinking: String, signature: Option<String> },
    RedactedThinking { data: Value },
}
```

### 3.5 第五步：在本地执行工具

claw-code 根据工具名，把请求分发到对应的执行函数。这是通过一个**巨大的 match 语句**实现的（在 `tools/src/lib.rs` 的 `execute_tool_with_enforcer` 函数中，约 110 行）：

```rust
fn execute_tool_with_enforcer(enforcer, name, input) -> Result<String, String> {
    match name {
        "bash"          => { /* 检查权限 → 执行命令 → 返回输出 */ }
        "read_file"     => { /* 读取文件，检查路径是否合规 */ }
        "write_file"    => { /* 写入文件 */ }
        "edit_file"     => { /* 编辑文件 */ }
        "WebFetch"      => { /* 抓取网页 */ }
        "WebSearch"     => { /* 搜索网页 */ }
        // ... 共 43 个工具
        _               => Err("unsupported tool".into())
    }
}
```

每个工具执行前还会经过**权限检查**（`PermissionEnforcer`），确保 AI 没有做超出授权范围的事。

### 3.6 第六步：把工具结果发回给 AI

工具执行完成后，claw-code 把结果包装成一个 **ToolResult** 消息，再次发给云端 AI：

```rust
pub enum InputContentBlock {
    Text { text: String },
    ToolResult {
        tool_use_id: String,   // 对应之前 ToolUse 的 id
        content: Vec<ToolResultContentBlock>,
        is_error: bool,        // 是否出错
    },
    // ...
}
```

AI 看到工具结果后，会继续思考，可能：
- 再调用下一个工具（形成链条）
- 生成最终的文本回答
- 承认任务完成

### 3.7 完整的一次"对话回合"示例

假设你说："帮我看看 main.rs 里有什么函数"：

```
第1轮：
  → 你: "帮我看看 main.rs 里有什么函数"
  → 云端 AI: ToolUse(read_file, {path: "src/main.rs"})
  → claw-code: 执行 read_file，返回文件内容
  → 云端 AI: "我看到 main.rs 里有 xxx 和 yyy 函数..."

如果 AI 觉得还需要更多信息：
  → 云端 AI: ToolUse(grep_search, {pattern: "fn ", path: "src/"})
  → claw-code: 执行 grep 搜索，返回结果
  → 云端 AI: "除了 main.rs，还有 zzz.rs 里的 functions..."
```

整个过程是一个**循环**——一路对话到模型输出 stop_reason=end_turn 才结束。

---

## 四、代码结构速查

如果你想自己看代码，关键文件在这里：

```
rust/
├── crates/
│   ├── api/                        # 云端 API 通信层
│   │   ├── src/
│   │   │   ├── client.rs           # ProviderClient：统一封装的 API 客户端
│   │   │   ├── types.rs            # 所有请求/响应/事件的数据结构
│   │   │   ├── sse.rs              # SSE 流解析器
│   │   │   ├── providers/
│   │   │   │   ├── anthropic.rs    # Anthropic Claude 专用客户端
│   │   │   │   ├── openai_compat.rs # OpenAI 兼容客户端（也用于 xAI、通义千问等）
│   │   │   │   └── mod.rs          # Provider 检测与模型路由
│   │   │   └── http_client.rs      # 底层 HTTP 客户端封装
│   │   │
│   │   └── tests/
│   │
│   ├── tools/                      # 工具实现层
│   │   ├── src/
│   │   │   └── lib.rs              # 所有 43 个工具的定义+执行函数（10346 行）
│   │   └── tests/
│   │
│   ├── runtime/                    # 运行时（会话管理、配置、权限等）
│   │   ├── src/
│   │   │   ├── session.rs          # 会话（对话记录）
│   │   │   ├── config.rs           # 配置管理
│   │   │   ├── permission_enforcer.rs # 权限执行器
│   │   │   ├── bash.rs             # bash 执行器
│   │   │   ├── mcp.rs / mcp_client.rs / mcp_server.rs # MCP 协议客户端/服务端
│   │   │   └── ...
│   │   └── tests/
│   │
│   ├── commands/                   # 斜杠命令（/help、/compact 等）
│   │   └── src/lib.rs
│   │
│   └── rusty-claude-cli/           # 最终的 CLI 入口程序
│       └── src/
│           ├── main.rs             # 程序入口
│           ├── init.rs             # 初始化
│           ├── input.rs            # 输入处理
│           └── render.rs           # 终端 UI 渲染
```

---

## 五、FAQ（常见问题）

**Q: AI 能直接访问我电脑上的文件吗？**  
A: 不能。AI 只能通过"工具"间接操作。所有工具调用都在你本地执行，且受权限系统控制。

**Q: 支持哪些 AI 模型？**  
A: 支持三类：Anthropic Claude（如 claude-sonnet-4）、OpenAI 兼容（如 GPT-4o、DeepSeek、Qwen）、xAI Grok。由模型名自动识别。

**Q: 什么是 SSE 流？**  
A: Server-Sent Events，即"服务器推送事件"。AI 的回复是逐字逐句"流"过来的，而不是一次性全返回。这样你能看到 AI 边思考边输出的过程，响应更快。

**Q: 什么是 ToolUse 和 ToolResult？**  
A: ToolUse 是 AI 说"我想调用这个工具"的消息；ToolResult 是系统说"工具执行完了，结果如下"的消息。两者形成一个"请求-响应"对。

**Q: 什么是 MCP？**  
A: Model Context Protocol（模型上下文协议），一种标准化的扩展方式。你可以通过 MCP 给 claw-code 添加自定义工具或数据源，就像安装插件一样。

---

> 本文基于 claw-code 源码（`rust/crates/tools/src/lib.rs`、`rust/crates/api/src/`、`rust/crates/runtime/src/session.rs` 等文件）编写。代码行数标注截至 2026 年 7 月。
