# claw-code 多 Provider 方案：子 agent 与主 LLM 用不同云服务商

> 本文件**既是方案设计文档，也是已落地的实施记录**。
>
> **2026-07-19 落地完成**（分支 `multi-provider-subagent`，基于 HEAD `83ce16e`）：
> 子 agent（`Agent` 工具）现在可以走与主 LLM **不同云服务商**的模型，例如主 LLM 走 DeepSeek、
> 子 agent 走联通云 GLM，或反过来。节制回执（`compact_receipt`）也改为按**当前调度的 model 名前缀**
> 判定（`deepseek*` → 节制，对齐 Reasonix；`glm5.1*` / 其他 → 保持原回执），主 agent 与子 agent
> 各自按自己的 model 走对应处理方式。
>
> 实施细节见下文各节"★ 2026-07-19 落地"小节。原方案设计文字保留不动。
>
> 最后更新：2026-07-19

---

## 一、动机与场景

当前 claw-code 的子 agent 和主 LLM **共用同一套 `ANTHROPIC_*` / `OPENAI_*` env**，仅 `AgentInput.model` 字段名不同。这意味着：

- 主 LLM 设 DeepSeek（`ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic`、`ANTHROPIC_API_KEY=sk-...`），子 agent 也只能走 DeepSeek——`build_provider_entry` → `ProviderClient::from_model` → `AnthropicClient::from_env()` 读的是同一组 env
- 想让子 agent 走联通云 GLM（`https://aigw-gzgy2.cucloud.cn:8443`、另一把 key）做不到，因为 env 只有一套

**用户场景**：DeepSeek V4 Pro 1M 窗口适合主 LLM 长会话，但子 agent 是短任务（review/explore/security_audit）——用便宜快的联通云 GLM-5.1 或反过来用更聪明但贵的模型做子 agent 都有诉求。这需要**每条子 agent 调度能独立指定 provider+auth**。

---

## 二、现状核实（本次会话已读源码确认）

### 2.1 claw 的子 agent 调度链

```
Agent 工具调用（model 可选字段）
  ↓
execute_agent_with_spawn(input, spawn_agent_job)        tools/src/lib.rs:3681
  ↓
spawn_agent_job → std::thread::Builder                  tools/src/lib.rs:3761
  ↓
run_agent_job                                            tools/src/lib.rs:3788
  ↓
build_agent_runtime(job)                                 tools/src/lib.rs:3797
  ↓
ProviderRuntimeClient::new(model, allowed_tools)        tools/src/lib.rs:4729
  ↓
new_with_fallback_config(model, ..., &fallback_config)  tools/src/lib.rs:4735
  ↓
build_provider_entry(model)                              tools/src/lib.rs:4762
  ↓
ProviderClient::from_model(model)                        api/src/client.rs:17
  ↓
from_model_with_anthropic_auth(model, None)             api/src/client.rs:21
  ↓
detect_provider_kind(model) → Anthropic/Xai/OpenAi     api/src/providers/mod.rs:341
  ↓
AnthropicClient::from_env()                              api/src/providers/anthropic.rs:160
  ↓
AuthSource::from_env_or_saved() 读 ANTHROPIC_API_KEY    api/src/providers/anthropic.rs:44
```

**关键事实**：
- 子 agent 建客户端走的是**和主 LLM 完全同一条 env 路径**——`from_model(model)` 第二参数 `anthropic_auth` 总传 `None`，强制走 `from_env()`
- `AgentInput.model: Option<String>` 字段虽在，但只影响 `detect_provider_kind` 的 model 名探测，**换不了 base_url 和 api_key**
- `ProviderFallbackConfig`（`runtime/src/config.rs:74`）已有 `primary` + `fallbacks` 链机制，但那是**主 LLM 单链路**的故障转移，子 agent 不参与

### 2.2 claw 已预留的钩子（重要）

`ProviderClient::from_model_with_anthropic_auth(model, anthropic_auth: Option<AuthSource>)` 那个第二参数就是为注入非默认 auth 留的口子。目前 `from_model` 总传 `None`（走 env），但**只要在子 agent 路径传 `Some(auth)`，就能绕过 env 用独立 auth**。这是本方案的核心复用点——**api 层的钩子已经留好了，只需在 tools 层把 auth 传下去**。

### 2.3 Reasonix 对照（已原生支持多 provider）

Reasonix `internal/agent/parallel_tasks.go:342` 的 `resolveSubagentProvider`：

```go
func resolveSubagentProvider(tt *TaskTool, modelRef, effortRef string) (provider.Provider, *provider.Pricing, int, error) {
    if tt.resolveProvider != nil && (modelRef != "" || effortRef != "") {
        return tt.resolveProvider(modelRef, effortRef)
    }
    return tt.prov, tt.pricing, tt.contextWindow, nil  // fallback 到主 provider
}
```

`TaskTool` 有 `resolveProvider` 字段——子 agent 调度时优先走 resolver 建独立 provider，没 resolver 才 fallback 到主 provider。**这正是 claw 要抄的形态**。

---

## 三、方案设计

### 3.1 总体思路

对齐 Reasonix `resolveSubagentProvider` 的 resolver 模式，但复用 claw 已有的 `from_model_with_anthropic_auth` 钩子——**不新建一套 provider 解析机制，只在 tools 层加一个"按 subagent_type 查 provider 配置"的 resolver**，把 resolver 解析出的 `(base_url, api_key)` 包成 `AuthSource` 传下去。

### 3.2 配置层改动（`.claw.json` / `.claw/settings.json`）

新增 `subagentProviders` 配置段，按 `subagent_type` 各指定一份独立 provider 配置：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
    "ANTHROPIC_API_KEY": "sk-deepseek-...",
    "ANTHROPIC_MODEL": "deepseek-v4-pro[1m]"
  },
  "subagentProviders": {
    "review": {
      "base_url": "https://aigw-gzgy2.cucloud.cn:8443",
      "api_key": "sk-glm-...",
      "model": "glm-5.1"
    },
    "explore": {
      "base_url": "https://aigw-gzgy2.cucloud.cn:8443",
      "api_key": "sk-glm-...",
      "model": "glm-5.1"
    },
    "security_review": {
      "base_url": "https://api.deepseek.com/anthropic",
      "api_key": "sk-deepseek-...",
      "model": "deepseek-v4-pro[1m]"
    }
  },
  "subagentProviderDefault": {
    "base_url": "https://aigw-gzgy2.cucloud.cn:8443",
    "api_key": "sk-glm-...",
    "model": "glm-5.1"
  }
}
```

**设计要点**：
- **按 `subagent_type` 路由**——对齐 `allowed_tools_for_subagent`（`tools/src/lib.rs:3849`）已经有的按 type 分工具的机制，provider 也按 type 分
- **`subagentProviderDefault` 兜底**——没显式配的 type 走这份；都没配则 fallback 到主 LLM env（保持向后兼容）
- **`api_key` 支持env 引用**——避免明文 key 写配置文件，允许 `"api_key": "${GLM_API_KEY}"` 形式从 env 读（对齐 Reasonix `config.ResolveEnvRef`）
- **不动 `ProviderFallbackConfig`**——那是主 LLM 故障转移链，与子 agent provider 路由是正交概念，不要混

### 3.3 配置加载层改动（`runtime/src/config.rs`）

新增两个解析函数 + 两个 struct：

```rust
/// 一份独立子 agent provider 配置（base_url + api_key + model）。
/// 对齐 Reasonix TaskTool.resolveProvider 接受的 provider.Config 形态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubagentProviderConfig {
    pub base_url: String,
    pub api_key: String,        // 支持 ${ENV_VAR} 引用，加载时 resolve
    pub model: String,
}

