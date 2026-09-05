//! Anthropic prompt-cache marker injection.
//!
//! Mirrors upstream claude-code's `addCacheBreakpoints` + `getCacheControl`
//! (`src/services/api/claude.ts`):
//! - Exactly one message-level `cache_control` marker per request, placed on
//!   the last `InputMessage` so the entire preceding prefix is cached.
//! - A `cache_control` marker on the last tool definition so the whole tool
//!   array prefix is cached.
//! - TTL is **session-latched**: `CacheConfig` reads `CLAW_CACHE_TTL` once at
//!   construction (default `"5m"`) and reuses the same value for the entire
//!   session. Mid-session TTL flips would bust the server-side cache key
//!   (upstream claude-code latches TTL for the same reason — see
//!   `should1hCacheTTL` bootstrap-state latch).
//!
//! System-prompt block-level cache markers (`splitSysPromptPrefix`) are
//! **not** implemented here. Upstream relies on a `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`
//! marker to split static/global prefix from dynamic suffix; claw-code builds
//! its system prompt as a single `Option<String>` and has no boundary marker.
//! Adding it is a larger change left for a follow-up; the message-level and
//! tools-level markers below already let `cache_read_input_tokens` cover the
//! system prompt + tool schema + message history prefix.

use crate::types::{CacheControl, InputMessage, ToolDefinition};

/// Session-latched prompt-cache configuration.
///
/// Construct one `CacheConfig` per session (or per `AnthropicClient`), then
/// call `.control()` to obtain the `CacheControl` marker to inject. The TTL
/// is read from `CLAW_CACHE_TTL` at construction time and never changes for
/// the lifetime of this `CacheConfig`, so mid-session TTL flips cannot bust
/// the server-side cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    ttl: String,
    enabled: bool,
}

