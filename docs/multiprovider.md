# claw-code 多 Provider 方案：子 agent 与主 LLM 用不同云服务商

> 本文是**方案设计文档**，不动代码。目标：让 claw-code 的子 agent（`Agent`/`Skill`/`Task`/`Worker`）能用与主 LLM **不同云服务商**的模型，例如主 LLM 走 DeepSeek、子 agent 走联通云 GLM，或反过来。
>
> 最后更新：2026-07-17

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

新增 `subagent_providers` 配置段，按 `subagent_type` 各指定一份独立 provider 配置：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
    "ANTHROPIC_API_KEY": "sk-deepseek-...",
    "ANTHROPIC_MODEL": "deepseek-v4-pro[1m]"
  },
  "subagent_providers": {
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
  "subagent_provider_default": {
    "base_url": "https://aigw-gzgy2.cucloud.cn:8443",
    "api_key": "sk-glm-...",
    "model": "glm-5.1"
  }
}
```

**设计要点**：
- **按 `subagent_type` 路由**——对齐 `allowed_tools_for_subagent`（`tools/src/lib.rs:3849`）已经有的按 type 分工具的机制，provider 也按 type 分
- **`subagent_provider_default` 兜底**——没显式配的 type 走这份；都没配则 fallback 到主 LLM env（保持向后兼容）
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
subagent_providers: parse_optional_subagent_provider_routing(&merged_value)?,
```

`parse_optional_subagent_provider_routing` 实现：
- 遍历 `subagent_providers` 对象的每个 key（subagent_type）→ 建 `SubagentProviderConfig`
- `api_key` 字段调 `resolve_env_ref`（新增 helper，对齐 Reasonix）把 `${VAR}` 替换成 `std::env::var(VAR)`
- `subagent_provider_default` 同理解析到 `default`
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

用户配 `subagent_providers` 的 key 要用上面这些标准 type 名。

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
| `.claw.json` / `.claw/settings.json` | 用户新增 `subagent_providers` / `subagent_provider_default` 配置段 | 配置 |
| 测试 | `config.rs` 加 routing 解析测试；`tools/lib.rs` 加 resolver 路由测试（per-type / default / fallback-to-env 三分支） | ~100 行 |

**总量**：约 220 行新代码 + 100 行测试，集中在 config + tools 两个 crate，api 层零改动（钩子已留）。

---

## 五、验证计划

### 5.1 编译层
- `cargo check --workspace` 干净
- `cargo test -p runtime --lib config` 通过（routing 解析测试）
- `cargo test -p tools --lib build_agent` 通过（resolver 三分支测试）

### 5.2 单元测试三分支
1. `resolves_per_type_provider_when_configured`——配 `subagent_providers.review`，调 `resolve_subagent_provider("review", None, &routing)`，断言返回的 `base_url`/`auth` 是 GLM 那份
2. `falls_back_to_default_when_type_unconfigured`——只配 `subagent_provider_default`，调未配的 type，断言走 default
3. `falls_back_to_main_env_when_nothing_configured`——空 routing，断言返回 `base_url: None` + `auth: None`（调用方走原 env 路径）

### 5.3 真机验证（用户重编后）
- 主 LLM 设 DeepSeek，`subagent_providers.review` 设联通云 GLM
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
- **`subagent_type` 名漂移**——如果未来 `normalize_subagent_type` 改了输出（如 `"Explore"` 改小写），配置里的 key 对不上 → fallback 到 default 或 env，**降级不崩溃**。需要给配置加载加 warning：`subagent_providers` 有 key 不在标准 type 列表里时打 stderr 警告（不 fail）。
- **`api_key` env 引用失败**——`${GLM_API_KEY}` 但 env 未设 → `resolve_env_ref` 返回空字符串，`AuthSource::ApiKey("")` 会让 API 调用 401。需要在 `resolve_subagent_provider` 里检查：`api_key` 解析后为空就报错明确提示"env VAR 未设"。
- **DeepSeek 的 `cache_control` 字段在 GLM 子 agent 路径失效**——`ProviderRuntimeClient` 的 `cache_config: api::CacheConfig::from_env()`（`lib.rs:4757`）是全局的，子 agent 走 GLM 时 cache_control 仍会被注入但 GLM Ignored（无害，MEMORY 第 425-477 行已核实）。**不需要为子 agent 单独关 cache_control**。

### 7.2 回退
- 删 `subagent_providers` 配置段 → `SubagentProviderRouting::default()` → `resolve_subagent_provider` 返回 None → 走原 `ProviderRuntimeClient::new` 路径 → **完全回到现状**
- 改动集中在 `tools/src/lib.rs` 的 `build_agent_runtime`，回退只需还原那一处调用 + 删新增函数

---

## 八、对照 Reasonix 的设计差异总结

| 维度 | Reasonix | claw 本方案 |
|---|---|---|
| resolver 注入方式 | `TaskTool.resolveProvider` 字段（代码注入） | `subagent_providers` 配置段（配置注入）——**对齐 claw 的配置驱动风格** |
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