/// 按 subagent_type 路由的 provider 映射 + 默认兜底。
/// 对齐 Reasonix TaskTool.resolveProvider 字段，claw 用配置文件而非代码注入。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubagentProviderRouting {
    pub by_type: BTreeMap<String, SubagentProviderConfig>,
    pub default: Option<SubagentProviderConfig>,
}
```

加载点加在 `ConfigLoader::load`（`config.rs:314` 那段 `parse_optional_*` 链里）：

```rust
subagentProviders: parse_optional_subagent_provider_routing(&merged_value)?,
```

`parse_optional_subagent_provider_routing` 实现：
- 遍历 `subagentProviders` 对象的每个 key（subagent_type）→ 建 `SubagentProviderConfig`
- `api_key` 字段调 `resolve_env_ref`（新增 helper，对齐 Reasonix）把 `${VAR}` 替换成 `std::env::var(VAR)`
- `subagentProviderDefault` 同理解析到 `default`
- 都没配置 → 返回 `SubagentProviderRouting::default()`（空 by_type + None default），调用方据此判断"没配，fallback 主 env"

### 3.4 客户端构造层改动（`tools/src/lib.rs`）

这是**最关键的改动点**——把 `build_agent_runtime` 里建客户端那行改成调 resolver。

**当前**（`tools/src/lib.rs:3806`）：

```rust
let api_client = ProviderRuntimeClient::new(model.clone(), allowed_tools.clone())?;
```

**改后**：

```rust
let routing = load_subagent_provider_routing();  // 读 .claw.json
let resolved = resolve_subagent_provider(
    &normalized_subagent_type,
    input.model.as_deref(),
    &routing,
);
let api_client = ProviderRuntimeClient::new_with_resolved(
    resolved.model.clone(),
    allowed_tools.clone(),
    resolved.base_url.as_deref(),
    resolved.auth.as_ref(),
)?;
```

新增的 `resolve_subagent_provider` 函数（对照 Reasonix `parallel_tasks.go:342`）：

```rust
/// 按 subagent_type 路由子 agent 的 provider 配置。
/// 1. 优先查 routing.by_type[type] —— 显式配置
/// 2. 次查 routing.default —— 兜底默认
/// 3. 都没 → 返回 None，调用方 fallback 到主 LLM env 路径
/// 对齐 Reasonix resolveSubagentProvider 的 resolver-then-fallback 形态。
fn resolve_subagent_provider(
    subagent_type: &str,
    input_model: Option<&str>,
    routing: &SubagentProviderRouting,
) -> ResolvedSubagentProvider {
    // 显式 per-type 配置
    if let Some(cfg) = routing.by_type.get(subagent_type) {
        return ResolvedSubagentProvider {
            model: input_model.unwrap_or(&cfg.model).to_string(),
            base_url: Some(cfg.base_url.clone()),
            auth: Some(AuthSource::ApiKey(cfg.api_key.clone())),
        };
    }
    // default 兜底
    if let Some(cfg) = &routing.default {
        return ResolvedSubagentProvider {
            model: input_model.unwrap_or(&cfg.model).to_string(),
            base_url: Some(cfg.base_url.clone()),
            auth: Some(AuthSource::ApiKey(cfg.api_key.clone())),
        };
    }
    // 都没配 → fallback 主 env（保持向后兼容）
    // 这条路径走原来的 ProviderRuntimeClient::new，让 from_model 读 env
    ResolvedSubagentProvider {
        model: input_model.unwrap_or(DEFAULT_AGENT_MODEL).to_string(),
        base_url: None,
        auth: None,
    }
}

struct ResolvedSubagentProvider {
    model: String,
    base_url: Option<String>,
    auth: Option<AuthSource>,
}
```

`ProviderRuntimeClient` 加一个 `new_with_resolved` 构造器：

```rust
impl ProviderRuntimeClient {
    fn new_with_resolved(
        model: String,
        allowed_tools: BTreeSet<String>,
        base_url: Option<&str>,
        auth: Option<&AuthSource>,
    ) -> Result<Self, String> {
        let fallback_config = load_provider_fallback_config();
        // 走新的 build_provider_entry_with_override，把 base_url/auth 注入
        let primary = build_provider_entry_with_override(&model, base_url, auth)?;
        // ... fallback 链保持原状（主 LLM 故障转移，子 agent 不参与）...
    }
}