impl CacheConfig {
    /// Read `CLAW_CACHE_TTL` (default `"5m"`; accepted: `"5m"`, `"1h"`) and
    /// `DISABLE_PROMPT_CACHING` once. The TTL is latched for the lifetime of
    /// the returned `CacheConfig`.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = !std::env::var("DISABLE_PROMPT_CACHING")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let ttl = std::env::var("CLAW_CACHE_TTL")
            .ok()
            .filter(|v| v == "5m" || v == "1h")
            .unwrap_or_else(|| "5m".to_string());
        Self { ttl, enabled }
    }

    /// Build a `CacheConfig` with an explicit TTL (testing / programmatic use).
    #[must_use]
    pub fn new(ttl: &str, enabled: bool) -> Self {
        Self {
            ttl: ttl.to_string(),
            enabled,
        }
    }

    /// **2026-09-03 高缓存命中模式**：显式禁用 cache_control 标记注入的构造器。
    ///
    /// 背景：scnet/DeepSeek 类网关走**自动前缀缓存**（`cache_creation`/`hit`/`miss` 全程 0，
    /// 请求体里的 `cache_control` 被直接忽略——claw_glm_diag.log 实测 744 处死字节）。
    /// 对这类网关，每个 message-level + tools 标记都在白耗 token。判定入口
    /// `is_glm51_cache_model`：仅 glm-5.1 老机制保留标记注入（真 Anthropic 协议后端需要），
    /// 其他模型（glm-5.2 / deepseek 系等高缓存模式）用本构造器禁用。
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            ttl: "5m".to_string(),
            enabled: false,
        }
    }

    /// Whether prompt-cache marker injection is enabled.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// The latched TTL string (e.g. `"5m"`).
    #[must_use]
    pub fn ttl(&self) -> &str {
        &self.ttl
    }

    /// Construct the `CacheControl` marker to inject. Returns `None` when
    /// prompt caching is disabled.
    #[must_use]
    pub fn control(&self) -> Option<CacheControl> {
        if self.enabled {
            Some(CacheControl::ephemeral(Some(&self.ttl)))
        } else {
            None
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Inject exactly one message-level `cache_control` marker on the last
/// `InputMessage`.
///
/// Mirrors upstream claude-code's `addCacheBreakpoints` "exactly one
/// message-level cache_control marker per request" rule. The marker tells the
/// server to cache every byte of the prefix up to and including the last
/// message, so on the next turn only the new user/assistant delta needs to be
/// re-tokenised.
///
/// When caching is disabled (`CacheConfig::enabled() == false`) this is a
/// no-op.
pub fn add_cache_breakpoints(messages: &mut [InputMessage], config: &CacheConfig) {
    let Some(control) = config.control() else {
        return;
    };
    if let Some(last) = messages.last_mut() {
        last.cache_control = Some(control);
    }
}

/// Inject a `cache_control` marker on the last `ToolDefinition`.
///
/// Mirrors upstream claude-code placing `cache_control` on the tail of the
/// `tools` array. The tool schemas are large and stable across a session, so
/// caching them is high-value.
pub fn add_tools_cache_marker(tools: &mut [ToolDefinition], config: &CacheConfig) {
    let Some(control) = config.control() else {
        return;
    };
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(control);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputContentBlock, InputMessage, ToolDefinition};
    use serde_json::json;
    use std::sync::{Mutex, OnceLock};

    /// Serialize env-dependent tests so parallel test threads don't race on
    /// `CLAW_CACHE_TTL` / `DISABLE_PROMPT_CACHING` (process-global env vars).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(Mutex::default)
    }

    fn user_msg(text: &str) -> InputMessage {
        InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
            cache_control: None,
        }
    }

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
        }
    }

    #[test]
    fn cache_config_default_reads_env_5m() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("CLAW_CACHE_TTL");
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        let cfg = CacheConfig::from_env();
        assert!(cfg.enabled());
        assert_eq!(cfg.ttl(), "5m");
    }

    #[test]
    fn cache_config_respects_disable_env() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("CLAW_CACHE_TTL");
        std::env::set_var("DISABLE_PROMPT_CACHING", "1");
        let cfg = CacheConfig::from_env();
        assert!(!cfg.enabled());
        assert!(cfg.control().is_none());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
    }

    #[test]
    fn cache_config_respects_claw_cache_ttl_1h() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::set_var("CLAW_CACHE_TTL", "1h");
        let cfg = CacheConfig::from_env();
        assert_eq!(cfg.ttl(), "1h");
        std::env::remove_var("CLAW_CACHE_TTL");
    }

    #[test]
    fn cache_config_ignores_invalid_ttl() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("CLAW_CACHE_TTL", "garbage");
        let cfg = CacheConfig::from_env();
        assert_eq!(cfg.ttl(), "5m");
        std::env::remove_var("CLAW_CACHE_TTL");
    }

    #[test]
    fn add_cache_breakpoints_places_marker_on_last_message() {
        let cfg = CacheConfig::new("5m", true);
        let mut msgs = vec![user_msg("hello"), user_msg("world")];
        add_cache_breakpoints(&mut msgs, &cfg);
        assert!(msgs[0].cache_control.is_none());
        let last = &msgs[1];
        assert!(last.cache_control.is_some());
        // TTL is propagated.
        let cc = last.cache_control.as_ref().unwrap();
        assert_eq!(cc.type_, "ephemeral");
        assert_eq!(cc.ttl.as_deref(), Some("5m"));
    }

    #[test]
    fn add_cache_breakpoints_noop_when_disabled() {
        let cfg = CacheConfig::new("5m", false);
        let mut msgs = vec![user_msg("hello")];
        add_cache_breakpoints(&mut msgs, &cfg);
        assert!(msgs[0].cache_control.is_none());
    }

    /// ★ 2026-09-03 高缓存命中模式：`disabled()` 构造器必须让两类标记注入全部变成 no-op，
    /// 请求体里不再出现任何 `cache_control` 字节（scnet/DeepSeek 自动前缀缓存网关忽略标记，
    /// 每个标记都是白耗的 token）。
    #[test]
    fn cache_config_disabled_is_noop_for_both_marker_kinds() {
        let cfg = CacheConfig::disabled();
        assert!(!cfg.enabled());
        assert!(cfg.control().is_none());

        let mut msgs = vec![user_msg("hello"), user_msg("world")];
        add_cache_breakpoints(&mut msgs, &cfg);
        assert!(msgs.iter().all(|m| m.cache_control.is_none()));

        let mut tools = vec![tool_def("read_file"), tool_def("write_file")];
        add_tools_cache_marker(&mut tools, &cfg);
        assert!(tools.iter().all(|t| t.cache_control.is_none()));

        // 序列化后请求体不含 cache_control 字节
        let v = serde_json::to_value(&msgs).unwrap();
        assert!(!v.to_string().contains("cache_control"));
    }

    #[test]
    fn add_cache_breakpoints_noop_on_empty_messages() {
        let cfg = CacheConfig::new("5m", true);
        let mut msgs: Vec<InputMessage> = vec![];
        add_cache_breakpoints(&mut msgs, &cfg);
        // No panic, no marker.
        assert!(msgs.is_empty());
    }

    #[test]
    fn add_tools_cache_marker_places_marker_on_last_tool() {
        let cfg = CacheConfig::new("5m", true);
        let mut tools = vec![tool_def("read_file"), tool_def("write_file")];
        add_tools_cache_marker(&mut tools, &cfg);
        assert!(tools[0].cache_control.is_none());
        assert!(tools[1].cache_control.is_some());
    }

    #[test]
    fn add_tools_cache_marker_noop_when_disabled() {
        let cfg = CacheConfig::new("5m", false);
        let mut tools = vec![tool_def("read_file")];
        add_tools_cache_marker(&mut tools, &cfg);
        assert!(tools[0].cache_control.is_none());
    }

    #[test]
    fn add_tools_cache_marker_noop_on_empty_tools() {
        let cfg = CacheConfig::new("5m", true);
        let mut tools: Vec<ToolDefinition> = vec![];
        add_tools_cache_marker(&mut tools, &cfg);
        assert!(tools.is_empty());
    }

    #[test]
    fn cache_control_serializes_to_ephemeral_with_ttl() {
        let cc = CacheControl::ephemeral(Some("5m"));
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v, json!({"type": "ephemeral", "ttl": "5m"}));
    }

    #[test]
    fn cache_control_serializes_without_ttl_when_none() {
        let cc = CacheControl::ephemeral(None);
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v, json!({"type": "ephemeral"}));
    }

    #[test]
    fn message_with_cache_control_serializes_marker() {
        let mut msg = user_msg("hello");
        msg.cache_control = Some(CacheControl::ephemeral(Some("5m")));
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            })
        );
    }
}
