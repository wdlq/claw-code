# claw-code 子 agent（subagent）使用指南

> 本指南面向 claw-code 初学者，从零讲到进阶。所有机制均核对源码（`rust/crates/tools/src/lib.rs` + `rust/crates/runtime/src/`），非凭印象。

---

## 一、子 agent 是什么？

**子 agent 是主 agent（你和 claw 对话的那个）派出去的"小弟"**——独立跑一轮工具调用链，干完活返回结论，不和你交互。

类比：你是项目经理（主 agent），遇到简单子活（grep 全仓、code review、跑测试）就派给实习生（子 agent）干，自己专注规划决策。实习生干完汇报结果，不直接跟客户聊。

| 角色 | 模型 | 职责 | 能不能跟你交互 |
|------|------|------|---------------|
| 主 agent | 你选的（如 deepseek-v4-pro[1m]，贵但强） | 规划、决策、复杂推理、跟你对话 | ✅ 能 |
| 子 agent | 各自配置（如 deepseek-v4-flash，便宜快） | 简单子活（grep、review、debug） | ❌ 不能，只调工具+返回结论 |

**为什么用子 agent？** 两个收益：
1. **省钱**：简单子活用低级模型（v4-flash 比 v4-pro 便宜约 10 倍），省 80%+ 成本
2. **省上下文**：子 agent 独立 session，它调工具产生的一堆中间结果不占主 agent 的上下文——主 agent 只收最终结论，保持自己的上下文清爽

---

## 二、最简单的开通方式（3 步）

### Step 1：建配置文件

在项目根目录（或 `~/.claw/`）建 `.claw.json`：

```json
{
  "aliases": {
    "fast": "deepseek-v4-flash",
    "smart": "deepseek-v4-pro[1m]"
  },
  "subagents": {
    "code-review": {
      "model": "fast",
      "description": "Review code changes for bugs and security issues",
      "tools": ["read_file", "grep_search", "glob_search"]
    }
  }
}
```

### Step 2：主线程用 smart 模型启动

```bash
./claw.exe --model smart
# 或对话中切
> /model smart
```

### Step 3：对话中触发子 agent

两种触发方式：

**方式 A：主 agent 自动判断派活**（推荐，最自然）

直接对话：
```
> 帮我 review 一下刚才的改动

主 agent（smart）思考："这活适合 code-review 子 agent"
  → 派 Task(subagent_type="code-review", prompt="review recent changes")
    → 子 agent（fast，deepseek-v4-flash）跑工具链：
        read_file 改动文件 → grep_search 相关代码 → 返回 review 结论
主 agent 收到结论，整理给你
```

**方式 B：你显式点名派活**（精确控制）

```
> 用 code-review 子 agent 检查 src/auth.rs 的安全问题

主 agent → Task(subagent_type="code-review", prompt="检查 src/auth.rs 的安全问题")
```

---

## 三、`.claw.json` 配置详解

### 3.1 顶层结构

```json
{
  "aliases": {          // 模型别名，给长模型名起短名
    "fast": "deepseek-v4-flash",
    "smart": "deepseek-v4-pro[1m]"
  },
  "subagents": {         // 子 agent 定义
    "子agent名": {
      "model": "模型名或别名",
      "description": "职责描述",
      "tools": ["可用工具白名单"],
      "systemPrompt": "额外系统提示"
    }
  }
}
```

### 3.2 字段详解

| 字段 | 作用 | 必填 | 缺省值 |
|------|------|------|--------|
| `model` | 子 agent 用的模型（走 aliases 解析或直接写模型名） | 否 | `claude-opus-4-6`（DEFAULT_AGENT_MODEL） |
| `description` | 子 agent 职责描述——主 agent 派活时靠它判断"这活该不该派给这子 agent" | 否 | 空 |
| `tools` | 子 agent 可用的工具白名单（数组） | 否 | 按 `subagent_type` 给默认集（见 3.4） |
| `systemPrompt` | 子 agent 的额外系统提示（追加到默认 "You are a background sub-agent..." 后） | 否 | 空 |

### 3.3 `aliases`（模型别名）的妙用

`aliases` 是全局的模型短名映射，**子 agent 和主线程都能用**：

```json
{
  "aliases": {
    "fast": "deepseek-v4-flash",           // 便宜快
    "smart": "deepseek-v4-pro[1m]",        // 贵但强
    "haiku": "claude-haiku-4-5-20251213",  // DeepSeek 会映射到 v4-flash
    "opus": "claude-opus-4-6"              // DeepSeek 会映射到 v4-pro
  }
}
```

