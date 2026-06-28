# Claw Code settings.json 配置说明

## 1. 配置文件加载顺序

配置文件按以下优先级从低到高加载，后面的文件会深度合并覆盖前面的文件（对象递归合并，数组和标量直接替换）：

| 顺序 | 来源 | 路径 | 说明 |
|------|------|------|------|
| 1 | 用户（旧版） | `~/.claw.json` | 旧版兼容文件，解析失败时静默跳过 |
| 2 | 用户 | `~/.claw/settings.json` | 主要的用户级配置 |
| 3 | 项目（旧版） | `<cwd>/.claw.json` | 旧版项目级兼容文件 |
| 4 | 项目 | `<cwd>/.claw/settings.json` | 主要的项目级配置 |
| 5 | 本地 | `<cwd>/.claw/settings.local.json` | 机器本地覆盖，不应提交到版本控制 |

> `~/.claw` 目录位置：优先使用 `CLAW_CONFIG_HOME` 环境变量，其次 `$HOME/.claw`，最后回退到 CWD 下的 `.claw`。

---

## 2. 配置项一览

### 2.1 `$schema`

JSON Schema 引用，用于编辑器自动补全。运行时忽略。

```json
{ "$schema": "SettingsSchema" }
```

---

### 2.2 `model`

设置默认使用的模型。
嗯。嗯。嗯。
- **类型**: `String`
- **默认**: 内置默认模型
- **覆盖**: `--model` CLI 参数 > `ANTHROPIC_MODEL` 环境变量 > 此配置
- **内置别名**: `"opus"` -> `"claude-opus-4-6"`, `"sonnet"` -> `"claude-sonnet-4-6"`, `"haiku"` -> `"claude-haiku-4-5-20251213"`

```json
{ "model": "claude-sonnet-4-6" }
```

---

### 2.3 `env`

注入到进程的环境变量，覆盖系统已有变量。多个配置文件中的 `env` 对象会深度合并。

```json
{
  "env": {
    "ANTHROPIC_API_KEY": "sk-ant-...",
    "MY_CUSTOM_VAR": "value"
  }
}
```

---

### 2.4 `aliases`

自定义模型名称别名。解析顺序：用户别名 -> 内置别名。

```json
{
  "aliases": {
    "fast": "claude-haiku-4-5-20251213",
    "smart": "claude-opus-4-6",
    "cheap": "grok-3-mini"
  }
}
```

---

### 2.5 `permissions`

权限系统配置。

```json
{
  "permissions": {
    "defaultMode": "workspace-write",
    "allow": ["read_file(D:/other-project/:*)", "bash(git:*)"],
    "deny": ["bash(rm -rf/:*)"],
    "ask": ["write_file"]
  }
}
```

#### `permissions.defaultMode`

基础权限级别。

| 值 | 等效值 | 说明 |
|---|---|---|
| `"default"`, `"plan"`, `"read-only"` | `ReadOnly` | 只允许读操作 |
| `"acceptEdits"`, `"auto"`, `"workspace-write"` | `WorkspaceWrite` | 允许工作区内读写 |
| `"dontAsk"`, `"danger-full-access"` | `DangerFullAccess` | 允许所有操作 |

#### `permissions.allow` / `permissions.deny` / `permissions.ask`

权限规则列表，格式为 `工具名(匹配条件)`。

**匹配语法**：

| 格式 | 说明 | 示例 |
|------|------|------|
| `ToolName` | 匹配该工具的任意输入 | `Read` |
| `ToolName(*)` | 同上 | `Bash(*)` |
| `ToolName(exact)` | 精确匹配 | `Bash(npm test)` |
| `ToolName(prefix:*)` | 前缀匹配 | `read_file(D:/projects/:*)` |

**规则优先级**: `deny` > `allow` > 模式比较

**Subject 提取字段**（从工具输入 JSON 中提取）: `command`, `path`, `file_path`, `filePath`, `notebook_path`, `notebookPath`, `url`, `pattern`, `code`, `message`

**跨目录访问示例**（允许访问 CWD 以外的特定目录）：