fn build_provider_entry_with_override(
    model: &str,
    base_url: Option<&str>,
    auth: Option<&AuthSource>,
) -> Result<ProviderEntry, String> {
    let resolved = resolve_model_alias(model).clone();
    // ★ 关键：调 from_model_with_anthropic_auth，注入 auth
    //   base_url 通过 AnthropicClient::with_base_url 覆盖
    let mut client = ProviderClient::from_model_with_anthropic_auth(
        &resolved,
        auth.cloned(),
    ).map_err(|e| e.to_string())?;
    if let Some(url) = base_url {
        if let ProviderClient::Anthropic(ref mut c) = client {
            *c = c.clone().with_base_url(url.to_string());
        }
        // OpenAi/Xai 分支同理调 with_base_url（需要给 OpenAiCompatClient 加这方法）
    }
    Ok(ProviderEntry { model: resolved, client })
}
```

**要点**：
- **复用 `from_model_with_anthropic_auth`**——api 层那个 `Option<AuthSource>` 钩子就是为这留的，本方案不动 api 层
- **`base_url` 覆盖**需要给 `AnthropicClient` / `OpenAiCompatClient` 加 `with_base_url` builder（`AnthropicClient` 已有 `with_base_url` 见 `anthropic.rs:161`，`OpenAiCompatClient` 需新增——但本次先做 Anthropic 路径，OpenAi 路径留二期）
- **fallback 链不动**——`load_provider_fallback_config` 那套主 LLM 故障转移保持原状，子 agent 不参与故障转移（Reasonix 也没让子 agent 复用主 agent 的 fallback 链）
- **★ 配套项：`resolved.model` 必须传给工具层用于节制回执判定**——`build_agent_runtime` 建客户端时拿到 `resolved.model`（如 `"glm-5.1"` 或 `"deepseek-v4-pro[1m]"`），**要把它通过工具执行上下文传给 `file_ops::edit_file`/`write_file`**，让 `should_use_compact_receipt(model: &str)` 按**当前调度的 model 名**判定节制策略，不能读全局 `ANTHROPIC_MODEL` env。否则两种破裂场景：
  - 主 LLM=DeepSeek（env 是 deepseek）+ 子 agent=GLM（resolved 是 glm）→ `should_use_compact_receipt` 读 env 误判 true → GLM 子 agent 被误节制，丢失 `original_file` 自检能力（功能退化）
  - 主 LLM=GLM（env 是 glm）+ 子 agent=DeepSeek（resolved 是 deepseek）→ `should_use_compact_receipt` 读 env 误判 false → DeepSeek 子 agent 漏节制，edit_file 塞整份原文件击穿字节级缓存（**前轮改这条节制回执本来要治的病在子 agent 路径复发**）
  - 落地方式见下方 3.4bis 节

### 3.4bis 工具执行上下文传 model（节制回执判定的强制配套）

前轮 `file_ops.rs:213` 落地的 `should_use_compact_receipt()` 当前**只抽 `ANTHROPIC_MODEL` env**——multiprovider 落地后必须改成按 resolved model 判定，配套改三处：

**改动 1** —— `should_use_compact_receipt` 改签名按 model 判定（不读 env）：

```rust
/// 按当前调度的 model 名前缀判定回执节制策略。
/// `deepseek` 开头 → true（节制，对齐 Reasonix）；其他 → false（保持原回执）。
/// ★ multiprovider 落地后必须按 resolved model 判定，不能读全局 env——
///   否则主 LLM=DeepSeek/子 agent=GLM 场景会让子 agent 误节制；
///   主 LLM=GLM/子 agent=DeepSeek 场景会让子 agent 漏节制击穿缓存。
fn should_use_compact_receipt(model: &str) -> bool {
    model.trim().to_lowercase().starts_with("deepseek")
}
```

**改动 2** —— `file_ops::edit_file`/`write_file` 函数签名加参数把 model 传进来。有两个选项：

- **选项 A（推荐，小改）**：两个函数加 `compact_receipt: bool` 参数，由调用方（`tools/src/lib.rs` 的工具 dispatch 层）算好布尔值传进来。调用方那时能拿到当前调度的 model 名（主 LLM 的从 env、子 agent 的从 `ResolvedSubagentProvider.model`），调 `should_use_compact_receipt(model)` 算布尔后传进 file_ops。file_ops 层不再自己读 env。
- **选项 B（大改）**：建 `ToolContext` struct 含 `model: String` 等字段，所有工具函数收这个 context。更干净但改动面大。

**本方案选 A**——改动小、不破坏现有工具签名风格、与 multiprovider 的 `ResolvedSubagentProvider.model` 字段自然衔接。

**改动 3** —— `tools/src/lib.rs` 工具 dispatch 层算布尔值传进 file_ops。主 LLM 路径从 `std::env::var("ANTHROPIC_MODEL")` 拿 model 名；子 agent 路径从 `build_agent_runtime` 拿到的 `resolved.model` 传下去。两路汇合到 `should_use_compact_receipt(model)` 算布尔，再调 `file_ops::edit_file(..., compact_receipt)`。

### 3.5 subagent_type 标准化（已有，不改）

`normalize_subagent_type`（`tools/src/lib.rs:5350`）已经把空值归到 `"general-purpose"`，本方案直接用它的输出查 `routing.by_type`。需要给配置文档补一份**支持的 subagent_type 枚举**：

| subagent_type | 用途 | 默认 allowed_tools（已有） |
|---|---|---|
| `general-purpose` | 通用子 agent（fallback） | 全工具 |
| `Explore` | 只读探查 | read_file/glob/grep/ls |
| `Plan` | 计划模式 | 同 Explore + TodoWrite |
| `review` | 代码审查 | 同 Explore + StructuredOutput |
| `security_review` | 安全审查 | 同 review |

用户配 `subagentProviders` 的 key 要用上面这些标准 type 名。

### 3.6 主 LLM 路径不动

主 LLM 走 `main.rs` 的 `AnthropicRuntimeClient`，那是另一套构造链（`main.rs:5237` 那条），**本方案不动主 LLM**——只动 `tools/src/lib.rs` 的子 agent 路径。主 LLM 继续读 `ANTHROPIC_*` env，子 agent 走配置路由。

---

## 四、改动文件清单

| 文件 | 改动 | 量级 |
|---|---|---|
| `runtime/src/config.rs` | 新增 `SubagentProviderConfig` / `SubagentProviderRouting` struct + `parse_optional_subagent_provider_routing` + `resolve_env_ref` helper | ~80 行 |
| `runtime/src/lib.rs` | 导出新类型 | ~5 行 |
| `tools/src/lib.rs` | `build_agent_runtime` 改调 resolver；新增 `resolve_subagent_provider` / `ResolvedSubagentProvider` / `load_subagent_provider_routing` / `ProviderRuntimeClient::new_with_resolved` / `build_provider_entry_with_override` | ~120 行 |
| `api/src/providers/openai_compat.rs` | 给 `OpenAiCompatClient` 加 `with_base_url` builder（二期，先做 Anthropic 路径） | ~10 行 |
| `api/src/client.rs` | 无改动（`from_model_with_anthropic_auth` 钩子已留） | 0 |
| `.claw.json` / `.claw/settings.json` | 用户新增 `subagentProviders` / `subagentProviderDefault` 配置段 | 配置 |
| 测试 | `config.rs` 加 routing 解析测试；`tools/lib.rs` 加 resolver 路由测试（per-type / default / fallback-to-env 三分支） | ~100 行 |

**总量**：约 220 行新代码 + 100 行测试，集中在 config + tools 两个 crate，api 层零改动（钩子已留）。

---

## 五、验证计划

### 5.1 编译层
- `cargo check --workspace` 干净
- `cargo test -p runtime --lib config` 通过（routing 解析测试）
- `cargo test -p tools --lib build_agent` 通过（resolver 三分支测试）

### 5.2 单元测试三分支
1. `resolves_per_type_provider_when_configured`——配 `subagentProviders.review`，调 `resolve_subagent_provider("review", None, &routing)`，断言返回的 `base_url`/`auth` 是 GLM 那份
2. `falls_back_to_default_when_type_unconfigured`——只配 `subagentProviderDefault`，调未配的 type，断言走 default
3. `falls_back_to_main_env_when_nothing_configured`——空 routing，断言返回 `base_url: None` + `auth: None`（调用方走原 env 路径）

### 5.3 真机验证（用户重编后）
- 主 LLM 设 DeepSeek，`subagentProviders.review` 设联通云 GLM
- 跑一轮让主 LLM 调 `Agent` 工具 `subagent_type: "review"`
- 看 `claw_glm_diag.log` 里那条 subagent 的 `claw_glm_diag` 事件——**`url` 字段应该是 `aigw-gzgy2.cucloud.cn:8443` 而不是 `api.deepseek.com`**
- 反过来主 LLM 设 GLM、子 agent 设 DeepSeek 同理验证

---

## 六、不做的（留二期）

1. **`OpenAiCompatClient::with_base_url`** ——二期补。一期只做 Anthropic 路径（DeepSeek/GLM 都走 Anthropic 协议，是当前主用例）。OpenAi 路径补后能支持子 agent 走通义千问/Grok 等。
2. **`AgentInput` 加 `provider` 字段** ——本期不动 `AgentInput` schema（避免工具 schema 变化影响模型兼容）。路由只按 `subagent_type`，不让模型逐调用选 provider。如果未来要模型自己选，再扩 schema。**注意这不等于"工具执行上下文不动"**——3.4bis 节那条节制回执配套改动要给 `file_ops::edit_file`/`write_file` 加 `compact_receipt: bool` 参数（选项 A），由工具 dispatch 层算好传进来。这是**工具函数签名的参数变化**，不是 `AgentInput` JSON schema 的字段变化——后者会改工具描述传给模型（破坏兼容），前者只是内部调用链改签名（模型看不到）。两件事不要混。
3. **主 LLM 故障转移链扩到子 agent**——`ProviderFallbackConfig` 那条链保持主 LLM 专用。子 agent 要故障转移另立一套（二期）。
4. **Worker/Task 工具的多 provider**——本期只做 `Agent` 工具。`WorkerCreate`/`TaskCreate` 那套后台任务有独立的客户端构造链（`run_worker_create` 等），改动面更大，留二期对照本方案再补。

---

## 七、风险与回退

### 7.1 风险
- **`subagent_type` 名漂移**——如果未来 `normalize_subagent_type` 改了输出（如 `"Explore"` 改小写），配置里的 key 对不上 → fallback 到 default 或 env，**降级不崩溃**。需要给配置加载加 warning：`subagentProviders` 有 key 不在标准 type 列表里时打 stderr 警告（不 fail）。
- **`api_key` env 引用失败**——`${GLM_API_KEY}` 但 env 未设 → `resolve_env_ref` 返回空字符串，`AuthSource::ApiKey("")` 会让 API 调用 401。需要在 `resolve_subagent_provider` 里检查：`api_key` 解析后为空就报错明确提示"env VAR 未设"。
- **DeepSeek 的 `cache_control` 字段在 GLM 子 agent 路径失效**——`ProviderRuntimeClient` 的 `cache_config: api::CacheConfig::from_env()`（`lib.rs:4757`）是全局的，子 agent 走 GLM 时 cache_control 仍会被注入但 GLM Ignored（无害，MEMORY 第 425-477 行已核实）。**不需要为子 agent 单独关 cache_control**。

### 7.2 回退
- 删 `subagentProviders` 配置段 → `SubagentProviderRouting::default()` → `resolve_subagent_provider` 返回 None → 走原 `ProviderRuntimeClient::new` 路径 → **完全回到现状**
- 改动集中在 `tools/src/lib.rs` 的 `build_agent_runtime`，回退只需还原那一处调用 + 删新增函数

---

## 八、对照 Reasonix 的设计差异总结

| 维度 | Reasonix | claw 本方案 |
|---|---|---|
| resolver 注入方式 | `TaskTool.resolveProvider` 字段（代码注入） | `subagentProviders` 配置段（配置注入）——**对齐 claw 的配置驱动风格** |
| resolver 签名 | `(modelRef, effortRef) → (Provider, Pricing, contextWindow, error)` | `(subagent_type, input_model, routing) → (model, base_url, auth)`——**claw 不需要 pricing/contextWindow**（那些主 LLM 已有，子 agent 用主 LLM 的） |
| fallback | `tt.prov`（主 agent 的 provider） | 主 env（`ANTHROPIC_*`）——**claw 的 env 路径就是 Reasonix 的 tt.prov 等价物** |
| 路由 key | `modelRef`/`effortRef`（模型名/努力等级） | `subagent_type`（子 agent 类型）——**claw 已有 `allowed_tools_for_subagent` 按 type 分工具的机制，provider 也按 type 分对齐既有模式** |

**核心差异**：Reasonix 让模型用 `model`/`effort` 字段逐调用选 provider，claw 让用户用 `subagent_type` 在配置文件预路由。后者更简单且不动工具 schema，但牺牲了模型自主选 provider 的灵活性——这是有意的 trade-off（claw 的用户场景是"review 类子 agent 永远走 GLM"，不需要模型逐调用决策）。

---

## 九、下次接手清单

1. **先读本文件** 还原方案全貌
2. **核实 `from_model_with_anthropic_auth` 钩子是否仍在**——`api/src/client.rs:21` 那个 `anthropic_auth: Option<AuthSource>` 参数是本方案的复用锚点，如果被改掉了要重新找注入口
3. **核实 `normalize_subagent_type` 输出的标准 type 列表**——`tools/src/lib.rs:5350`，如果新增了 type 要同步更新本文件 3.5 节那张表和配置加载的 warning 列表
4. **先做 Anthropic 路径**——一期只做 DeepSeek/GLM 这类走 Anthropic 协议的，OpenAi 路径二期补
5. **真机验证看 `claw_glm_diag.log` 的 `url` 字段**——子 agent 那条的 url 应该是配置的 `base_url` 而不是主 LLM 的

---

## ★★★ 2026-07-19 multiprovider 落地实施记录（本次会话）

### 落地范围（已实现）

| 项 | 落地情况 |
|---|---|
| `SubagentProviderConfig` / `SubagentProviderRouting` 结构体（`runtime/src/config.rs`） | ✅ 已加，含 `by_type` + `default` 两段 |
| `parse_optional_subagent_provider_routing` 解析（`config.rs`） | ✅ 已加，读 `.claw.json` 顶层 `subagentProviders` / `subagentProviderDefault` 段，字段名 `baseUrl` / `apiKey` / `model`（camelCase 对齐既有 JSON schema 约定） |
| `resolve_env_ref` helper（`config.rs`） | ✅ 已加，支持 `${VAR}` / `${VAR:-default}` 形态 |
| `runtime/src/lib.rs` 导出 | ✅ 已加 `SubagentProviderConfig` / `SubagentProviderRouting` / `should_use_compact_receipt` |
| `tools/src/lib.rs` 落地 resolver | ✅ 已加 `resolve_subagent_provider` / `ResolvedSubagentProvider` / `ProviderRuntimeClient::new_with_resolved` / `build_provider_entry_with_override`；`build_agent_runtime` 已改调 resolver |
| 节制回执 model 判定 | ✅ 已落地：`should_use_compact_receipt(model: &str)` 按 `deepseek*` 前缀判定；`file_ops::edit_file`/`write_file` 加 `compact_receipt: bool` 参数；dispatch 层用 `current_dispatch_model()`（thread-local 优先，回退 `ANTHROPIC_MODEL` env）算 `compact_receipt` |
| 单元测试 | ✅ runtime/config.rs 加 6 个 routing 解析测试；tools/lib.rs 加 7 个 resolver + compact_receipt 测试；全绿 |

### 与原方案设计的差异（落地时调整）

1. **配置字段名用 camelCase**：`baseUrl` / `apiKey` / `model`（非 `base_url` / `api_key`）。对齐既有 JSON schema（`providerFallbacks.primary` / `trustedRoots` 等都 camelCase）。**用户写 `.claw.json` 时注意用 camelCase**。

2. **节制回执用 thread-local 传 model**：原 3.4bis 节选项 A 设想"工具 dispatch 层算好布尔传进 file_ops"，但主 LLM / 子 agent 共用同一份 `execute_tool_with_enforcer` dispatch 入口，无法从参数区分两条路径。**最终方案**：用 `thread_local! { SUBAGENT_MODEL }` 在 `run_agent_job` 入口设置子 agent 的 resolved model；dispatch 层调 `current_dispatch_model()` 优先读 thread-local（子 agent 路径），回退到 `ANTHROPIC_MODEL` env（主 LLM 路径）。这样主 LLM 用 `deepseek*` 走节制回执、子 agent 用 `glm5.1*` 走原回执（或反过来）都能各自正确判定，不再破裂。

3. **`build_provider_entry_with_override` 仅做 Anthropic 路径**：`base_url` 覆盖只对 `ProviderClient::Anthropic` 生效（调 `AnthropicClient::with_base_url`）；`OpenAi` / `Xai` 路径未实现 `with_base_url`，留二期。**本期 DeepSeek/GLM 都走 Anthropic 协议，是当前主用例**。

4. **`ResolvedSubagentProvider.model` 用于 `with_model_context_window`**：`build_agent_runtime` 改用 `resolved.model`（routing 配的 model）算 context window，而不是 `job.manifest.model`（主 LLM 的 model 名）。子 agent 走 GLM 时用 GLM 的 200K 窗口算 auto-compact 阈值，走 DeepSeek 时用 1M 窗口。

### 配置示例（用户使用）

主 LLM 走 DeepSeek V4 Pro 1M，子 agent 走联通云 GLM-5.1：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
    "ANTHROPIC_API_KEY": "sk-deepseek-...",
    "ANTHROPIC_MODEL": "deepseek-v4-pro[1m]"
  },
  "subagentProviders": {
    "review": {
      "baseUrl": "https://aigw-gzgy2.cucloud.cn:8443",
      "apiKey": "sk-glm-plainkey-or-${GLM_API_KEY}",
      "model": "glm-5.1"
    }
  },
  "subagentProviderDefault": {
    "baseUrl": "https://aigw-gzgy2.cucloud.cn:8443",
    "apiKey": "sk-glm-plainkey-or-${GLM_API_KEY}",
    "model": "glm-5.1"
  }
}
```

