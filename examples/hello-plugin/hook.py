"""A bounded session-start hook using Medha's versioned JSON envelope."""

import json
import sys

event = json.load(sys.stdin)
if event.get("point") == "session_start":
    result = {
        "decision": "add_context",
        "context": "The hello plugin is available when the user asks for a greeting.",
    }
else:
    result = {"decision": "continue"}
print(json.dumps(result))
