#!/usr/bin/env python3
"""
Keyword Restorer Hook for Claw Code (PreToolUse)

This hook restores redacted keywords before writing/editing files.
It reverses the replacements made by keyword_redactor.py.

For Edit tools, it restores BOTH old_string and new_string so that:
- old_string can match the actual file content
- new_string contains the correct original keywords

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

        # Only process Write and Edit tools (claw uses write_file and edit_file)
        if tool_name not in ("Write", "Edit", "write_file", "edit_file"):
            print(json.dumps({}))
            sys.exit(0)

        # Process based on tool type
        if tool_name in ("Write", "write_file"):
            content = tool_input.get("content", "")
            restored_content = restore_keywords(content)

            if restored_content != content:
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
                print(json.dumps(result))
            else:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }))

        elif tool_name in ("Edit", "edit_file"):
            # For Edit tool, we need to restore BOTH old_string and new_string
            old_string = tool_input.get("old_string", "")
            new_string = tool_input.get("new_string", "")

            restored_old = restore_keywords(old_string)
            restored_new = restore_keywords(new_string)

            # Check if any changes were made
            if restored_old != old_string or restored_new != new_string:
                result = {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "old_string": restored_old,
                            "new_string": restored_new
                        }
                    }
                }
                print(json.dumps(result))
            else:
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