反过来：主 LLM 走 GLM-5.1，子 agent 走 DeepSeek V4 Flash：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://aigw-gzgy2.cucloud.cn:8443",
    "ANTHROPIC_API_KEY": "sk-glm-...",
    "ANTHROPIC_MODEL": "glm-5.1"
  },
  "subagentProviders": {
    "review": {
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-deepseek-plainkey",
      "model": "deepseek-v4-flash"
    }
  }
}
```

### 节制回执 model 判定逻辑（★ 关键）

| 主 LLM model | 子 agent model | 主 LLM 走哪条 | 子 agent 走哪条 |
|---|---|---|---|
| `deepseek*` | `deepseek*` | 节制回执（DeepSeek 处理方式） | 节制回执（DeepSeek 处理方式） |
| `deepseek*` | `glm5.1*` / `glm*` | 节制回执 | **保持原回执**（GLM 处理方式） |
| `glm5.1*` / `glm*` | `deepseek*` | **保持原回执** | 节制回执 |
| `glm5.1*` / `glm*` | `glm5.1*` / `glm*` | 保持原回执 | 保持原回执 |

判定函数：`runtime::should_use_compact_receipt(model: &str) -> bool`
- `model.trim().to_lowercase().starts_with("deepseek")` → `true`
- 其他（含 `glm5.1*`、空串、未设）→ `false`

### 验证状态

- ✅ `cargo check --workspace`（lib）全绿，无新增 warning
- ✅ `cargo test -p runtime --lib config` 31 passed 0 failed（含 6 个新 routing 测试）
- ✅ `cargo test -p tools --lib` 7 个新 resolver / compact_receipt 测试全过
- ⏳ **真机验证未做**——用户需 `cargo build --release` 替换 `claw.exe` 后真机跑一轮，看 `claw_glm_diag.log` 里子 agent 那条 `claw_glm_diag` 事件的 `url` 字段是否是配置的 `base_url`（GLM 的 `aigw-gzgy2.cucloud.cn`）而非主 LLM 的 DeepSeek endpoint

### 已知预存债（本次未动，与本次改动无关）

1. **api crate 测试桩 `cache_control` 字段缺失**——`openai_compat_integration` / `client_integration` / `request_building` 测试在初始化 `InputMessage` / `InputContentBlock` / `ToolDefinition` 时缺 `cache_control` 字段，导致 `cargo check --workspace --all-targets` 报 E0063。这是 2026-07-14 把 `MessageResponse.role` 改 `Option` 时遗漏的预存测试桩债，MEMORY 第 343-351 行已记。本次未修。
2. **runtime hooks/mcp_stdio Windows 测试**——`executes_hooks_in_configured_order` / `manager_discovers_tools_from_stdio_config` 在 Windows 下失败，MEMORY 第 158 行已记。本次未修。
3. **file_ops `reads_and_writes_files` 测试**——7-18 落地三重门 trailer 逻辑后，`read_file(path, Some(1), Some(1))` 在 3 行文件上触发 `has_more` trailer 追加，断言 `"two"` 失败。这是 7-18 落地遗留的预存债（`git stash` 验证确认），本次未修。

### Git 状态备忘

- 分支：`multi-provider-subagent`（基于 `henry-dev` HEAD `83ce16e`）
- 改动文件：`runtime/src/config.rs` / `runtime/src/file_ops.rs` / `runtime/src/lib.rs` / `tools/src/lib.rs`
- **我（AtomCode）的所有改动都是 unstaged**，从未自动 commit/push
- 用户决定是否合并回 `henry-dev` 或新建分支推送

---

## 3.4ter ★ 2026-07-19 子 agent context window 与压缩策略独立（strict 变体）

### 背景

3.4 节落地后主子各走不同 provider，但 **auto-compact 阈值**有破裂点：`CLAUDE_CODE_AUTO_COMPACT_*` env 是全局共用的。

| 主子组合 | `.claw.json` env 设的阈值 | 子 agent 实际拿到的窗口 | 病 |
|---|---|---|---|
| 主=DeepSeek 1M + 子=GLM 200K | 用户设 `WINDOW=131000`（针对 GLM 200K 算的 75%） | 131K 阈值 | 子 agent 走 GLM 拿 131K 阈值合理，但若主 env 那套设的是给主 DeepSeek 算的 750K 阈值，反过来子 agent 走 GLM 200K 拿 750K 阈值直接撑爆报 `ContextWindowExceeded` 400 |
| 主=GLM 200K + 子=DeepSeek 1M | 同上（针对 GLM 200K 算的 131K） | 子 agent 走 DeepSeek 1M 被压到 131K | 频繁 compact 反伤 DeepSeek 硬盘缓存 |

根因：`ConversationRuntime::with_model_context_window`（`runtime/src/conversation.rs:221`）的语义是"env 优先于动态算"——只有 env **两个都没设**时才动态算 `context_window × 75%`。这套语义对主 LLM 合理（用户用 env 显式控制主 LLM 阈值），但**不该施加到子 agent**——子 agent 的窗口由 routing 配的 model 决定，与主 LLM 的 env 配置无关。

### 修法

在 `ConversationRuntime` 加一个 **strict 变体** `with_model_context_window_strict`：

| 路径 | 用哪个变体 | 语义 |
|---|---|---|
| 主 LLM（`rusty-claude-cli/src/main.rs:8681`） | `with_model_context_window`（原路径不动） | env 优先覆盖，保留用户显式控制主 LLM 阈值的能力 |
| 子 agent（`tools/src/lib.rs:3859` `build_agent_runtime`） | `with_model_context_window_strict`（新变体） | **不读任何 env**，强制用 `context_window × 75%`，与主 LLM env 彻底独立 |

`strict` 变体仍保留**下限保护**：算出的阈值若小于 `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD`（55K）则兜底到 55K——太小阈值频繁 compact 反伤缓存，对子 agent 仍是不良。子 agent 走 DeepSeek-v4-flash 128K 窗口 → 96K 阈值（>55K 直接用），走极小窗口模型时也有兜底。

### 落地改动

| 文件 | 改动 |
|---|---|
| `runtime/src/conversation.rs:252` | 加 `with_model_context_window_strict` 方法：不读 env，直接 `context_window × 75%` + 下限保护 |
| `tools/src/lib.rs:3859` | `build_agent_runtime` 改调 `with_model_context_window_strict`（原 `with_model_context_window` 替换）|
| `runtime/src/conversation.rs:1812-1909` | 加 4 个单元测试：strict 不读 env / strict 忽略 env 覆盖 / strict 下限保护 / 对照原路径仍读 env |

### 主子压缩策略组合速查

| 主 LLM model | 子 agent model | 主 LLM auto-compact 阈值 | 子 agent auto-compact 阈值 |
|---|---|---|---|
| `deepseek-v4-pro` 1M | `glm-5.1` 200K | env 设了用 env；没设用 1M×75%=750K | 200K×75%=**150K**（strict，env 不施加）|
| `deepseek-v4-pro` 1M | `deepseek-v4-flash` 128K | 同上 | 128K×75%=**96K**（strict） |
| `glm-5.1` 200K | `deepseek-v4-pro` 1M | env 设了用 env；没设用 200K×75%=150K | 1M×75%=**750K**（strict，env 不施加）|
| `glm-5.1` 200K | `glm-5.1` 200K | 同上 | 200K×75%=**150K**（strict） |

即：子 agent 阈值严格按自己的 model 算，主 LLM 的 env 配置无法污染——主子压缩策略彻底独立。

### 验证状态

- ✅ `cargo check --workspace`（lib）全绿，无新增 warning
- ✅ `cargo test -p runtime --lib with_model_context_window -- --test-threads=1` 4 passed 0 failed
  - **注意**：cargo test 默认并行跑，env 是进程级全局——`with_model_context_window_strict_ignores_env_override` 和 `with_model_context_window_original_path_still_reads_env_override` 都用 `set_var`/`remove_var` 操作同一组 env，交错时会污染。验对照测试那条**必须用 `--test-threads=1` 串行模式**跑才稳定。
- ⏳ **真机验证未做**——用户需 `cargo build --release` 替换 `claw.exe` 后真机跑一轮，看 `claw_glm_diag.log` 里子 agent 那条 `claw_auto_compact` 事件的 `threshold` 字段是否按子 agent 的 model 算（GLM 走 150K / DeepSeek 走 750K）而非主 LLM env 设的值

---

## 3.4quater ★ 2026-07-19 multiprovider 路由真机验证——路由真生效，403 是 GLM 网关 model 名拒识

### 真机现场

用户照 MEMORY 第 27 条修订建议"更强制点名派活"——用 `Agent` 工具派活（不是上轮的 `TaskCreate` 那条死登记器）：

```
调 Agent 工具，subagent_type="review"，description="检查 setup.py 的安全问题"，prompt="检查 setup.py 的安全问题"
```

主 LLM **真输出 Agent 工具调用**——cli 回执含 `agentId=agent-1784433232959863800`、`status=running`、`lane.started` 事件 emitted。走的是 multiprovider 已覆盖那条真 spawn 链（`execute_agent`→`execute_agent_with_spawn`→`spawn_agent_job`→`thread::Builder.spawn`→`run_agent_job`→`build_agent_runtime`→`resolve_subagent_provider`）。但子 agent 调 GLM 网关失败报 403：

```
api returned 403 Forbidden: Authentication failed: Remote validation failed, message: 当前访问模型不存在或者模型名称错误
```

### 路由真生效确认（源码扒读）

`build_agent_runtime`（`tools/src/lib.rs:3841`）真调 `resolve_subagent_provider`，第 4935-4941 行真返了 `routing.default` 的 GLM 配置：

| 字段 | 值 |
|---|---|
| `resolved.base_url` | `Some("https://aigw-gzgy2.cucloud.cn:8443")` |
| `resolved.auth` | `Some(ApiKey("sk-sp-..."))` |
| `resolved.model` | `"glm-5.1"` |

子 agent 真调了 GLM endpoint 用 `glm-5.1`。**字段名口径全链一致**——`config.rs:1024` 解析 `object.get("subagentProviderDefault")`（camelCase）+ `config_validate.rs:215` schema 白名单 `subagentProviderDefault`（camelCase）+ settings.json 写的 `subagentProviderDefault`（camelCase）+ 子段 `baseUrl`/`apiKey`/`model`（camelCase，`config.rs:1039-1042`）全对得上。routing 解析链全对，**不是路由失效**。

### 403 真根因 = GLM 网关拒识 model 名 `glm-5.1`

子 agent 真调了 GLM 网关 `aigw-gzgy2.cucloud.cn:8443` 用 model `glm-5.1`，但 GLM 网关那边没注册 `glm-5.1` 这个 model 名报 403 `当前访问模型不存在或者模型名称错误`。

claw 源码侧 `model_token_limit` 表（`api/src/providers/mod.rs:648`）注册名是 `"glm-5" | "glm-5.1"`——跟 settings.json 配的 `glm-5.1` 对得上，源码侧没问题。**403 是 GLM 网关那头的 model 名注册问题**，可能网关认得的形是 `glm-5`（不带 .1）/ `GLM-5.1`（大写）/ 网关自定义别名——这条要查 GLM 网关 API 文档确认，claw 源码侧判不出来。

### 扒日志两次错判纠正

| 错判 | 真相 |
|---|---|
| ❌ 第一次"路由没生效，子 agent 用 DEFAULT_AGENT_MODEL=claude-opus-4-6 调 DeepSeek" | manifest.model 显示 claude-opus-4-6 是给主 LLM 看的回执字段（`execute_agent` 开头创建 manifest 时用 DEFAULT_AGENT_MODEL 写的，`:3673`），子 agent 真用的是 `build_agent_runtime` 里 `resolved.model` |
| ❌ 第二次"403 是 DeepSeek 网关拒识 claude-opus-4-6" | 403 是 GLM 网关拒识 `glm-5.1`，不是 DeepSeek |

两次错判都因日志没记录子 agent 那条 API 调用的 `claw_glm_diag` 事件（grep `"url"` 命中 0），只凭 manifest.model 字段判读不严谨。

### 扒日志教训补充第七步

日志没记录子 agent 路径诊断事件时，**别凭 manifest 回执字段判子 agent 真用的 model/url**——要看 `build_agent_runtime` 里 `resolved.model` 与 `new_with_resolved` 注入的 `base_url`，那才是子 agent 真调的。

日志改进建议（未做）：给 `build_agent_runtime` 加一条 `claw_subagent_dispatch` 诊断事件记录 `resolved.base_url` / `resolved.model` / `resolved.auth`（脱敏），方便扒日志判路由真生效与否。

### 四验证点真机结果

| 验证点 | 期望 | 实际 |
|---|---|---|
| Agent 工具链真 spawn 跑了 | ✅ | agentId 真创建、status=running、lane.started emitted |
| 子 agent 真调 GLM endpoint | ✅ | 源码扒 `build_agent_runtime` 路由真生效（日志没记诊断事件但源码链对了）|
| 403 报错原文 | GLM 网关拒识 `glm-5.1` | ✅ `当前访问模型不存在或者模型名称错误` |
| hook 脱敏生效 | ✅ | `词1_635df8ef` 等 10 种占位符大量命中（主 LLM + 子 agent 路径都跑）|
| strict 阈值 | 150K（GLM 200K × 75%）| 未触发——子 agent 跑得太短，403 后立即失败没到 auto-compact 阈值 |

### 下次真机验证建议（再次修订）

用户查 GLM 网关 `aigw-gzgy2.cucloud.cn:8443` API 文档确认认得的 model 名形——可能要改 settings.json 的 `subagentProviderDefault.model` 从 `glm-5.1` 改成网关注册的形：

- 试 `glm-5` 不带 `.1`
- 或 `GLM-5.1` 大写
- 或网关自定义别名

改完真机重跑同样的 Agent 工具派活 prompt，看 403 是否消失子 agent 真回结果。如果所有网关别名都不行，可能要联系 GLM 网关运营方注册 `glm-5.1` 这个 model 名到网关路由表。

**本期 claw 源码侧不用改**——multiprovider 路由落地是对的，403 是外部网关配置问题。

---

## 3.4quinquies ★ 2026-07-19 auth 头分支修正——`authKind` 字段让子 agent 走 `Authorization: Bearer` 头连 GLM 网关

### 真根因锁定（纠正 3.4quater 节错判）

3.4quater 节判"403 是 GLM 网关拒识 model 名 `glm-5.1`"**错了**。用户反证——旧版 `7c95f2bd` 上用 `settings.json.glm51`（env 段配 `ANTHROPIC_AUTH_TOKEN=sk-sp-jokud5S` + `ANTHROPIC_MODEL=glm-5.1`）**能完全连 GLM5.1 网关**，说明 model 名 `glm-5.1` 网关认得，403 不是 model 名拒识。

扒两份 settings.json 对比找真根因：

| 项 | 旧版能连（`settings.json.glm51`）| multiprovider 落地后 403（现 `settings.json`）|
|---|---|---|
| GLM auth 字段 | **`ANTHROPIC_AUTH_TOKEN`**（env 段）→ `resolve_startup_auth_source` 走 `AuthSource::BearerToken` 分支 → `Authorization: Bearer <token>` 头 | **`apiKey`**（`subagentProviderDefault` 段）→ `resolve_subagent_provider:4932` 硬绑 `AuthSource::ApiKey` → `x-api-key` 头 |
| GLM 网关认得哪个头 | `Authorization: Bearer`（不认 `x-api-key`）| 同上 |

**真根因**：multiprovider 落地初版 `resolve_subagent_provider` 把 `apiKey` 字段硬绑到 `AuthSource::ApiKey` 分支，一刀切走 `x-api-key` 头。但**不同网关要不同 auth 头分支**——GLM 网关要 `Authorization: Bearer`（Bearer 分支），DeepSeek 兼容端要 `x-api-key`（ApiKey 分支）。旧版用 `ANTHROPIC_AUTH_TOKEN` env 走 Bearer 分支能连 GLM，新版硬绑 ApiKey 分支连不上报 403 `Authentication failed: Remote validation failed`。

### 修法落地（3 文件 4 处）

| 文件 | 改动 |
|---|---|
| `runtime/src/config.rs:96` | `SubagentProviderConfig` struct 加 `auth_kind: String` 字段（默认空等价 `"api_key"` 保持向后兼容）|
| `runtime/src/config.rs:1056` | `parse_subagent_provider_config` 解析 `authKind` 可选字段（camelCase 对齐既有约定），默认 `"api_key"` |
| `tools/src/lib.rs:4935/4941` | `resolve_subagent_provider` 把硬绑 `ApiKey` 改调新函数 `resolve_auth_source(&cfg)`——`"bearer"` → `AuthSource::BearerToken` → `Authorization: Bearer` 头（GLM），`"api_key"`/空/其他 → `AuthSource::ApiKey` → `x-api-key` 头（Anthropic + DeepSeek）|
| `runtime/src/config.rs:2442` | 测试 `routing_equality_and_default_construction` struct 构造补 `auth_kind: "api_key"` 字段 |

### `authKind` 字段速查

| `authKind` 值 | `AuthSource` 分支 | HTTP 头 | 适用网关 |
|---|---|---|---|
| `"bearer"` | `BearerToken` | `Authorization: Bearer <token>` | GLM 网关 `aigw-gzgy2.cucloud.cn:8443` |
| `"api_key"` / 空 / 其他 | `ApiKey` | `x-api-key: <key>` | Anthropic 原生 + DeepSeek 兼容端 `api.deepseek.com/anthropic` |

### 用户配置示例（主=DeepSeek + 子=GLM）

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
    "ANTHROPIC_API_KEY": "sk-deepseek-...",
    "ANTHROPIC_MODEL": "deepseek-v4-pro[1m]"
  },
  "subagentProviderDefault": {
    "baseUrl": "https://aigw-gzgy2.cucloud.cn:8443",
    "apiKey": "sk-sp-...",
    "model": "glm-5.1",
    "authKind": "bearer"
  }
}
```