**DeepSeek 的模型映射规则**（源码核对 `docs/DeepseekAPI/1.md:52-54`）：
- `claude-opus` 开头 → DeepSeek 后端映射到 `deepseek-v4-pro`
- `claude-haiku`/`claude-sonnet` 开头 → 映射到 `deepseek-v4-flash`

所以你写 `fast: "claude-haiku-4-5"` 也行，DeepSeek 后端自动转 v4-flash。

### 3.4 预置 subagent_type 和默认工具集

如果你 `.claw.json` 里**不配 `tools` 字段**，claw 按 `subagent_type` 给默认工具白名单。源码核对 `allowed_tools_for_subagent`（lib.rs:3849）：

| subagent_type | 归一化名（源码用这个） | 默认工具集 | 适用场景 |
|---------------|----------------------|-----------|---------|
| `general`/`generalpurpose`/`generalpurposeagent` | `general-purpose` | 全量工具（bash/read/write/edit/grep/glob/Web 等） | 通用子活 |
| `explore`/`explorer`/`exploreagent` | `Explore` | read_file/glob/grep/WebFetch/WebSearch/ToolSearch/Skill/StructuredOutput | 探索代码、调研 |
| `plan`/`planagent` | `Plan` | Explore 的工具 + TodoWrite + SendUserMessage | 规划任务、列 todo |
| `verification`/`verify`/`verifier` | `Verification` | bash/read/glob/grep/Web + TodoWrite + SendUserMessage + PowerShell | 跑测试、验证改动 |
| `clawguide`/`guide` | `claw-guide` | read/glob/grep/Web + Skill | claw 自身使用帮助 |
| `statusline`/`statuslinesetup` | `statusline-setup` | bash/read/write/edit/glob/grep + ToolSearch | 配状态栏 |
| **其他任意名**（如你自配的 `code-review`） | 原名保留 | 默认全量工具集（bash/read/write/edit/grep/glob 等） | 自定义场景 |

**归一化意思是**：你派活时写 `subagent_type="verify"` 或 `"verifier"` 或 `"verification"`，claw 都认成 `Verification`，给对应的默认工具集。

**注意**：如果你在 `.claw.json` 里**显式配了 `tools` 字段**，就**覆盖默认集**——配 `tools: ["read_file"]` 那 子 agent 只能用 read_file，其他工具都调不了。

---

## 四、子 agent 的关键特性（源码核对）

| 特性 | 说明 | 源码依据 |
|------|------|---------|
| **独立模型** | 每个 subagent 可配自己的 model，不和主线程共享 | `build_agent_runtime` 的 `job.manifest.model`（lib.rs:3800-3804） |
| **独立工具白名单** | 子 agent 只能用配的 tools，不能越权调主线程的工具 | `allowed_tools_for_subagent`（lib.rs:3849） |
| **独立系统提示** | 子 agent 有自己的角色定位（"You are a background sub-agent of type X. Work only on the delegated task, do not ask the user questions, and finish with a concise result."） | `build_agent_system_prompt`（lib.rs:3825-3836） |
| **独立迭代上限** | 子 agent 默认最多 32 轮（DEFAULT_AGENT_MAX_ITERATIONS），避免死循环 | `build_agent_runtime` 的 `.with_max_iterations`（lib.rs:3789） |
| **不问用户问题** | 子 agent 是后台跑的，没有和你的交互通道，只能调工具+返回结果 | system prompt 明示 "do not ask the user questions"（lib.rs:3836） |
| **独立 session** | 子 agent 不共享主线程的对话历史，独立 session 从零跑 | `build_agent_runtime` 用 `Session::new()`（lib.rs:3811） |
| **独立缓存** | 子 agent 的请求走同一客户端，`with_model_context_window`/microcompact 都生效，但**独立 session 意味着独立前缀**——主线程的缓存帮不到它 | 同上 |

---

## 五、完整实操示例（针对 DeepSeek 场景）

### 5.1 目标

- 主线程：deepseek-v4-pro[1m]（贵但强）——规划、决策、复杂推理
- code-review 子 agent：deepseek-v4-flash（便宜快）——review 改动
- grep-all 子 agent：deepseek-v4-flash——全仓 grep
- debug 子 agent：deepseek-v4-flash——定位 bug

### 5.2 配置文件

