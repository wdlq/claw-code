use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, estimate_session_tokens, CompactionConfig, CompactionResult,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::session::{ContentBlock, ConversationMessage, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 55_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";
/// Percentage of context window to trigger auto-compact (e.g. 75 means 75%).
const AUTO_COMPACT_PCT_OVERRIDE_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE";
/// Context window size in tokens, used together with PCT_OVERRIDE.
const AUTO_COMPACT_WINDOW_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

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

    /// 按**模型上下文窗口**动态算auto-compact阈值（二期-C1）。
    ///
    /// DeepSeek V4 Pro 1M窗口→750K才压，几乎不触发，前缀稳定→DeepSeek硬盘缓存命中。
    /// GLM 5.1 200K窗口→150K才压，不撑爆200K上下文。
    ///
    /// **优先级**：env显式阈值（`CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` /
    /// `CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE`+`CLAUDE_CODE_AUTO_COMPACT_WINDOW`）
    /// 优先于本builder。只有env没显式设阈值时，才用`context_window × pct`动态算。
    /// 默认pct=75（对齐官方claude-code的0.75阈值）。
    #[must_use]
    pub fn with_model_context_window(mut self, context_window_tokens: u32) -> Self {
        // env没显式设阈值时才动态算，避免覆盖用户显式配置。
        if std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR).is_err()
            && std::env::var(AUTO_COMPACT_PCT_OVERRIDE_ENV_VAR).is_err()
        {
            let pct = 75u32; // 对齐官方claude-code的0.75阈值
            let dynamic_threshold = (context_window_tokens as u64 * pct as u64 / 100) as u32;
            // 下限保护：太小阈值会频繁compact反伤缓存，至少给到默认阈值
            self.auto_compaction_input_tokens_threshold =
                dynamic_threshold.max(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD);
        }
        self
    }

    /// **2026-07-19 multiprovider 落地**：子 agent 专用——按 model 上下文窗口动态算
    /// auto-compact 阈值，**不读任何 env 覆盖**，强制用 `(context_window - max_output) × 75%`。
    ///
    /// 修的破裂点：主 LLM 走 DeepSeek 1M，子 agent 走 GLM 200K 时，若 `.claw.json`
    /// 的 `env` 段显式设了 `CLAUDE_CODE_AUTO_COMPACT_WINDOW=131000`（针对 GLM 200K 算的 75%），
    /// 原路径会把这套全局 env 误施加到子 agent 上——主=DeepSeek 时子 agent 阈值被压到 131K
    /// 频繁 compact 反伤 DeepSeek 缓存；主=GLM 子=DeepSeek 时子 agent 阈值 750K 直接撑爆
    /// GLM 200K 窗口报 `ContextWindowExceeded` 400。
    ///
    /// **2026-07-22 改进**：阈值基于“输入预算”（context_window - max_output_tokens）而非
    /// 总窗口。之前用 `context_window × 75%` 算出 150K，但 GLM-5.1 的 max_output=64K，
    /// 实际输入预算只有 136K——阈值 150K > 输入预算 136K，导致 auto-compact 触发前
    /// 请求已经超出 GLM 的输入上限报 400。改为 `(200K-64K)×75% = 102K` 后安全。
    ///
    /// 子 agent 走 strict 路径后阈值严格按自己的 model 算，与主 LLM 的 env 配置彻底独立。
    /// 仍保留下限保护：至少给到 `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD` 55K，
    /// 避免子 agent 走 128K DeepSeek-flash 等小窗口模型算出太小阈值频繁 compact 反伤缓存。
    ///
    /// 对照 `docs/multiprovider.md` 3.4ter 节。主 LLM 路径仍走 `with_model_context_window`
    /// （允许用户用 env 显式覆盖主 LLM 阈值），两条路径彻底独立。
    #[must_use]
    pub fn with_model_context_window_strict(
        mut self,
        context_window_tokens: u32,
        max_output_tokens: u32,
    ) -> Self {
        let pct = 75u32; // 对齐官方claude-code的0.75阈值
                         // 基于输入预算（总窗口 - 输出预留）算阈值，确保触发 compact 时 input 仍在窗口内。
        let input_budget = context_window_tokens.saturating_sub(max_output_tokens);
        let dynamic_threshold = (input_budget as u64 * pct as u64 / 100) as u32;
        self.auto_compaction_input_tokens_threshold =
            dynamic_threshold.max(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD);
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
            );
            if mc_result.cleared_count > 0 {
                eprintln!(
                    "[micro-compact: cleared {} old tool result(s), freed {} chars]",
                    mc_result.cleared_count, mc_result.chars_freed
                );
            }

            // Pre-flight auto-compact: if the session is still large after
            // micro-compact, do a full auto-compact. This prevents 400 errors
            // from providers (e.g. GLM) that reject oversized requests.
            if self.session_needs_pre_flight_compact() {
                if let Some(event) = self.maybe_auto_compact() {
                    eprintln!(
                        "[auto-compacted: removed {} messages]",
                        event.removed_message_count
                    );
                    write_auto_compact_diag(
                        event.removed_message_count,
                        self.auto_compaction_input_tokens_threshold,
                    );
                }
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
                        // 再 `continue` 重试本轮。`maybe_auto_compact` 轻量路径阈值不够低时撝不住，
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

        let auto_compaction = self.maybe_auto_compact();
        if let Some(event) = &auto_compaction {
            write_auto_compact_diag(
                event.removed_message_count,
                self.auto_compaction_input_tokens_threshold,
            );
        }

        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction,
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

    fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
        // Some providers (e.g. GLM) don't return input_tokens in usage.
        // Fall back to a rough estimate from the session messages so
        // auto-compact still triggers when the context grows large.
        let input_tokens = self.usage_tracker.cumulative_usage().input_tokens;
        let estimated_tokens = if input_tokens == 0 {
            // **2026-07-23 CJK 感知估算**：bytes/4 对中文严重偏低（UTF-8 中文 3 bytes ≈ 1 token，
            // 但 bytes/4 只算 0.75 token）。用 `estimate_tokens_mixed` 按 ASCII/非 ASCII 分开算。
            let byte_count: usize = self
                .session
                .messages
                .iter()
                .map(|m| {
                    m.blocks
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text { text } => text.len(),
                            ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                            ContentBlock::ToolResult { output, .. } => output.len(),
                            _ => 0,
                        })
                        .sum::<usize>()
                })
                .sum::<usize>();
            estimate_tokens_mixed(byte_count)
        } else {
            input_tokens
        };
        if estimated_tokens < self.auto_compaction_input_tokens_threshold {
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
            return None;
        }

        self.session = result.compacted_session;
        Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        })
    }

    /// Returns true if the current session is large enough to risk a
    /// provider-side rejection (e.g. GLM's 400 Bad Request) and should
    /// be compacted BEFORE the next API call.
    fn session_needs_pre_flight_compact(&self) -> bool {
        // **2026-07-23 CJK 感知 + 系统开销估算**：
        // 之前用 bytes/4 对中文内容严重偏低（实际≈ bytes/3），导致 pre-flight
        // 放行了实际已超 GLM 输入预算的请求。现改用 `estimate_tokens_mixed`，
        // 并加上系统提示词+工具定义的开销估算（~15K tokens）。
        let byte_count: usize = self
            .session
            .messages
            .iter()
            .map(|m| {
                m.blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                        ContentBlock::ToolResult { output, .. } => output.len(),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum::<usize>();
        let estimated_tokens = estimate_tokens_mixed(byte_count);
        // 加上系统提示词 + 工具定义的开销（子 agent 约 10-15K tokens）。
        // 主 LLM 路径也适用——系统提示词始终随请求发送但不计入 session messages。
        const SYSTEM_OVERHEAD_TOKENS: u32 = 15_000;
        let total_estimate = estimated_tokens.saturating_add(SYSTEM_OVERHEAD_TOKENS);
        total_estimate >= self.auto_compaction_input_tokens_threshold
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

/// **2026-07-23 CJK 感知 token 估算**：替代原来的 `bytes / 4` 硬编码。
///
/// 原理：
/// - 纯英文/代码：UTF-8 1 byte/char，tokenizer ~4 chars/token → bytes/4
/// - 纯中文：UTF-8 3 bytes/char，tokenizer ~1 token/char → bytes/3
/// - 混合内容：取两者中间值 bytes/3 作为保守估算（宁可早 compact 也不撑爆 400）
///
/// 用 bytes/3 而非精确统计非 ASCII 字节数，因为：
/// 1. 子 agent 读的文件多为中文注释+英文代码混合，bytes/3 是安全上界
/// 2. 早触发 compact 的代价（丢失部分上下文）远小于 400 错误的代价（整个任务失败）
/// 3. 避免遍历内容统计字节分布的性能开销
#[must_use]
fn estimate_tokens_mixed(byte_count: usize) -> u32 {
    (byte_count as u64 / 3) as u32
}

/// Reads the automatic compaction threshold from the environment.
/// Supports three formats:
/// 1. CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS (direct token count)
/// 2. CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE + CLAUDE_CODE_AUTO_COMPACT_WINDOW (percentage of context window)
/// 3. Default: 55,000 tokens
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
        build_assistant_message, ApiClient, ApiRequest, AssistantEvent, AutoCompactionEvent,
        ConversationRuntime, PromptCacheEvent, RuntimeError, StaticToolExecutor, ToolExecutor,
        DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
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
    fn auto_compacts_when_cumulative_input_threshold_is_crossed() {
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

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
            })
        );
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
                        input_tokens: 99_999,
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

    /// ★ 2026-07-19 multiprovider：strict 变体不读任何 env，强制用 `(context_window - max_output) × 75%`。
    /// 验 DeepSeek V4 Pro 1M 窗口 + max_output=0 → 750K 阈值。
    #[test]
    fn with_model_context_window_strict_uses_dynamic_threshold_without_env() {
        let runtime = minimal_runtime().with_model_context_window_strict(1_000_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 750_000,
            "1M 窗口 × 75% = 750K，不读 env"
        );
    }

    /// ★ 2026-07-22：strict 变体验 GLM-5.1 场景——200K 窗口 - 64K max_output = 136K 输入预算，
    /// 136K × 75% = 102K 阈值。确保 auto-compact 在输入超出实际上限前触发。
    #[test]
    fn with_model_context_window_strict_glm51_scenario() {
        // GLM-5.1: context=200K, effective max_tokens=64K
        let runtime = minimal_runtime().with_model_context_window_strict(200_000, 64_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 102_000,
            "(200K - 64K) × 75% = 102K，安全低于 136K 输入上限"
        );
    }

    /// ★ 2026-07-19 multiprovider：strict 变体**不读 env 覆盖**——
    /// 即使主 LLM 那套全局 env 显式设了阈值，子 agent 路径也不被污染。
    /// 这是 strict 变体与原 `with_model_context_window` 的核心差异点。
    #[test]
    fn with_model_context_window_strict_ignores_env_override() {
        // 模拟主 LLM 在 .claw.json env 段设的全局阈值——这套是针对主 LLM 算的，
        // 不该施加到子 agent 上（主=DeepSeek 1M 时这套 env 设 131K 是给 GLM 200K 用的，
        // 强加到子 agent=DeepSeek 1M 会被压到 131K 频繁 compact 反伤缓存）。
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "131000");
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE", "75");
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "131000");

        let runtime = minimal_runtime().with_model_context_window_strict(1_000_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 750_000,
            "strict 路径必须忽略 env 覆盖，按 (1M - 0) × 75% = 750K 算"
        );

        // 清理 env 防止污染后续测试
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE");
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
    }

    /// ★ 2026-07-19 multiprovider：strict 变体保留下限保护——
    /// 子 agent 走小窗口模型（如 DeepSeek-v4-flash 128K）算出 96K 阈值，
    /// 仍被 `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD` 55K 下限保护兜底。
    /// 太小阈值频繁 compact 反伤缓存，对子 agent 仍是不良。
    #[test]
    fn with_model_context_window_strict_keeps_floor_protection() {
        // (128K - 0) × 75% = 96K > 55K 下限，直接用 96K
        let runtime = minimal_runtime().with_model_context_window_strict(128_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 96_000,
            "(128K - 0) × 75% = 96K > 55K 下限，用 96K"
        );

        // (50K - 0) × 75% = 37.5K < 55K 下限，兖底到 55K
        let runtime = minimal_runtime().with_model_context_window_strict(50_000, 0);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold,
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
            "(50K - 0) × 75% = 37.5K < 55K 下限，兖底到 55K"
        );
    }

    /// ★ 2026-07-19 multiprovider：对照测试——原 `with_model_context_window` **会被 env 覆盖**。
    /// 同样的 env 设定下，原路径不动态算（保留 `new()` 里 `auto_compaction_threshold_from_env()` 读的 env 值），
    /// strict 路径强制动态算（用 750K）。这条测试佐证两条路径彻底独立。
    ///
    /// **注意**：cargo test 默认并行跑，env 是进程级全局——本测试和 `with_model_context_window_strict_ignores_env_override`
    /// 都用 `set_var`/`remove_var` 操作同一组 env，交错时会污染。改用**串行模式**跑这条对照测试
    /// （`cargo test -- --test-threads=1` 或单独 `cargo test with_model_context_window_original`）才能稳定。
    #[test]
    fn with_model_context_window_original_path_still_reads_env_override() {
        // 自设 env——不依赖其他测试的时序
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "131000");
        // 关键：`with_model_context_window` 的逻辑是"两个 env 都没设才动态算"，
        // 我们设了 INPUT_TOKENS，那它就**不进入**动态算分支，
        // 字段保持 `ConversationRuntime::new()` 构造时调 `auto_compaction_threshold_from_env()`
        // 读 INPUT_TOKENS=131000 算出的 131K。
        let runtime = minimal_runtime().with_model_context_window(1_000_000);
        assert_eq!(
            runtime.auto_compaction_input_tokens_threshold, 131_000,
            "原路径 env 设了 INPUT_TOKENS=131000 就用 131000，不动态算 750K"
        );

        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
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