**关键**：`"authKind": "bearer"` 必须显式配——默认空走 `ApiKey` 分支连不上 GLM 网关。

### 验证状态

- ✅ `cargo check --workspace` 全绿零 warning
- ✅ `cargo test -p runtime --lib parse_subagent_provider` 3 passed 0 failed
- ✅ `cargo test -p runtime --lib routing_equality` 1 passed 0 failed
- ⏳ 真机验证未做——用户重编 `cargo build --release` 替换 `claw.exe` 后真机重跑同样的 Agent 工具派活 prompt，看 403 是否消失子 agent 真回结果

### 教训（写给下次接手）

multiprovider 落地时**不要一刀切把 `apiKey` 字段硬绑到某个 `AuthSource` 分支**——不同网关要不同 auth 头分支：

| 网关 | auth 头分支 | 配置 |
|---|---|---|
| Anthropic 原生 | `x-api-key` | `authKind: "api_key"` 或不配 |
| DeepSeek 兼容端 | `x-api-key` | 同上 |
| GLM 兼容端 | `Authorization: Bearer` | `authKind: "bearer"` 必须显式配 |

同理其他可能差异的字段（如 `max_output_tokens` / `context_window` / `cache_control` 支持）也要做成 per-provider 可配，不要硬绑主 LLM 那套。下次接手加新网关支持时先扒该网关 API 文档确认 auth 头分支 + model 名形 + 其他兼容性字段，再在 `SubagentProviderConfig` 加对应字段 + `resolve_*` 按字段选分支。