```json
{
  "aliases": {
    "fast": "deepseek-v4-flash",
    "smart": "deepseek-v4-pro[1m]"
  },
  "subagents": {
    "code-review": {
      "model": "fast",
      "description": "Review code changes for bugs, security issues, and style problems. Return a prioritized list of findings.",
      "tools": ["read_file", "grep_search", "glob_search"],
      "systemPrompt": "You are a rigorous code reviewer. Check correctness > security > reliability. Report findings with file:line references."
    },
    "grep-all": {
      "model": "fast",
      "description": "Bulk grep across files and return matches. Use when the main agent needs to find all occurrences of a pattern.",
      "tools": ["grep_search", "glob_search", "list_directory"]
    },
    "debug": {
      "model": "fast",
      "description": "Debug and diagnose failures. Reproduce the error, read relevant code, identify root cause.",
      "tools": ["read_file", "grep_search", "bash", "edit_file", "glob_search"],
      "systemPrompt": "Follow the REPRODUCE → DIAGNOSE → FIX → VERIFY workflow. Run the failing command first to see the real error."
    },
    "architect": {
      "model": "smart",
      "description": "Plan complex refactors. Read code, propose plan, list affected files.",
      "tools": ["read_file", "grep_search", "glob_search", "list_directory", "TodoWrite"]
    }
  }
}
```

### 5.3 启动

```bash
./claw.exe --model smart
```

CLI 头部应显示：
```
Model            deepseek-v4-pro[1m]
```

### 5.4 对话中使用

**场景 1：主 agent 自动派活**
```
> 帮我看看 src/auth.rs 有没有安全问题

主 agent 思考："这是 review 活，适合 code-review 子 agent"
  → 派 Task(subagent_type="code-review", prompt="检查 src/auth.rs 的安全问题")
    → 子 agent（deepseek-v4-flash）跑：
        read_file src/auth.rs → grep_search 相关调用 → 返回发现列表
主 agent 收到列表，整理给你：
  "发现 3 处问题：
   1. src/auth.rs:42 密码明文比较，应用 hash
   2. ..."
```

**场景 2：你显式点名**
```
> 用 grep-all 子 agent 找全仓所有调 oldApi 的地方

主 agent → Task(subagent_type="grep-all", prompt="找全仓所有调 oldApi 的地方")
  → 子 agent 跑 grep_search → 返回匹配列表
主 agent 转交给你
```

**场景 3：多子 agent 协同**
```
> 这次改动很大，先让 debug 定位 bug，再让 code-review review 改动

主 agent → Task(subagent_type="debug", prompt="定位 X 失败的根因")
  → debug 子 agent 跑工具链 → 返回"根因是 Y 函数没处理 null"
主 agent → Task(subagent_type="code-review", prompt="review 修 Y 函数的改动")
  → code-review 子 agent → 返回 review 结论
主 agent 综两子 agent 的结论，给你最终答复
```

---

## 六、`/agents` 命令（交互式管理）

除了改 `.claw.json`，还能用 CLI 命令管子 agent：

```
/agents
```

进入交互式管理界面，可以：
- 列出所有配好的子 agent
- 创建新子 agent
- 编辑现有子 agent 的 model/tools/description
- 删除子 agent

对应代码 `commands/src/lib.rs:240`。

**还有个 `/subagent` 命令**（lib.rs:1003）——控制活动子 agent 执行，比如中止一个跑太久的子 agent。

---

## 七、子 agent 的限制和注意

### 7.1 不能和用户交互

子 agent 的系统提示硬编码了 "do not ask the user questions"——它只能调工具+返回结论。如果子 agent 遇到需要问你问题的情况，会**返回失败**让主 agent 处理。

### 7.2 独立 session = 不共享主线程上下文

子 agent 从空 session 起跑，**看不到主线程的对话历史**。你派活时要在 `prompt` 里把必要上下文传给它：

```
❌ 太简的派活：
   Task(subagent_type="code-review", prompt="review")
   → 子 agent 不知道 review 什么

✅ 带上下文的派活：
   Task(subagent_type="code-review", prompt="review src/auth.rs 的改动，重点是密码处理逻辑")
   → 子 agent 知道目标文件和重点
```

### 7.3 独立缓存 = 主线程缓存帮不到它

子 agent 走独立 session，前缀和主线程不同——主线程的 DeepSeek 硬盘缓存命中帮不到子 agent。但子 agent 自己的请求会进自己的缓存，多轮调用能命中。

### 7.4 工具白名单要配好

如果子 agent 需要调 `bash` 但你 `tools` 里没配，它会失败。**默认工具集**（见 3.4）已配好，但你显式配 `tools` 时要列全。

### 7.5 成本对比

DeepSeek V4 系列定价（参考）：

