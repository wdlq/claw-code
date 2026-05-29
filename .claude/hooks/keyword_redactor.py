#!/usr/bin/env python3
"""
Keyword Redactor Hook for Claw Code

This hook replaces sensitive keywords in file content before Claude sees them.
The replacement is deterministic - the same keyword always produces the same replacement.

Usage:
- Add to .claude/settings.json to activate
- Customize KEYWORDS dict to define your own keywords and replacement patterns
"""

import sys
import json
import hashlib

# ============================================================
# CONFIGURATION: Define keywords to replace
# ============================================================
# Format: "keyword": "replacement_pattern"
# Use {hash} in pattern to insert a deterministic hash value

KEYWORDS = {
    "AAA": "REDACTED_AAA_{hash}",
    "BBB": "REDACTED_BBB_{hash}",
    "SECRET_KEY": "REDACTED_KEY_{hash}",
    "PASSWORD": "REDACTED_PWD_{hash}",
    "API_TOKEN": "REDACTED_TOKEN_{hash}",
}

# Hash length to use (shorter = more compact output)
HASH_LENGTH = 8

# ============================================================
# IMPLEMENTATION (no need to modify below this line)
# ============================================================

def generate_hash(keyword: str) -> str:
    """
    Generate a deterministic hash for a keyword.
    Same keyword always produces the same hash.
    """
    return hashlib.sha256(keyword.encode()).hexdigest()[:HASH_LENGTH]


def replace_keywords(content: str) -> str:
    """Replace all configured keywords in the content."""
    result = content
    for keyword, pattern in KEYWORDS.items():
        if keyword in result:
            hash_value = generate_hash(keyword)
            replacement = pattern.replace("{hash}", hash_value)
            result = result.replace(keyword, replacement)
    return result


def is_reading_hook_self(tool_input: dict, tool_name: str) -> bool:
    """Check if the tool is reading this hook script itself."""
    if tool_name == "Read":
        file_path = tool_input.get("file_path", "")
        # Normalize path separators for cross-platform comparison
        normalized = file_path.replace("\\", "/").lower()
        if "hooks/keyword_redactor.py" in normalized or "hooks\\keyword_redactor.py" in file_path.lower():
            return True
    elif tool_name == "Grep":
        path = tool_input.get("path", "")
        pattern = tool_input.get("pattern", "")
        normalized_path = path.replace("\\", "/").lower()
        # Block grep that targets hook files or searches for keyword patterns
        if "hooks/keyword_redactor" in normalized_path:
            return True
        # Block grep patterns that look like they're trying to extract KEYWORDS dict
        if "KEYWORDS" in pattern and "hooks" in normalized_path:
            return True
    return False


def main():
    try:
        # Read JSON input from stdin
        input_data = json.load(sys.stdin)

        # Extract tool information
        tool_name = input_data.get("tool_name", "")
        tool_input = input_data.get("tool_input", {})
        tool_output = input_data.get("tool_output", "")

        # Security check: prevent reading this hook script itself
        if is_reading_hook_self(tool_input, tool_name):
            print(json.dumps({}))
            sys.exit(0)

        # Only process if there's actual content
        if not tool_output:
            print(json.dumps({}))
            sys.exit(0)

        # Replace keywords in the output
        modified_output = replace_keywords(tool_output)

        # Only return updated output if something changed
        if modified_output != tool_output:
            result = {
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "updatedToolOutput": modified_output
                }
            }
            print(json.dumps(result))
        else:
            # No changes needed, return empty JSON
            print(json.dumps({}))

        sys.exit(0)

    except json.JSONDecodeError as e:
        print(json.dumps({"error": f"Invalid JSON input: {e}"}), file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