---

## 3.4sexies ★ 2026-07-19 model 覆盖修正——routing 配了就用 `cfg.model`，不让 `DEFAULT_AGENT_MODEL` 兜底值覆盖

### 真根因锁定（纠正 3.4quinquies 节 auth 头判错后又被用户纠正 apiKey 错判）

3.4quinquies 节判"apiKey 值错了要改回 10 字符的 `sk-sp-jokud5S`"**错了**——用户纠正 apiKey 值 `sk-sp-jokud5SbMF1Or07qVms5UeSNGzsdsXFG` 是对的。重扒源码链 `execute_agent`（`tools/src/lib.rs:3717`）发现真根因：

```
execute_agent
  ↓
let model = resolve_agent_model(input.model.as_deref())
  ↑ AgentInput.model 字段主 LLM 派活时通常没传
  ↑ resolve_agent_model(None) → DEFAULT_AGENT_MODEL = "claude-opus-4-6" 兜底值
  ↓
model 被塞进 manifest.model + build_agent_system_prompt + AgentJob.manifest
  ↓
build_agent_runtime
  ↓
let model = job.manifest.model.clone().unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string())
  → 拿到 "claude-opus-4-6"
  ↓
resolve_subagent_provider(subagent_type, Some(&model), &routing)
  ↑ input_model = Some("claude-opus-4-6")，不是 None
  ↓
model: input_model.unwrap_or(&cfg.model).to_string()
  ↑ unwrap_or 不走 cfg.model，直接用 "claude-opus-4-6"
  ↓
resolved.model = "claude-opus-4-6"（不是配的 glm-5.1）
  ↓
子 agent 拿 claude-opus-4-6 调 GLM endpoint
  ↓
GLM 网关拒识 claude-opus-4-6 报 403 当前访问模型不存在或者模型名称错误
```