```json
{
  "permissions": {
    "defaultMode": "workspace-write",
    "allow": [
      "read_file(D:/other-project/:*)",
      "write_file(D:/other-project/:*)",
      "edit_file(D:/other-project/:*)",
      "glob_search(D:/other-project/:*)",
      "grep_search(D:/other-project/:*)",
      "bash(ls D:/other-project/:*)",
      "bash(cd D:/other-project/:*)"
    ]
  }
}
```

> 当前工作目录（CWD）内的路径始终自动允许，无需额外配置。

---

### 2.6 `permissionMode` (已弃用)

`permissions.defaultMode` 的旧版简写，加载时会发出弃用警告。两者同时存在时 `permissionMode` 优先。

```json
{ "permissionMode": "workspace-write" }
```

---

### 2.7 `hooks`

配置工具调用生命周期中的钩子命令。多个配置文件中的钩子列表会合并（去重追加）。

```json
{
  "hooks": {
    "PreToolUse": ["echo 'tool about to run'"],
    "PostToolUse": ["echo 'tool finished'"],
    "PostToolUseFailure": ["echo 'tool failed'"]
  }
}
```

#### `hooks.PreToolUse`

工具调用**前**执行的命令。通过 stdin 接收 JSON 载荷，包含 `hook_event_name`, `tool_name`, `tool_input`, `tool_input_json` 等字段。

- 退出码 `0` = 允许
- 退出码 `2` = 拒绝
- 其他非零 = 失败

stdout 可输出 JSON 对象，支持以下字段：
- `systemMessage` / `reason`: 反馈信息
- `continue`: 是否继续
- `hookSpecificOutput.permissionDecision`: `"allow"` / `"deny"` / `"ask"`
- `hookSpecificOutput.updatedInput`: 修改后的工具输入
- `hookSpecificOutput.additionalContext`: 附加上下文

#### `hooks.PostToolUse`

工具调用**后**执行的命令（无论成功或失败）。stdout 可输出 `hookSpecificOutput.updatedToolOutput` 来修改工具输出。

#### `hooks.PostToolUseFailure`

工具调用**失败时**执行的命令。载荷中 `tool_result_is_error` 始终为 `true`。

---

### 2.8 `sandbox`

进程沙箱配置（主要用于 Linux 命名空间隔离）。

```json
{
  "sandbox": {
    "enabled": true,
    "namespaceRestrictions": true,
    "networkIsolation": false,
    "filesystemMode": "workspace-only",
    "allowedMounts": ["logs", "tmp/cache"]
  }
}
```

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `enabled` | `bool` | `true` | 沙箱总开关 |
| `namespaceRestrictions` | `bool` | `true` | 启用 Linux user namespace（需 `unshare` 支持） |
| `networkIsolation` | `bool` | `false` | 禁用沙箱内网络访问 |
| `filesystemMode` | `string` | `"workspace-only"` | 文件系统隔离模式 |
| `allowedMounts` | `string[]` | `[]` | `allow-list` 模式下的额外可访问路径 |

**`filesystemMode` 取值**：

| 值 | 说明 |
|---|---|
| `"off"` | 无文件系统隔离 |
| `"workspace-only"` | 仅工作区目录可访问 |
| `"allow-list"` | 仅 `allowedMounts` 列出的路径（加上工作区）可访问 |

> Windows/macOS 不支持 Linux namespace，沙箱退化为环境变量重定向（`HOME`/`TMPDIR` 指向 CWD 下的沙箱目录）。

---

### 2.9 `mcpServers`

配置 MCP（Model Context Protocol）服务器，为 Agent 提供额外工具。同名服务器条目在后续配置文件中覆盖前面的。

```json
{
  "mcpServers": {
    "my-stdio-server": {
      "type": "stdio",
      "command": "uvx",
      "args": ["mcp-server-sqlite", "--db-path", "test.db"],
      "env": {"TOKEN": "secret"},
      "toolCallTimeoutMs": 30000,
      "required": true
    },
    "my-http-server": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": {"Authorization": "Bearer token"},
      "headersHelper": "helper-script.sh"
    },
    "my-ws-server": {
      "type": "ws",
      "url": "wss://example.com/mcp"
    },
    "my-sdk-server": {
      "type": "sdk",
      "name": "some-sdk-name"
    },
    "my-proxy-server": {
      "type": "claudeai-proxy",
      "url": "https://proxy.example.com",
      "id": "server-id-123"
    }
  }
}
```

