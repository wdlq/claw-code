# CLAW-CODE 架构与修复总结

## 2025-06-28 修复记录

### 1. 权限规则路径前缀尾部斜杠问题

**问题描述**：
用户的权限规则配置如下：
```json
"glob_search(E:/内网工程/资料库AI版/sirchmunk/sirchmunk-0.0.7post1/:*)"
```

但访问该目录时仍然报错 "escapes workspace boundary"。

**根本原因**：
- 规则解析后的前缀是 `E:/内网工程/资料库AI版/sirchmunk/sirchmunk-0.0.7post1/`（带尾部斜杠）
- 实际路径是 `E:/内网工程/资料库AI版/sirchmunk/sirchmunk-0.0.7post1`（没有尾部斜杠）
- Rust 的 `starts_with` 严格匹配，导致匹配失败

**修复位置**：
- `rust/crates/runtime/src/permissions.rs` - `allowed_path_prefixes()` 方法和 `PermissionRule::matches()` 方法
- `rust/crates/runtime/src/file_ops.rs` - `validate_workspace_boundary_impl()` 函数

**修复方法**：
在路径比较时统一添加或忽略尾部斜杠，确保前缀能够正确匹配目录及其内容。

---

### 2. GLM 模型 API 响应兼容性问题

**问题描述**：
使用 GLM 模型（glm-5.1）时，返回错误：
```
failed to parse Anthropic response for model glm-5.1: missing field `type`
```

**根本原因**：
1. CLAW-CODE 总是使用流式 API（`stream: true`），而 GLM 返回的响应中 `MessageResponse` 对象缺少 `type` 和 `role` 字段
2. 标准的 Anthropic SSE 响应格式需要这些字段，但 GLM 的兼容实现不包含它们

**修复位置**：
- `rust/crates/api/src/types.rs` - `MessageResponse` 结构体
  - 将 `kind` 字段从 `String` 改为 `Option<String>`
  - 将 `role` 字段从 `String` 改为 `Option<String>`

- `rust/crates/api/src/sse.rs` - SSE 解析器
  - 添加对纯 JSON 格式（而非标准 SSE 格式）的兼容处理

- 多处代码中创建 `MessageResponse` 的地方需要更新以适应新的可选字段

---

### 3. 日志记录的最佳实践

**问题**：
之前的日志记录使用相对路径 `std::env::current_dir().join("glm_debug.log")`，由于工作目录问题导致日志文件未创建。

**解决方案**：
使用绝对路径确保日志文件能够正确创建：
```rust
let _ = std::fs::write("E:/Claude Code/claw-code/glm_sse_debug.log", ...);
```

---

### 关键文件清单

| 文件 | 修改内容 |
|------|----------|
| `rust/crates/runtime/src/permissions.rs` | 路径前缀匹配逻辑，添加尾部斜杠处理 |
| `rust/crates/runtime/src/file_ops.rs` | 工作区边界验证逻辑，添加尾部斜杠处理 |
| `rust/crates/api/src/types.rs` | `MessageResponse` 结构体的 `kind` 和 `role` 改为可选 |
| `rust/crates/api/src/sse.rs` | SSE 解析器添加 JSON 格式兼容处理 |
| `rust/crates/api/src/providers/openai_compat.rs` | 更新 `MessageResponse` 创建代码 |
| `rust/crates/api/src/prompt_cache.rs` | 更新 `MessageResponse` 创建代码 |
| `rust/crates/api/src/providers/anthropic.rs` | 添加 GLM 调试日志 |
| `rust/crates/api/src/client.rs` | 添加调试日志 |
| `rust/crates/tools/src/lib.rs` | 添加调试日志 |
| `rust/crates/mock-anthropic-service/src/lib.rs` | 更新测试代码 |
| `rust/crates/rusty-claude-cli/src/main.rs` | 更新测试代码 |

---

### 调试技巧

1. **使用绝对路径记录日志**：避免工作目录问题
2. **在多个代码路径添加日志**：追踪错误发生的具体位置
3. **检查 SSE 响应的实际格式**：有时 provider 返回的格式与文档描述不同