| 模型 | 输入价格 | 输出价格 | 适用 |
|------|---------|---------|------|
| deepseek-v4-pro[1m] | 高 | 高 | 规划、决策、复杂推理 |
| deepseek-v4-flash | 低（约 pro 的 1/10） | 低 | 简单子活（grep、review、debug） |

**省钱策略**：主线程用 pro，所有子 agent 用 flash。简单子活占 80% 工作量，能省 80%+ 成本。

---

## 八、常见问题

### Q1：子 agent 会自动触发吗？还是都要我手动派？

**会自动触发**——主 agent 看 `description` 判断当前活适不适合某子 agent，适合就自动派。你手动点名也能强制派。

### Q2：子 agent 能调子 agent 吗？

**不能**——子 agent 的工具白名单里没有 `Task` 工具，不能递归派。主 agent 才有 `Task`。

### Q3：子 agent 跑太久怎么 abort？

用 `/subagent` 命令中止，或 Ctrl+C 中断主 agent（会连带中止子 agent）。子 agent 默认最多 32 轮迭代，超了自动停。

### Q4：多个子 agent 能并行跑吗？

**能**——主 agent 可以同时派多个 `Task`，claw 会并行跑。对应代码 `commands/src/lib.rs:991` "Run commands in parallel subagents"。

### Q5：子 agent 的结果主 agent 能看到全部吗？

**能**——子 agent 返回的结论主 agent 全看到，但子 agent 调工具的**中间过程**（如 grep 的原始输出）不进主 agent 上下文——这正是子 agent 省上下文的机制。

### Q6：我配了 `.claw.json` 但子 agent 没生效？

排查：
1. `.claw.json` 在项目根目录或 `~/.claw/` 吗？claw 启动时会读这两个位置
2. JSON 格式对吗？用 `python -m json.tool .claw.json` 验证
3. `aliases` 里短名对应的模型名拼写对吗？
4. 主 agent 的 `description` 写清楚了吗？太模糊主 agent 不知道何时该派

### Q7：子 agent 和主线程用同一后端（DeepSeek）会冲突吗？

**不冲突**——claw 按 model 名路由到后端，`deepseek-v4-flash` 和 `deepseek-v4-pro[1m]` 都走 DeepSeek 的 Anthropic-compat 接口（`https://api.deepseek.com/anthropic`），用同一 API key，只是模型名不同。

---

## 九、快速起手模板（复制即用）

```json
{
  "aliases": {
    "fast": "deepseek-v4-flash",
    "smart": "deepseek-v4-pro[1m]"
  },
  "subagents": {
    "code-review": {
      "model": "fast",
      "description": "Review code changes for bugs and security issues",
      "tools": ["read_file", "grep_search", "glob_search"]
    },
    "grep-all": {
      "model": "fast",
      "description": "Bulk grep across files and return matches",
      "tools": ["grep_search", "glob_search", "list_directory"]
    },
    "debug": {
      "model": "fast",
      "description": "Debug and diagnose failures",
      "tools": ["read_file", "grep_search", "bash", "edit_file", "glob_search"]
    },
    "architect": {
      "model": "smart",
      "description": "Plan complex refactors",
      "tools": ["read_file", "grep_search", "glob_search", "list_directory", "TodoWrite"]
    }
  }
}
```

存成 `.claw.json` 放项目根，`./claw.exe --model smart` 启动，对话中主 agent 会自动调这些子 agent。

---

## 十、源码索引（方便查证）

| 机制 | 文件 | 行号 |
|------|------|------|
| 子 agent 默认模型 | `tools/src/lib.rs` | 3673 `DEFAULT_AGENT_MODEL` |
| 子 agent 默认迭代上限 | `tools/src/lib.rs` | 3675 `DEFAULT_AGENT_MAX_ITERATIONS = 32` |
| AgentJob 构造 | `tools/src/lib.rs` | 3697-3760 |
| `build_agent_runtime` | `tools/src/lib.rs` | 3797-3817 |
| `allowed_tools_for_subagent`（预置类型工具集） | `tools/src/lib.rs` | 3849-3916 |
| `build_agent_system_prompt` | `tools/src/lib.rs` | 3825-3845 |
| `normalize_subagent_type`（类型归一化） | `tools/src/lib.rs` | 5350-5366 |
| `/agents` 命令 | `commands/src/lib.rs` | 240 |
| `/subagent` 命令 | `commands/src/lib.rs` | 1003 |
| aliases 配置 | `runtime/src/config.rs` | 62, 390, 777 |
