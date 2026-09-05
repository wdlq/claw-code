use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, estimate_context_tokens, estimate_session_tokens, CompactionConfig,
    CompactionResult,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::session::{ContentBlock, ConversationMessage, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 55_000;
/// **2026-09-03 G5 熔断器**：连续无效 proactive auto-compact 的最大次数——超过即停手
/// （对齐 claude-code autoCompact.ts:70 `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`）。
const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: u32 = 3;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";
/// Percentage of context window to trigger auto-compact (e.g. 75 means 75%).
const AUTO_COMPACT_PCT_OVERRIDE_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE";
/// Context window size in tokens, used together with PCT_OVERRIDE.
const AUTO_COMPACT_WINDOW_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
/// **2026-09-03 G4 逃生口**：设为 "1" 时跳过 env min 封顶，恢复旧"env 显式替代"语义
/// （调试用，默认不设——正常路径下 env 覆盖只能提前、不能拖后）。
const AUTO_COMPACT_THRESHOLD_UNCAPPED_ENV_VAR: &str = "CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED";

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    MessageStop,
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// Minimal streaming API contract required by [`ConversationRuntime`].
pub trait ApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
///
/// `kind` carries structured error semantics (e.g. `OverSize400` for GLM 网关
/// 撝请求体过大的 400 Bad Request）透传到 `run_turn`，让它能据此降级处理
/// （auto-compact 后重试）而非直接退出。对应 `ApiError::Api.retryable` +
/// `suggested_action` 语义，但 `RuntimeError` 从 `ApiError::to_string()` 构造时
/// 原本的 retryable/kind 标记会丢失，所以这里独立保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
    kind: ErrorKind,
}