**通用字段**：

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `type` | `string` | 自动推断 | 服务器类型 |
| `required` | `bool` | `false` | 是否为必需服务器（启动失败则报错） |

**`type` 取值**: `"stdio"`, `"sse"`, `"http"`, `"ws"`, `"sdk"`, `"claudeai-proxy"`

**Stdio 服务器字段** (`type: "stdio"`)：

| 字段 | 必需 | 说明 |
|------|------|------|
| `command` | 是 | 可执行文件路径 |
| `args` | 否 | 命令行参数，默认 `[]` |
| `env` | 否 | 环境变量，默认 `{}` |
| `toolCallTimeoutMs` | 否 | 单次工具调用超时（毫秒） |

**SSE / HTTP 服务器字段** (`type: "sse"` / `type: "http"`)：

| 字段 | 必需 | 说明 |
|------|------|------|
| `url` | 是 | 服务器 URL |
| `headers` | 否 | 静态 HTTP 头 |
| `headersHelper` | 否 | 提供动态 HTTP 头的 shell 命令 |
| `oauth` | 否 | OAuth 配置对象 |

**WebSocket 服务器字段** (`type: "ws"`)：

| 字段 | 必需 | 说明 |
|------|------|------|
| `url` | 是 | WebSocket URL |
| `headers` | 否 | 静态 HTTP 头 |
| `headersHelper` | 否 | 动态 HTTP 头命令 |

**SDK 服务器字段** (`type: "sdk"`)：

| 字段 | 必需 | 说明 |
|------|------|------|
| `name` | 是 | SDK 名称 |

**Managed Proxy 服务器字段** (`type: "claudeai-proxy"`)：

| 字段 | 必需 | 说明 |
|------|------|------|
| `url` | 是 | 代理 URL |
| `id` | 是 | 服务器 ID |