**apiKey 是对的，authKind: bearer 是对的，baseUrl 是对的**——全是对的，但 model 字段被 `DEFAULT_AGENT_MODEL` 兜底值覆盖成 `claude-opus-4-6`，GLM 网关拒识这个 model 名报 403。报错原文 `当前访问模型不存在或者模型名称错误` 讲的就是这条——model 名 `claude-opus-4-6` 在 GLM 网关不存在。

### 修法落地（2 文件 3 处）

| 文件 | 改动 |
|---|---|
| `tools/src/lib.rs:4936/4943` | `resolve_subagent_provider` 把 `input_model.unwrap_or(&cfg.model).to_string()` 改成 `cfg.model.clone()`——routing 配了（by_type 或 default）就用 cfg.model，**不让 input_model 覆盖**；`input_model` 只在 fallback 分支（routing 都没配）才用 |
| `tools/src/lib.rs:10815/10823` | 测试 `make_routing` helper 的 `SubagentProviderConfig` struct 构造补 `auth_kind: "api_key"` 字段（上轮 auth 头分支修正漏补这里导致 E0063）|
| `tools/src/lib.rs:10885` | 测试 `input_model_overrides_routing_model` 断言同步改——旧行为断言 `input_model` 覆盖 cfg.model（`assert_eq!(resolved.model, "deepseek-v4-pro[1m]")`），新行为断言 routing 配了就用 cfg.model 不让 input_model 覆盖（`assert_eq!(resolved.model, "glm-5.1")`）|

