#!/usr/bin/env python3
"""
Keyword Restorer Hook for Claw Code (PreToolUse)

This hook restores redacted keywords before writing/editing files.
It reverses the replacements made by keyword_redactor.py.

Usage:
- Add to .claude/settings.json as a PreToolUse hook for Write and Edit tools
"""

import sys
import json
import hashlib

# ============================================================
# CONFIGURATION: Must match keyword_redactor.py
# ============================================================

KEYWORDS = {
    "AAA": "REDACTED_AAA_{hash}",
    "BBB": "REDACTED_BBB_{hash}",
    "SECRET_KEY": "REDACTED_KEY_{hash}",
    "PASSWORD": "REDACTED_PWD_{hash}",
    "API_TOKEN": "REDACTED_TOKEN_{hash}",
}

HASH_LENGTH = 8

# ============================================================
# IMPLEMENTATION
# ============================================================

def generate_hash(keyword: str) -> str:
    """Generate the same deterministic hash as keyword_redactor.py"""
    return hashlib.sha256(keyword.encode()).hexdigest()[:HASH_LENGTH]


def restore_keywords(content: str) -> str:
    """Restore all redacted keywords back to originals."""
    result = content
    for keyword, pattern in KEYWORDS.items():
        hash_value = generate_hash(keyword)
        replacement = pattern.replace("{hash}", hash_value)
        if replacement in result:
            result = result.replace(replacement, keyword)
    return result


def main():
    try:
        # Read JSON input from stdin
        input_data = json.load(sys.stdin)

        # Extract tool information
        tool_name = input_data.get("tool_name", "")
        tool_input = input_data.get("tool_input", {})

        # Only process Write and Edit tools
        if tool_name not in ("Write", "Edit"):
            print(json.dumps({}))
            sys.exit(0)

        # Get content based on tool type
        if tool_name == "Write":
            content = tool_input.get("content", "")
        elif tool_name == "Edit":
            content = tool_input.get("new_string", "")
        else:
            print(json.dumps({}))
            sys.exit(0)

        # Restore keywords in the content
        restored_content = restore_keywords(content)

        # Only return updated input if something changed
        if restored_content != content:
            if tool_name == "Write":
                result = {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "content": restored_content
                        }
                    }
                }
            elif tool_name == "Edit":
                result = {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "new_string": restored_content
                        }
                    }
                }
            print(json.dumps(result))
        else:
            # No changes needed, allow without modification
            print(json.dumps({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow"
                }
            }))

        sys.exit(0)

    except json.JSONDecodeError as e:
        print(json.dumps({"error": f"Invalid JSON input: {e}"}), file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