/// Structured error kind for `RuntimeError`.
///
/// `OverSize400` 标记 GLM 网关因请求体过大回 400 Bad Request 的语义——
/// 这种 400 不是请求格式错误而是上下文累积超网关硬上限，`run_turn` 捕获后
/// 应 auto-compact 再重试而非直接退出。其他 400（请求体格式错误）仍走
/// `Generic` 直接退出路径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorKind {
    /// 默认——不可降级的普通错误。
    #[default]
    Generic,
    /// GLM 网关因请求体过大回 400 Bad Request——可降级 auto-compact 后重试。
    OverSize400,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Generic,
        }
    }

    /// 构造带 `kind` 标记的 `RuntimeError`——透传 400 over_size 等结构化语义。
    #[must_use]
    pub fn with_kind(message: impl Into<String>, kind: ErrorKind) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }

    /// 返回结构化错误语义——`run_turn` 据此判定是否降级重试。
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// 返回是否为可降级的 GLM 400 over_size 错误。
    #[must_use]
    pub fn is_over_size_400(&self) -> bool {
        self.kind == ErrorKind::OverSize400
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed runtime turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
    /// **2026-07-30 subagent 自适应判活**：心跳回调，`run_turn` loop 每轮完成时调一次。
    /// 主线程（`spawn_agent_job`）注入 sender——收到心跳即知子 agent 还在干活，重置静默计时器；
    /// 静默超过 `STALE_SECS` 判"已挂"提前结束，不再死切 600s。`None` = 主 LLM 路径，不判活。
    heartbeat: Option<Box<dyn Fn() + Send + Sync>>,
    /// **2026-09-03 缓存命中率长效修复**：高缓存命中模式——`true` 时 microcompact 跳过
    /// 常规 snip/清空（只保留 emergency 清超大输出），auto-compact 走模型真实窗口的大阈值，
    /// 让网关自动前缀缓存持续累积。由 `with_cache_mode_for_model` 按 model 名一次性设定：
    /// 仅 `glm-5.1` 走 `false`（老压缩机制），其他模型（glm-5.2 / deepseek 系等）走 `true`。
    microcompact_high_cache_mode: bool,
    /// **2026-09-03 G5 熔断器**：proactive auto-compact 连续无效计数——连续 3 次
    /// 压不动/压后仍超阈值即停手（交给 over_size_400 reactive 兜底），
    /// compact 生效后清零。对齐 claude-code autoCompact.ts:67-70。
    auto_compact_consecutive_failures: u32,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            heartbeat: None,
            microcompact_high_cache_mode: false,
            auto_compact_consecutive_failures: 0,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    /// **2026-09-03 缓存命中率长效修复**：按**当前调度的 model 名**一次性设定缓存策略——
    /// 这是本修复的核心入口，主 lane（`main.rs::build_runtime_with_plugin_state`）与
    /// 子 agent lane（`tools/src/lib.rs::build_agent_runtime`）都必须调它，保证两条路径
    /// 按各自 model 名走同一套分支（对照 multiprovider 的 per-lane model 判定范式）。
    ///
    /// - `glm-5.1`（唯一例外）→ `microcompact_high_cache_mode = false`：维持"GLM-5.1 小窗口
    ///   激进压缩"老机制——microcompact 常规 snip/清空照跑，阈值可被 env 覆盖（原行为）。
    /// - 其他一切模型（glm-5.2 / glm 系其他 / deepseek 系 / claude 等）→ `true`：
    ///   **高缓存命中模式**——microcompact 只保留 emergency 清（≥500K 字符安全网），
    ///   历史字节完全稳定，网关自动前缀缓存可持续累积。
    ///
    /// 注意：本方法**只设 microcompact 模式位**，不动阈值；动态大阈值由调用方按
    /// `api::model_token_limit` 的结果走 `with_model_context_window`（主）/`_strict`（子）设定。
    /// 两者分开是因为主 lane 要保留 env 覆盖能力（GLM-5.1 场景），子 lane 用 strict 忽略 env。
    #[must_use]
    pub fn with_cache_mode_for_model(mut self, model: &str) -> Self {
        self.microcompact_high_cache_mode = !crate::micro_compact::is_glm51_cache_model(model);
        self
    }

    /// 高缓存命中模式位读取器（诊断/测试用）。
    #[must_use]
    pub fn microcompact_high_cache_mode(&self) -> bool {
        self.microcompact_high_cache_mode
    }

    /// 按**模型上下文窗口**动态算auto-compact阈值（二期-C1）。
    ///
    /// **2026-09-03 P3 修复**：env 未设时动态值从 `窗口×75%` 换成 claude-code
    /// `autocompact_threshold_formula`（窗口 − min(max_output, 20K 摘要预留) − 13K 缓冲，
    /// 下限 55K）——对 200K 级 LLM 触发在 167K（83.5%），比 75% 版晚触发，对前缀缓存更友好。
    ///
    /// **2026-09-03 G4 封顶**：env 覆盖从"替代"改为"min 封顶"——对齐 claude-code 语义，
    /// **只能提前、不能拖后**：
    /// - `CLAUDE_CODE_AUTO_COMPACT_WINDOW`：从"替代窗口"改为"封顶有效窗口"
    ///   （`effective_window = min(模型窗口, env值)`，再进公式）；
    /// - `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` / `CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE`：
    ///   env 算出的阈值与公式默认阈值取 `min`。
    ///
    /// 逃生口：`CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED=1` 跳过封顶恢复旧替代语义（调试用）。
    #[must_use]
    pub fn with_model_context_window(
        mut self,
        context_window_tokens: u32,
        max_output_tokens: u32,
    ) -> Self {
        // env WINDOW 是"封顶"而非"替代"：glm-5.1 时代旧值 131000 不会把 1M 窗口顶成 131K。
        let env_window = std::env::var(AUTO_COMPACT_WINDOW_ENV_VAR)
            .ok()
            .and_then(|value| {
                value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|window| *window > 0)
            });
        let effective_window = env_window.map_or(context_window_tokens, |window| {
            window.min(context_window_tokens)
        });
        let formula_threshold = autocompact_threshold_formula(effective_window, max_output_tokens);

        // env 显式阈值（原始值，解析语义与 auto_compaction_threshold_from_env 一致）
        let mut env_threshold: Option<u32> = None;
        if let Ok(value) = std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR) {
            if let Ok(tokens) = value.trim().parse::<u32>() {
                if tokens > 0 {
                    env_threshold = Some(tokens);
                }
            }
        }
        if env_threshold.is_none() {
            if let (Ok(pct_str), Ok(window_str)) = (
                std::env::var(AUTO_COMPACT_PCT_OVERRIDE_ENV_VAR),
                std::env::var(AUTO_COMPACT_WINDOW_ENV_VAR),
            ) {
                if let (Ok(pct), Ok(window)) = (
                    pct_str.trim().parse::<u32>(),
                    window_str.trim().parse::<u32>(),
                ) {
                    if pct > 0 && pct <= 100 && window > 0 {
                        env_threshold = Some(((window as u64 * pct as u64) / 100) as u32);
                    }
                }
            }
        }

        // 逃生口：恢复旧"env 显式替代"语义（调试用，默认关）。
        let uncapped = std::env::var(AUTO_COMPACT_THRESHOLD_UNCAPPED_ENV_VAR)
            .map(|value| value.trim() == "1")
            .unwrap_or(false);
        if uncapped {
            self.auto_compaction_input_tokens_threshold =
                env_threshold.unwrap_or(formula_threshold);
            return self;
        }

        // **G4 封顶**：env 只能提前（更小），不能拖后（更大）——min(env, 公式默认)。
        self.auto_compaction_input_tokens_threshold = env_threshold
            .map_or(formula_threshold, |threshold| {
                threshold.min(formula_threshold)
            });
        self
    }

    /// **2026-07-19 multiprovider 落地**：子 agent 专用——按 model 上下文窗口动态算
    /// auto-compact 阈值，**不读任何 env 覆盖**。
    ///
    /// **2026-09-03 P3 修复**：阈值算法从 `(窗口−max_output)×75%` 换成
    /// `autocompact_threshold_formula`（窗口 − min(max_output, 20K) − 13K 缓冲，
    /// 下限 55K）——数值普遍比 75% 版晚触发，对前缀缓存更友好；
    /// 高缓存模式（不读 env）语义不变（§6 不许动清单）。
    ///
    /// 修的破裂点：主 LLM 走 DeepSeek 1M，子 agent 走 GLM 200K 时，若 `.claw.json`
    /// 的 `env` 段显式设了 `CLAUDE_CODE_AUTO_COMPACT_WINDOW=131000`（针对 GLM 200K 算的 75%），
    /// 原路径会把这套全局 env 误施加到子 agent 上——主=DeepSeek 时子 agent 阈值被压到 131K
    /// 频繁 compact 反伤 DeepSeek 缓存；主=GLM 子=DeepSeek 时子 agent 阈值 750K 直接撑爆
    /// GLM 200K 窗口报 `ContextWindowExceeded` 400。
    ///
    /// 子 agent 走 strict 路径后阈值严格按自己的 model 算，与主 LLM 的 env 配置彻底独立。
    /// 仍保留下限保护（公式内置 55K 下限），避免子 agent 走小窗口模型算出太小阈值
    /// 频繁 compact 反伤缓存。
    ///
    /// 对照 `docs/multiprovider.md` 3.4ter 节。主 LLM 路径仍走 `with_model_context_window`
    /// （允许用户用 env 显式覆盖主 LLM 阈值），两条路径彻底独立。
    #[must_use]
    pub fn with_model_context_window_strict(
        mut self,
        context_window_tokens: u32,
        max_output_tokens: u32,
    ) -> Self {
        self.auto_compaction_input_tokens_threshold =
            autocompact_threshold_formula(context_window_tokens, max_output_tokens);
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    /// **2026-07-30 subagent 自适应判活**：注入心跳回调，`run_turn` loop 每轮完成时调一次。
    /// 子 agent 路径用此让主线程判活——收到心跳即"还在干活"重置静默计时器，
    /// 静默超阈值判"已挂"提前结束。主 LLM 路径不注入（`None`），`run_turn` 调空回调即跳过。
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Box<dyn Fn() + Send + Sync>) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    /// Run a session health probe to verify the runtime is functional after compaction.
    /// Returns Ok(()) if healthy, Err if the session appears broken.
    fn run_session_health_probe(&mut self) -> Result<(), String> {
        // Check if we have basic session integrity
        if self.session.messages.is_empty() && self.session.compaction.is_some() {
            // Freshly compacted with no messages - this is normal
            return Ok(());
        }

        // Verify tool executor is responsive with a non-destructive probe
        // Using glob_search with a pattern that won't match anything
        let probe_input = r#"{"pattern": "*.health-check-probe-"}"#;
        match self.tool_executor.execute("glob_search", probe_input) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Tool executor probe failed: {e}")),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();

        // ROADMAP #38: Session-health canary - probe if context was compacted
        if self.session.compaction.is_some() {
            if let Err(error) = self.run_session_health_probe() {
                return Err(RuntimeError::new(format!(
                    "Session health probe failed after compaction: {error}. \
                     The session may be in an inconsistent state. \
                     Consider starting a fresh session with /session new."
                )));
            }
        }

        self.record_turn_started(&user_input);
        self.session
            .push_user_text(user_input)
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        // **2026-07-22 子 agent 撝爆 GLM 修复**：over_size 400 降级重试计数——
        // 每次降级 auto-compact 后重试本轮，但最多 3 次避免无限循环（compact 后仍撝 400 说明压不动）。
        let mut over_size_400_retries: u32 = 0;
        const OVER_SIZE_400_MAX_RETRIES: u32 = 3;
        // **2026-09-03 P2 修复**：turn 内请求前那次 compact 的 event——TurnSummary
        // 沿用原字段（CLI 渲染 / jsonl 兼容），但来源从"turn 后独立触发点"改为请求前。
        let mut pre_turn_auto_compaction: Option<AutoCompactionEvent> = None;

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                // **2026-07-23 子 agent 超限 graceful 退出**：
                // 之前此处直接 return Err，导致子 agent 已收集的所有文本全丢失，
                // 主 LLM 只看到 "conversation loop exceeded..." 错误字符串。
                // 现在改为 break——带着已累积的 assistant_messages 走下方正常返回路径，
                // 让主 LLM 能看到子 agent 已完成的部分工作。
                eprintln!(
                    "[conversation: 达到最大迭代数 {}，带已有结果退出]",
                    self.max_iterations
                );
                break;
            }

            // Check if the user requested an abort (e.g. via CTRL+C).
            if self.hook_abort_signal.is_aborted() {
                let error = RuntimeError::new("Turn aborted by user");
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            // Micro-compact: clear old tool-result content before building the
            // API request.  This is the primary defense against oversized
            // request bodies — tool results (file reads, grep output, …)
            // are replaced with a placeholder once they're old enough that
            // the model no longer needs the verbatim content.
            let mc_result = crate::micro_compact::microcompact_session(
                &mut self.session,
                self.auto_compaction_input_tokens_threshold,
                self.microcompact_high_cache_mode,
            );
            if mc_result.cleared_count > 0 {
                eprintln!(
                    "[micro-compact: cleared {} old tool result(s), freed {} chars]",
                    mc_result.cleared_count, mc_result.chars_freed
                );
            }

            // Pre-flight auto-compact（**唯一的 proactive 触发点**——2026-09-03 P2）：
            // if the session is still large after micro-compact, do a full
            // auto-compact. This prevents 400 errors from providers (e.g. GLM)
            // that reject oversized requests.
            // Step 6（G6）占位：snip 联动稳定后把 mc_result.chars_freed 折算传进来。
            if let Some(event) = self.auto_compact_if_needed(0) {
                pre_turn_auto_compaction = Some(event);
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    // **2026-07-22 子 agent 撝爆 GLM 修复**：GLM 网关因请求体过大回 400 Bad Request 时，
                    // 降级 auto-compact 后重试本轮而非直接退出。`over_size_400` 语义由 `ProviderRuntimeClient::stream`
                    // 从 `ApiError::is_over_size_400()` 透传到 `RuntimeError::with_kind(..., OverSize400)`。
                    // 其他错误（含格式错误的 400）仍走原 `return Err` 直接退出路径。
                    if error.is_over_size_400() {
                        if over_size_400_retries >= OVER_SIZE_400_MAX_RETRIES {
                            // compact 后仍撝 400 说明压不动——放弃降级，上抛让调用方知道。
                            eprintln!(
                                "[over_size_400: exhausted {} 降级重试，放弃]",
                                OVER_SIZE_400_MAX_RETRIES
                            );
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
                        over_size_400_retries += 1;
                        eprintln!(
                            "[over_size_400: GLM 网关撝请求体过大回 400，降级 auto-compact 后重试 ({}/{})]",
                            over_size_400_retries,
                            OVER_SIZE_400_MAX_RETRIES
                        );
                        // 强制 auto-compact：用 `compact_session` 压到尽量小（max_estimated_tokens=0）
                        // 再 `continue` 重试本轮。`auto_compact_if_needed` 轻量路径阈值不够低时撝不住，
                        // 这里直接调 `compact_session` 强压。
                        let compact_result = compact_session(
                            &self.session,
                            CompactionConfig {
                                max_estimated_tokens: 0,
                                ..CompactionConfig::default()
                            },
                        );
                        if compact_result.removed_message_count > 0 {
                            self.session = compact_result.compacted_session;
                            eprintln!(
                                "[over_size_400: auto-compact 移除 {} 条消息]",
                                compact_result.removed_message_count
                            );
                        } else {
                            // compact 压不动（消息全保留或全空）——再试也是 400，提前放弃避免空转。
                            eprintln!("[over_size_400: auto-compact 压不动，放弃降级重试]");
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
                        // 本轮迭代不计入 max_iterations——降级重试不应被错误地算成"超额迭代"。
                        iterations -= 1;
                        continue;
                    }
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            };
            let (assistant_message, usage, turn_prompt_cache_events) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);
            // **2026-07-30 subagent 自适应判活**：本轮成功拿到 assistant 消息即证明子 agent 还在干活，
            // 发心跳让主线程重置静默计时器。主 LLM 路径 heartbeat=None 跳过（if let Some 解构空 Option）。
            if let Some(heartbeat) = self.heartbeat.as_ref() {
                heartbeat();
            }
            let pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );

            self.session
                .push_message(assistant_message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                break;
            }

            for (tool_use_id, tool_name, input) in pending_tool_uses {
                let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
                let effective_input = pre_hook_result
                    .updated_input()
                    .map_or_else(|| input.clone(), ToOwned::to_owned);
                let permission_context = PermissionContext::new(
                    pre_hook_result.permission_override(),
                    pre_hook_result.permission_reason().map(ToOwned::to_owned),
                );

                let permission_outcome = if pre_hook_result.is_cancelled() {
                    // Hook was cancelled (e.g. user pressed CTRL+C).
                    // Abort the entire turn immediately instead of denying
                    // the tool and letting the model retry in an infinite loop.
                    let error = RuntimeError::new(format!(
                        "Turn aborted: PreToolUse hook cancelled for tool `{tool_name}`"
                    ));
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                } else if pre_hook_result.is_failed() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook failed for tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_denied() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook denied tool `{tool_name}`"),
                        ),
                    }
                } else if let Some(prompt) = prompter.as_mut() {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        Some(*prompt),
                    )
                } else {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        None,
                    )
                };

                let result_message = match permission_outcome {
                    PermissionOutcome::Allow => {
                        self.record_tool_started(iterations, &tool_name);
                        let (mut output, mut is_error) =
                            match self.tool_executor.execute(&tool_name, &effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            };
                        output = merge_hook_feedback(pre_hook_result.messages(), output, false);

                        let post_hook_result = if is_error {
                            self.run_post_tool_use_failure_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                            )
                        } else {
                            self.run_post_tool_use_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                                false,
                            )
                        };
                        // If hook returns updated_tool_output, replace the tool output
                        if let Some(updated_output) = post_hook_result.updated_tool_output() {
                            output = updated_output.to_string();
                        }
                        if post_hook_result.is_denied()
                            || post_hook_result.is_failed()
                            || post_hook_result.is_cancelled()
                        {
                            is_error = true;
                        }
                        output = merge_hook_feedback(
                            post_hook_result.messages(),
                            output,
                            post_hook_result.is_denied()
                                || post_hook_result.is_failed()
                                || post_hook_result.is_cancelled(),
                        );

                        ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error)
                    }
                    PermissionOutcome::Deny { reason } => ConversationMessage::tool_result(
                        tool_use_id,
                        tool_name,
                        merge_hook_feedback(pre_hook_result.messages(), reason, true),
                        true,
                    ),
                };
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                tool_results.push(result_message);
                // **2026-07-30 subagent 自适应判活**：工具执行（grep/read_file 等）本身可能耗时数十秒，
                // 主线程需要在工具期间也收到心跳才能判"还在干活"而非误判静默超时。每个工具完成发一次。
                if let Some(heartbeat) = self.heartbeat.as_ref() {
                    heartbeat();
                }
            }
        }

        // **2026-09-03 P2 修复**：turn 后的独立 auto-compact 触发点已删——触发收敛到
        // 请求前单点（auto_compact_if_needed）。TurnSummary.auto_compaction 字段保留，
        // 记录本轮请求前那次 compact 的 event（CLI 渲染与 jsonl 兼容不变）。
        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction: pre_turn_auto_compaction,
        };
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session(&self.session, config)
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    /// **2026-09-03 P2 修复**：全 runtime 唯一的 proactive auto-compact 触发点——
    /// 只在"发请求前"（run_turn loop 的 pre-flight 位置）调用。turn 结束后的独立
    /// 触发点已删：两套触发点估算口径不一致，turn 后触发多一次前缀击穿风险
    /// （对齐 claude-code query.ts:453 单触发点）。
    ///
    /// `snip_tokens_freed`：**Step 6（G6）占位参数**——microcompact/snip 本轮释放的量
    /// 折算 tokens 后从触发估算中扣除（对齐 claude-code `snipTokensFreed`）。
    /// 当前调用方传 0；等 Step 1-5 真机验证稳定后再接 `microcompact_session`
    /// 的 `chars_freed` 折算（除以 CJK 估算比率 ~3）。
    fn auto_compact_if_needed(&mut self, snip_tokens_freed: usize) -> Option<AutoCompactionEvent> {
        // **2026-09-03 G5 熔断器**（对齐 claude-code autoCompact.ts:67-70
        // `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES=3`——Anthropic 实测"失败后连续重试
        // 每天 250K 次 API 调用"的教训）：连续 3 次 proactive compact 无效
        // （压不动 / 压后仍超阈值）即停手，交给 over_size_400 reactive 降级路径兜底；
        // compact 生效（removed>0 且压后低于阈值）后计数清零恢复尝试。
        if self.auto_compact_consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES {
            eprintln!(
                "[auto-compact: ineffective ×{}/{} — 熔断停手，交给 over_size_400 reactive 兜底]",
                self.auto_compact_consecutive_failures, MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
            );
            return None;
        }

        // **2026-09-03 P1 修复**：触发判断改用回执锚点估算（对齐 claude-code
        // tokenCountWithEstimation）。此前读 `cumulative_usage().input_tokens`——
        // cumulative 是跨 turn 累加的计费/统计值，只增不减、不是当前上下文大小：
        // 一旦累计破阈值，之后每轮都满足条件，会 2-3 轮压一次停不下来（3 小时 31 次
        // auto_compact 的日志铁证）。锚点 = 最近一条带真实 usage 的 assistant 消息回执
        // 全量 + 尾部粗估；找不到锚点走全量粗估兜底（含系统开销 15K）。
        let estimated_tokens =
            estimate_context_tokens(&self.session).saturating_sub(snip_tokens_freed);
        if estimated_tokens < self.auto_compaction_input_tokens_threshold as usize {
            return None;
        }

        let result = compact_session(
            &self.session,
            CompactionConfig {
                max_estimated_tokens: 0,
                ..CompactionConfig::default()
            },
        );

        if result.removed_message_count == 0 {
            // 压不动（可压缩消息不足 preserve_recent 等）——计一次无效。
            self.auto_compact_consecutive_failures += 1;
            eprintln!(
                "[auto-compact: ineffective ×{}/{} — compact 压不动]",
                self.auto_compact_consecutive_failures, MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
            );
            return None;
        }

        self.session = result.compacted_session;
        let event = AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        };

        if estimate_context_tokens(&self.session)
            >= self.auto_compaction_input_tokens_threshold as usize
        {
            // 压了但没压到阈值下——计无效但不吞事件：compact 确实发生，
            // TurnSummary/diag 如实记录；连续 3 次后熔断。
            self.auto_compact_consecutive_failures += 1;
            eprintln!(
                "[auto-compact: ineffective ×{}/{} — 压后仍超阈值]",
                self.auto_compact_consecutive_failures, MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
            );
        } else {
            // compact 生效（removed>0 且压后低于阈值）——清零恢复尝试。
            self.auto_compact_consecutive_failures = 0;
        }

        // diag 事件发射随触发点收敛——只有真正 compact 时才发（原 turn 后触发点已删）。
        eprintln!(
            "[auto-compacted: removed {} messages]",
            event.removed_message_count
        );
        write_auto_compact_diag(
            event.removed_message_count,
            self.auto_compaction_input_tokens_threshold,
        );
        Some(event)
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }
}

