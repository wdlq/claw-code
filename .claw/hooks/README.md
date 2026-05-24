# Keyword Redactor Hook

## 概述

这个 hook 会在 Claude 读取文件时自动替换敏感关键词，保护敏感信息不被 Claude 看到。

## 工作原理

```
文件内容 → Read 工具 → PostToolUse Hook → 关键词替换 → Claude 看到替换后的内容
                ↓
          原始文件不变（AAA、BBB 仍在文件中）
```

## 特性

- **确定性替换**: 同一关键词总是生成相同的替换值（基于 SHA-256 哈希）
- **非破坏性**: 原始文件不会被修改
- **可配置**: 轻松添加/修改要替换的关键词
- **高性能**: 仅在读取文件时触发

## 配置的关键词

| 原始关键词 | 替换示例 |
|-----------|---------|
| AAA | REDACTED_AAA_a3f2b1c4 |
| BBB | REDACTED_BBB_b7c8d9e0 |
| SECRET_KEY | REDACTED_KEY_c1d2e3f4 |
| PASSWORD | REDACTED_PWD_d5e6f7a8 |
| API_TOKEN | REDACTED_TOKEN_e9f0a1b2 |

## 自定义关键词

编辑 `keyword_redactor.py` 文件中的 `KEYWORDS` 字典：

```python
KEYWORDS = {
    "你的关键词": "替换模式_{hash}",
    "MY_SECRET": "REDACTED_{hash}",
}
```

## 文件结构

```
.claude/
├── hooks/
│   ├── keyword_redactor.py    # Hook 脚本
│   ├── test_config.txt        # 测试文件
│   └── README.md              # 本文件
└── settings.json              # Hook 配置
```

## 测试方法

1. 重启 Claude Code 会话
2. 读取 `test_config.txt` 文件
3. 观察输出中 AAA、BBB 等关键词是否被替换

## 禁用方法

临时禁用：在 `.claude/settings.json` 中添加：
```json
{
  "disableAllHooks": true
}
```

永久禁用：从 `settings.json` 中删除 hooks 配置或删除脚本文件。