**MCP OAuth 配置** (`oauth` 子对象）：

```json
{
  "oauth": {
    "clientId": "my-client",
    "callbackPort": 7777,
    "authServerMetadataUrl": "https://issuer.example.com/.well-known/oauth-authorization-server",
    "xaa": true
  }
}
```

---

### 2.10 `oauth`

主运行时的 OAuth 认证配置。

```json
{
  "oauth": {
    "clientId": "my-app",
    "authorizeUrl": "https://auth.example.com/authorize",
    "tokenUrl": "https://auth.example.com/token",
    "callbackPort": 54545,
    "manualRedirectUrl": "https://auth.example.com/callback",
    "scopes": ["org:read", "user:write"]
  }
}
```

| 字段 | 必需 | 说明 |
|------|------|------|
| `clientId` | 是 | OAuth 客户端 ID |
| `authorizeUrl` | 是 | 授权 URL |
| `tokenUrl` | 是 | Token 交换 URL |
| `callbackPort` | 否 | 回调端口 |
| `manualRedirectUrl` | 否 | 手动重定向 URL |
| `scopes` | 否 | 权限范围，默认 `[]` |

---

### 2.11 `plugins`

插件系统配置。

```json
{
  "plugins": {
    "enabled": {
      "tool-guard@builtin": true,
      "sample-plugin@external": false
    },
    "externalDirectories": ["./external-plugins"],
    "installRoot": "plugin-cache/installed",
    "registryPath": "plugin-cache/installed.json",
    "bundledRoot": "./bundled-plugins",
    "maxOutputTokens": 8192
  }
}
```

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `enabled` | `map<string, bool>` | `{}` | 插件 ID 到启用状态的映射 |
| `externalDirectories` | `string[]` | `[]` | 外部插件扫描目录 |
| `installRoot` | `string` | `null` | 已安装插件存储路径 |
| `registryPath` | `string` | `null` | 插件注册表文件路径 |
| `bundledRoot` | `string` | `null` | 内置插件目录 |
| `maxOutputTokens` | `number` | `null` | 插件输出最大 token 数 |

---

### 2.12 `enabledPlugins` (已弃用)

`plugins.enabled` 的旧版顶层写法，加载时会发出弃用警告。

```json
{
  "enabledPlugins": {
    "tool-guard@builtin": true
  }
}
```

---

### 2.13 `providerFallbacks`

主供应商失败时的模型回退链。按顺序尝试，直到有一个成功。

```json
{
  "providerFallbacks": {
    "primary": "claude-opus-4-6",
    "fallbacks": ["grok-3", "grok-3-mini"]
  }
}
```

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `primary` | `string` | `null` | 主模型标识（参考用，实际主模型来自 `model` 字段） |
| `fallbacks` | `string[]` | `[]` | 回退模型列表，按顺序尝试 |

---

### 2.14 `trustedRoots`

受信任的根路径列表。用于子代理（Worker）创建时的信任判断。

```json
{
  "trustedRoots": ["/home/user/projects", "/tmp/worktrees"]
}
```

---

## 3. CLI 专属参数（不在 settings.json 中）

### `--reasoning-effort`

设置推理模型的推理强度。仅对支持 reasoning_effort 的 OpenAI 兼容后端有效。

- **取值**: `"low"`, `"medium"`, `"high"`
- **用法**: `claw --reasoning-effort high`

---

## 4. 完整示例

```json
{
  "$schema": "SettingsSchema",
  "model": "claude-sonnet-4-6",
  "env": {
    "ANTHROPIC_API_KEY": "sk-ant-..."
  },
  "aliases": {
    "fast": "claude-haiku-4-5-20251213",
    "smart": "claude-opus-4-6"
  },
  "permissions": {
    "defaultMode": "workspace-write",
    "allow": [
      "read_file(D:/other-project/:*)",
      "write_file(D:/other-project/:*)",
      "edit_file(D:/other-project/:*)",
      "glob_search(D:/other-project/:*)",
      "grep_search(D:/other-project/:*)",
      "bash(ls D:/other-project/:*)",
      "bash(git:*)"
    ],
    "deny": [
      "bash(rm -rf/:*)"
    ],
    "ask": [
      "write_file"
    ]
  },
  "hooks": {
    "PreToolUse": ["./hooks/pre-tool-check.sh"],
    "PostToolUse": [],
    "PostToolUseFailure": ["./hooks/on-failure.sh"]
  },
  "mcpServers": {
    "sqlite": {
      "type": "stdio",
      "command": "uvx",
      "args": ["mcp-server-sqlite", "--db-path", "app.db"],
      "required": true
    },
    "remote-api": {
      "type": "http",
      "url": "https://api.example.com/mcp",
      "headers": {"Authorization": "Bearer token"}
    }
  },
  "sandbox": {
    "enabled": true,
    "namespaceRestrictions": true,
    "networkIsolation": false,
    "filesystemMode": "workspace-only",
    "allowedMounts": []
  },
  "plugins": {
    "enabled": {
      "tool-guard@builtin": true
    }
  },
  "providerFallbacks": {
    "primary": "claude-opus-4-6",
    "fallbacks": ["grok-3"]
  },
  "trustedRoots": ["/home/user/projects"]
}
```

---

## 5. 弃用字段

| 已弃用字段 | 替代方案 | 说明 |
|-----------|---------|------|
| `permissionMode` | `permissions.defaultMode` | 仍可用，但会发出警告 |
| `enabledPlugins` | `plugins.enabled` | 仍可用，但会发出警告 |

---

## 6. 校验规则

- **未知顶层键**: 硬错误，配置加载失败。会建议最接近的已知键名。
- **类型错误**: 硬错误（如 `"model": 123`）。
- **未知嵌套键**: 在 `hooks`、`permissions`、`plugins`、`sandbox`、`oauth` 内为硬错误。
- **TOML 文件**: 不支持，仅支持 JSON。
- **空文件**: 视为空对象，使用默认值。
- **非对象顶层**: 硬错误，顶层必须是 JSON 对象。
- **旧版 `.claw.json`**: 解析失败时静默跳过（不报错）。