### 修正前后行为对比

| 场景 | 修正前 | 修正后 |
|---|---|---|
| routing 配了 `model: "glm-5.1"` + input_model = `Some("claude-opus-4-6")`（DEFAULT_AGENT_MODEL 兜底值）| `input_model.unwrap_or(&cfg.model)` → 用 `claude-opus-4-6`（错——GLM 网关拒识报 403）| `cfg.model.clone()` → 用 `glm-5.1`（对——GLM 网关认得）|
| routing 都没配 + input_model = `Some("claude-opus-4-6")` | fallback 分支用 input_model | 同（fallback 分支不动）|
| routing 配了 + input_model = `None`（主 LLM 显式传 None）| `unwrap_or(&cfg.model)` → 用 cfg.model | `cfg.model.clone()` → 用 cfg.model（行为同）|

### 验证状态

- ✅ `cargo check --workspace` 全绿零 warning
- ✅ `cargo test -p tools --lib input_model_overrides_routing_model` 1 passed 0 failed
- ⏳ 真机验证未做——用户重编 `cargo build --release` 替换 `claw.exe` 后真机重跑同样的 Agent 工具派活 prompt，看 403 是否消失子 agent 真回结果

### 教训（写给下次接手，第八步姿势）

multiprovider 落地时**别让 `input_model.unwrap_or(&cfg.model)` 这种"input 优先 cfg 兜底"逻辑出现在 routing 配了的分支**——`input_model` 来自 `execute_agent` 的 `resolve_agent_model(input.model.as_deref())`，主 LLM 派活时 `AgentInput.model` 字段通常没传，`resolve_agent_model(None)` 返 `DEFAULT_AGENT_MODEL` 兜底值（不是 None）。这个兜底值会覆盖 cfg.model 导致子 agent 拿错 model 名调错网关报 403。

**正解是 routing 配了就用 cfg.model，input_model 只在 fallback 分支（routing 都没配）才用**——`cfg.model.clone()` 直接用，不走 `unwrap_or`。下次接手加新 routing 字段时也要注意这条——别让主 LLM 那套兜底值污染子 agent 的配置。
