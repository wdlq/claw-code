#!/usr/bin/env python3
"""
PostToolUse hook script to replace keywords in file content.

This script replaces specified keywords (AAA, BBB, etc.) with their
encoded versions when Claude reads files. The replacement is deterministic
based on the keyword itself.

Usage:
1. Place this script in your project
2. Add the hook configuration to .claude/settings.json:

{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "python replace_keywords.py"
          }
        ]
      }
    ]
  }
}
"""

import sys
import json
import hashlib

# Define keywords to replace and their replacement algorithm
KEYWORDS = ["AAA", "BBB", "CCC"]

def generate_replacement(keyword: str) -> str:
    """
    Generate a deterministic replacement for a keyword.
    Uses SHA-256 hash to ensure the same keyword always produces the same replacement.
    """
    hash_obj = hashlib.sha256(keyword.encode())
    hash_hex = hash_obj.hexdigest()[:12]  # Use first 12 chars of hash
    return f"REPLACED_{hash_hex}"

def main():
    try:
        # Read JSON input from stdin
        input_data = json.load(sys.stdin)

        # Get tool output
        tool_output = input_data.get("tool_output", "")

        # Replace keywords in the output
        modified_output = tool_output
        for keyword in KEYWORDS:
            replacement = generate_replacement(keyword)
            modified_output = modified_output.replace(keyword, replacement)

        # Return updatedToolOutput
        result = {
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "updatedToolOutput": modified_output
            }
        }

        print(json.dumps(result))
        sys.exit(0)

    except Exception as e:
        # On error, return empty output (no replacement)
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