/// 对齐 claude-code `getAutoCompactThreshold`（autoCompact.ts:72-91）的阈值公式
/// （**2026-09-03 P3 修复**）：
///
/// ```text
/// 有效窗口预留 = min(max_output, 20K 摘要预留)   // MAX_OUTPUT_TOKENS_FOR_SUMMARY
/// 触发阈值     = 窗口 − 预留 − 13K 缓冲          // AUTOCOMPACT_BUFFER_TOKENS
/// 下限保护     = 55K                              // 防小窗口频繁 compact 反伤缓存
/// ```
///
/// 200K/64K → 167K（83.5%）；1M/64K → 967K；200K/8K → 179K（max_output 低于 20K 按实际预留）。
/// `max_output` 高于 20K 时只预留 20K——摘要调用本身用不了那么多输出。
#[must_use]
pub fn autocompact_threshold_formula(context_window_tokens: u32, max_output_tokens: u32) -> u32 {
    const MAX_OUTPUT_TOKENS_FOR_SUMMARY: u32 = 20_000;
    const AUTOCOMPACT_BUFFER_TOKENS: u32 = 13_000;
    let reserve = max_output_tokens.min(MAX_OUTPUT_TOKENS_FOR_SUMMARY);
    let threshold = context_window_tokens
        .saturating_sub(reserve)
        .saturating_sub(AUTOCOMPACT_BUFFER_TOKENS);
    threshold.max(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

/// Reads the automatic compaction threshold from the environment.
/// Supports three formats:
/// 1. CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS (direct token count)
/// 2. CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE + CLAUDE_CODE_AUTO_COMPACT_WINDOW (percentage of context window)
/// 3. Default: 55,000 tokens
///
/// **2026-09-03 G4**：本函数只提供 `new()` 时的**兜底初值**（无窗口信息时的语义）。
/// 主 lane 随后调 `with_model_context_window(窗口, max_output)`，按"env 只能提前
/// 不能拖后"的 min 封顶语义重算（WINDOW 是封顶有效窗口、显式阈值取 min）。
/// 逃生口 `CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED=1` 只在该 builder 生效。
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    // First check for direct token threshold
    if let Ok(threshold) = std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR) {
        if let Ok(tokens) = threshold.trim().parse::<u32>() {
            if tokens > 0 {
                return tokens;
            }
        }
    }

    // Check for PCT_OVERRIDE + WINDOW combination
    if let (Ok(pct_str), Ok(window_str)) = (
        std::env::var(AUTO_COMPACT_PCT_OVERRIDE_ENV_VAR),
        std::env::var(AUTO_COMPACT_WINDOW_ENV_VAR),
    ) {
        if let (Ok(pct), Ok(window)) = (
            pct_str.trim().parse::<u32>(),
            window_str.trim().parse::<u32>(),
        ) {
            if pct > 0 && pct <= 100 && window > 0 {
                // Calculate threshold as percentage of context window
                return (window as u64 * pct as u64 / 100) as u32;
            }
        }
    }

    DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::Thinking {
                thinking,
                signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AssistantEvent::TextDelta(delta) => text.push_str(&delta),
            AssistantEvent::ToolUse { id, name, input } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    flush_text_block(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
    ))
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError>>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl FnMut(&str) -> Result<String, ToolError> + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        autocompact_threshold_formula, build_assistant_message, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
        StaticToolExecutor, ToolExecutor, DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::session::{ContentBlock, MessageRole, Session};
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[test]
    fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter))
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None)
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None)
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[test]
    fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime.run_turn("a", None).expect("turn a");
        runtime.run_turn("b", None).expect("turn b");
        runtime.run_turn("c", None).expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[test]
    fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("persist this turn", None)
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[test]
    fn auto_compacts_when_receipt_anchor_exceeds_threshold() {
        // **2026-09-03 P1+P2 修订**：触发判断 = 最近一条真实回执锚点（input+output+cache，
        // 不再读 cumulative 跨 turn 累加值）；触发点 = 请求前 pre-flight（turn 后独立
        // 触发点已删）。本轮跨阈值 → 本轮 summary.auto_compaction = None，下一轮
        // 请求前才 compact 并记录到那一轮的 TurnSummary。
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let first = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(
            first.auto_compaction, None,
            "turn 后触发点已删——跨阈值留到下一轮请求前处理"
        );

        let second = runtime
            .run_turn("continue", None)
            .expect("follow-up turn should succeed");
        let event = second
            .auto_compaction
            .expect("next turn pre-flight should compact once the anchor exceeds threshold");
        assert!(event.removed_message_count > 0);
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[test]
    fn skips_auto_compaction_below_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        // **2026-09-03 修订**：锚点语义下回执即"当前上下文总量"
                        // （input+output），50_004 < 100_000 阈值 → 不触发 compact。
                        input_tokens: 50_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn auto_compaction_threshold_defaults_and_parses_values() {
        // #186: parse_auto_compaction_threshold was removed from the runtime
        // API surface in an earlier refactor but its test + import were left
        // behind, blocking runtime lib test compilation on all platforms.
        // Rather than orphan the test we repoint it at the still-defined
        // const + a tiny inline parse to preserve coverage of the default
        // threshold semantics.
        fn inline_parse(value: Option<&str>) -> u32 {
            match value.and_then(|s| s.parse::<u32>().ok()) {
                Some(n) if n > 0 => n,
                _ => DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
            }
        }
        assert_eq!(
            inline_parse(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(inline_parse(Some("4321")), 4321);
        assert_eq!(
            inline_parse(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            inline_parse(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    /// env 是进程级全局——`set_var`/`remove_var` 同一组 `CLAUDE_CODE_AUTO_COMPACT_*`
    /// 的测试必须互斥，否则并行跑时互相污染（MEMORY 第 25 条坑；
    /// 对齐 api crate cache_control 测试的 `static Mutex` 串行化范式）。
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 构造一个最小可用的 ConversationRuntime 实例供 builder 测试用——
    /// 不跑 turn，只验 builder 设的字段值。对照 `auto_compacts_when_cumulative_input_threshold_is_crossed` 风格。
    /// ApiClient 用最简的 `SimpleApiForBuilder`（返回空流即可——builder 测试不会真调 stream）。
    struct SimpleApiForBuilder;
    impl ApiClient for SimpleApiForBuilder {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![AssistantEvent::MessageStop])
        }
    }
    fn minimal_runtime() -> ConversationRuntime<SimpleApiForBuilder, StaticToolExecutor> {
        ConversationRuntime::new(
            Session::new(),
            SimpleApiForBuilder,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
    }

    /// ★ 2026-07-19 multiprovider：strict 变体不读任何 env，强制用
    /// `autocompact_threshold_formula`。验 DeepSeek V4 Pro 1M 窗口 + max_output=0 → 987K 阈值。
    #[test]
    fn with_model_context_window_strict_uses_dynamic_threshold_without_env() {
        let runtime = minimal_runtime().with_model_context_window_strict(1_000_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 987_000,
            "1M 窗口 − min(0, 20K) − 13K = 987K，不读 env"
        );
    }

    /// **2026-09-03 P3**：`autocompact_threshold_formula` 公式数值矩阵（对齐 claude-code
    /// getAutoCompactThreshold）——20K 摘要预留封顶 + 13K 缓冲 + 55K 下限。
    #[test]
    fn autocompact_threshold_formula_matches_claude_code_matrix() {
        // 200K/64K → 167K（min(64K, 20K)=20K 预留封顶）
        assert_eq!(autocompact_threshold_formula(200_000, 64_000), 167_000);
        // 200K/128K → 167K（max_output 高于 20K 时仍只预留 20K）
        assert_eq!(autocompact_threshold_formula(200_000, 128_000), 167_000);
        // 1M/64K → 967K
        assert_eq!(autocompact_threshold_formula(1_000_000, 64_000), 967_000);
        // 128K/8K → 107K（max_output=8K 低于 20K，按实际预留）
        assert_eq!(autocompact_threshold_formula(128_000, 8_000), 107_000);
        // 下限保护：60K 窗口算出 47K < 55K → 兜底 55K
        assert_eq!(
            autocompact_threshold_formula(60_000, 0),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    /// **2026-09-03 G5 熔断器**：连续 3 次压不动后第 4 次不再尝试（计数停在 3、
    /// 会话未被压缩），交给 over_size_400 reactive 路径兜底。
    #[test]
    fn auto_compact_circuit_breaker_stops_after_3_failures() {
        // 4 条大 user 消息：fallback 估算（50_004 + 15K 系统开销）≥ 60K 阈值 → 触发条件满足；
        // 但 compactable.len() = 4 ≤ preserve_recent(4) → compact_session 压不动（removed=0）。
        let mut session = Session::new();
        let big = "x".repeat(50_000);
        for _ in 0..4 {
            session
                .push_user_text(big.clone())
                .expect("message should append");
        }
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApiForBuilder,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(60_000);

        for _ in 0..3 {
            assert!(
                runtime.auto_compact_if_needed(0).is_none(),
                "压不动 → None 且计一次无效"
            );
        }
        assert_eq!(runtime.auto_compact_consecutive_failures, 3);

        // 第 4 次：熔断开路——不再尝试。若仍尝试会再计一次无效（计数到 4），
        // 计数停在 3 即熔断生效的证据。
        assert!(runtime.auto_compact_if_needed(0).is_none());
        assert_eq!(
            runtime.auto_compact_consecutive_failures, 3,
            "熔断后不再尝试 compact，计数不再增长"
        );
        assert_eq!(runtime.session().messages.len(), 4, "会话未被压缩");
    }

    /// **2026-09-03 G5 熔断器**：连续无效（压不动）计数到 2 后，成功 compact 一次
    /// （removed>0 且压后低于阈值）→ 计数清零，恢复后续尝试。
    /// （熔断开路状态本身由上一条测试覆盖——3 次失败后停手，成功路径只能在
    /// 计数 <3 时发生，这也是"连续失败"语义的本意。）
    #[test]
    fn auto_compact_circuit_breaker_resets_on_success() {
        // 先制造 2 次压不动（4 条大消息：触发条件满足但 preserve_recent(4) 挡住压缩）
        let mut session = Session::new();
        let big = "x".repeat(50_000);
        for _ in 0..4 {
            session
                .push_user_text(big.clone())
                .expect("message should append");
        }
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApiForBuilder,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(60_000);
        for _ in 0..2 {
            runtime.auto_compact_if_needed(0);
        }
        assert_eq!(runtime.auto_compact_consecutive_failures, 2);

        // 换成"可压且压后低于阈值"的会话：50 条 6000 字符消息
        // （fallback 估算 ≈ 50×1501 + 15K 开销 ≈ 90K ≥ 60K 阈值触发），
        // compact 保留最近 4 条 → removed>0；压后（摘要 + 4 条 + 开销）远低于 60K → 成功。
        let mut compactable_session = Session::new();
        let padded = format!("m{} ", "y".repeat(5990));
        for i in 0..50 {
            compactable_session
                .push_user_text(format!("{padded}{i}"))
                .expect("message should append");
        }
        *runtime.session_mut() = compactable_session;

        let event = runtime
            .auto_compact_if_needed(0)
            .expect("可压会话应触发 compact");
        assert!(event.removed_message_count > 0);
        assert_eq!(
            runtime.auto_compact_consecutive_failures, 0,
            "compact 生效后计数清零恢复尝试"
        );

        // 清零后熔断器不再开路——空会话估算 < 阈值 → 不触发也不计数。
        *runtime.session_mut() = Session::new();
        assert!(runtime.auto_compact_if_needed(0).is_none());
        assert_eq!(runtime.auto_compact_consecutive_failures, 0);
    }

    /// ★ 2026-07-22：strict 变体验 GLM-5.1 场景——200K 窗口。
    /// **2026-09-03 P3 换公式**：(200K − min(64K, 20K 摘要预留) − 13K 缓冲) = 167K（83.5%），
    /// 对齐 claude-code getAutoCompactThreshold。200K 级 LLM 触发在 167K，
    /// 比 75% 版（102K）晚 65K，减少 compact 次数保护前缀缓存；
    /// 输入余量 33K（200K − 167K）> 0，不会撑爆窗口（over_size 400 由 reactive 路径兜底）。
    #[test]
    fn with_model_context_window_strict_glm51_scenario() {
        // GLM-5.1: context=200K, effective max_tokens=64K
        let runtime = minimal_runtime().with_model_context_window_strict(200_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 167_000,
            "200K − min(64K, 20K) − 13K = 167K，对齐 claude-code 公式"
        );
    }

    /// ★ 2026-09-03 缓存命中率长效修复：`with_cache_mode_for_model` 按**当前调度的 model 名**
    /// 分缓存模式——仅 glm-5.1（含大小写/日期后缀变体）维持老压缩机制
    /// （`microcompact_high_cache_mode = false`），其他一切模型走高缓存命中模式（`true`）。
    #[test]
    fn with_cache_mode_for_model_only_glm51_keeps_legacy_compaction() {
        assert!(
            !minimal_runtime()
                .with_cache_mode_for_model("glm-5.1")
                .microcompact_high_cache_mode(),
            "glm-5.1 必须维持老激进压缩机制"
        );
        assert!(
            !minimal_runtime()
                .with_cache_mode_for_model("GLM-5.1-0731")
                .microcompact_high_cache_mode(),
            "glm-5.1 日期后缀变体仍走老机制"
        );
        for model in [
            "glm-5.2",
            "GLM-5.2",
            "glm-5",
            "deepseek-v4-pro",
            "DeepSeek-V4-Flash-0731",
            "claude-opus-4-6",
        ] {
            assert!(
                minimal_runtime()
                    .with_cache_mode_for_model(model)
                    .microcompact_high_cache_mode(),
                "{model} 必须走高缓存命中模式"
            );
        }
    }

    /// ★ 2026-07-19 multiprovider：strict 变体**不读 env 覆盖**——
    /// 即使主 LLM 那套全局 env 显式设了阈值，子 agent 路径也不被污染。
    /// 这是 strict 变体与原 `with_model_context_window` 的核心差异点。
    #[test]
    fn with_model_context_window_strict_ignores_env_override() {
        let _env_guard = lock_env();
        // 模拟主 LLM 在 .claw.json env 段设的全局阈值——这套是针对主 LLM 算的，
        // 不该施加到子 agent 上（主=DeepSeek 1M 时这套 env 设 131K 是给 GLM 200K 用的，
        // 强加到子 agent=DeepSeek 1M 会被压到 131K 频繁 compact 反伤缓存）。
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "131000");
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE", "75");
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "131000");

        let runtime = minimal_runtime().with_model_context_window_strict(1_000_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 987_000,
            "strict 路径必须忽略 env 覆盖，按 1M − 0 − 13K = 987K 算"
        );

        // 清理 env 防止污染后续测试
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE");
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
    }

    /// ★ 2026-07-19 multiprovider：strict 变体保留下限保护——
    /// 子 agent 走小窗口模型（假设 128K 窗口）算出 115K 阈值，
    /// 仍被 `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD` 55K 下限保护兜底。
    /// 太小阈值频繁 compact 反伤缓存，对子 agent 仍是不良。
    #[test]
    fn with_model_context_window_strict_keeps_floor_protection() {
        // (128K − 0 − 13K) = 115K > 55K 下限，直接用 115K
        let runtime = minimal_runtime().with_model_context_window_strict(128_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 115_000,
            "128K − 0 − 13K = 115K > 55K 下限，用 115K"
        );

        // (50K − 0 − 13K) = 37K < 55K 下限，兜底到 55K
        let runtime = minimal_runtime().with_model_context_window_strict(50_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold,
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
            "50K − 0 − 13K = 37K < 55K 下限，兜底到 55K"
        );
    }

    /// ★ 2026-07-19 multiprovider + **2026-09-03 G4 封顶修订**：原路径 env 覆盖从
    /// "替代"改为"min 封顶"——INPUT_TOKENS 比公式默认小 → 提前（用户意图保留）；
    /// 比公式默认大 → 被公式值封顶（旧语义会让 1M 窗口被 131K 旧值顶回去，频繁 compact）。
    ///
    /// **注意**：cargo test 默认并行跑，env 是进程级全局——本测试和
    /// `with_model_context_window_strict_ignores_env_override` 用同一组 env，
    /// 交错时会污染。改用**串行模式**跑（`cargo test -- --test-threads=1`）才稳定。
    #[test]
    fn with_model_context_window_input_tokens_env_is_capped_not_replacement() {
        let _env_guard = lock_env();
        // 提前方向：env 131K < 公式默认 967K → 用 env 值（用户"更早触发"的意图保留）
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "131000");
        let runtime = minimal_runtime().with_model_context_window(1_000_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 131_000,
            "env INPUT_TOKENS=131000 比公式 967K 小 → min 取 env（提前方向生效）"
        );
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");

        // 拖后方向：env 2M > 公式默认 967K → 被公式封顶（**封顶语义核心断言**）
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "2000000");
        let runtime = minimal_runtime().with_model_context_window(1_000_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 967_000,
            "env INPUT_TOKENS=2M 比公式 967K 大 → 被公式封顶，env 不能拖后"
        );
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
    }

    /// **2026-09-03 G4**：`CLAUDE_CODE_AUTO_COMPACT_WINDOW` 从"替代窗口"改为
    /// "封顶有效窗口"——env 131K + 模型 1M → 有效窗口 = min(1M, 131K) = 131K，
    /// 阈值按 131K 进公式（131K − 20K − 13K = 98K），**不是**直接拿 env 当阈值，
    /// 也**不是**拿 131K 当替代窗口×75%。
    ///
    /// **注意**：env 全局污染——须 `--test-threads=1` 串行跑。
    #[test]
    fn with_model_context_window_env_is_capped_not_replacement() {
        let _env_guard = lock_env();
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "131000");
        let runtime = minimal_runtime().with_model_context_window(1_000_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 98_000,
            "env WINDOW=131000 封顶有效窗口 → 公式(131K − 20K − 13K) = 98K"
        );
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
    }

    /// **2026-09-03 G4**：PCT+WINDOW 组合路径同样受封顶。WINDOW 同时有两重作用：
    /// ①封顶公式侧有效窗口（1M → 131K，公式 = 131K−20K−13K = 98K）；
    /// ②PCT 组合算出原始 env 阈值（131K × 75% = 98250）。
    /// 最终 min(98250, 98000) = 98000——env 侧任何来源都不能拖后于公式值。
    ///
    /// **注意**：env 全局污染——须 `--test-threads=1` 串行跑。
    #[test]
    fn with_model_context_window_pct_override_is_capped_by_formula() {
        let _env_guard = lock_env();
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE", "75");
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "131000");
        let runtime = minimal_runtime().with_model_context_window(1_000_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 98_000,
            "WINDOW 封顶有效窗口 → 公式 98K；PCT 值 98250 > 98K → min 取公式 98000"
        );
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE");
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
    }

    /// **2026-09-03 G4 逃生口**：`CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED=1` 时跳过封顶，
    /// 恢复旧"env 显式替代"语义（调试用）。
    ///
    /// **注意**：env 全局污染——须 `--test-threads=1` 串行跑。
    #[test]
    fn with_model_context_window_uncapped_escape_hatch_restores_replacement_semantics() {
        let _env_guard = lock_env();
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "2000000");
        std::env::set_var("CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED", "1");
        let runtime = minimal_runtime().with_model_context_window(1_000_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 2_000_000,
            "逃生口开启 → env 原始值直接采用（旧替代语义），不被公式封顶"
        );
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
        std::env::remove_var("CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED");
    }

    #[test]
    fn compaction_health_probe_blocks_turn_when_tool_executor_is_broken() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                panic!("API should not run when health probe fails");
            }
        }

        let mut session = Session::new();
        session.record_compaction("summarized earlier work", 4);
        session
            .push_user_text("previous message")
            .expect("message should append");

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new("transport unavailable"))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("trigger", None)
            .expect_err("health probe failure should abort the turn");
        assert!(
            error
                .to_string()
                .contains("Session health probe failed after compaction"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("transport unavailable"),
            "expected underlying probe error: {error}"
        );
    }

    #[test]
    fn compaction_health_probe_skips_empty_compacted_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.record_compaction("fresh summary", 2);

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new(
                "glob_search should not run for an empty compacted session",
            ))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("trigger", None)
            .expect("empty compacted session should not fail health probe");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[test]
    fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[test]
    fn build_assistant_message_places_thinking_block_before_text_and_tool_use() {
        // given
        let events = vec![
            AssistantEvent::Thinking {
                thinking: "pondering".to_string(),
                signature: Some("sig".to_string()),
            },
            AssistantEvent::TextDelta("hello".to_string()),
            AssistantEvent::ToolUse {
                id: "tool-1".to_string(),
                name: "echo".to_string(),
                input: "payload".to_string(),
            },
            AssistantEvent::MessageStop,
        ];

        // when
        let (message, _, _) = build_assistant_message(events)
            .expect("assistant message should preserve thinking, text, and tool blocks");

        // then
        assert_eq!(
            message.blocks,
            vec![
                ContentBlock::Thinking {
                    thinking: "pondering".to_string(),
                    signature: Some("sig".to_string()),
                },
                ContentBlock::Text {
                    text: "hello".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "echo".to_string(),
                    input: "payload".to_string(),
                },
            ]
        );
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[test]
    fn run_turn_graceful_exit_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        impl ApiClient for LoopingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(1);

        // when — **2026-07-23 改为 graceful break**：超限不再报错，而是带已有结果正常返回。
        let summary = runtime
            .run_turn("loop", None)
            .expect("max iterations should gracefully break, not error");

        // then — 选代数不超过 max_iterations+1，且有已执行的 assistant 消息。
        assert_eq!(summary.iterations, 2); // iteration 1 执行了，iteration 2 触发 break
        assert!(!summary.assistant_messages.is_empty());
    }

    #[test]
    fn run_turn_propagates_api_errors() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // when
        let error = runtime
            .run_turn("hello", None)
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }
}

/// Append an auto-compact event to `claw_glm_diag.log`（对齐 microcompact 的 write_microcompact_diag）。
/// 之前 auto-compacted 只 eprintln! 打 stderr 不写 diag 日志，导致从日志判 auto-compact 触发次数时误判为 0。
fn write_auto_compact_diag(removed_message_count: usize, threshold: u32) {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = format!(
        "\n==== claw_auto_compact t={timestamp} removed={removed} threshold={threshold} ====\n",
        removed = removed_message_count,
        threshold = threshold,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("claw_glm_diag.log")
    {
        let _ = f.write_all(record.as_bytes());
    }
}
