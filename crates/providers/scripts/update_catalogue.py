#!/usr/bin/env python3
"""Regenerate the small, public offline catalogue; no keys or inference calls."""
import json
from pathlib import Path
import urllib.request

URL = "https://models.dev/api.json"
LIMIT = 16 * 1024 * 1024
FIELDS = ("name", "api", "env", "npm", "doc")
MODEL_FIELDS = ("name", "limit", "cost", "modalities", "attachment", "tool_call", "reasoning", "reasoning_options")


def main():
    request = urllib.request.Request(URL, headers={"User-Agent": "medha-catalogue-update"})
    with urllib.request.urlopen(request, timeout=15) as response:
        raw = response.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ValueError("public catalogue exceeds 16 MiB")
    data = json.loads(raw)
    if not isinstance(data, dict) or not data or len(data) > 512:
        raise ValueError("invalid provider catalogue")
    compact = {}
    for provider_id, provider in sorted(data.items()):
        item = {field: provider[field] for field in FIELDS if field in provider}
        item["models"] = {
            model_id: {field: model[field] for field in MODEL_FIELDS if field in model}
            for model_id, model in sorted(provider.get("models", {}).items())
        }
        compact[provider_id] = item
    encoded = json.dumps(compact, sort_keys=True, separators=(",", ":")) + "\n"
    path = Path(__file__).resolve().parents[1] / "src/fixtures/provider-catalogue.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(encoded, encoding="utf-8")
    print(f"{len(compact)} providers; {sum(len(p['models']) for p in compact.values())} models; {len(encoded.encode())} bytes")


if __name__ == "__main__":
    main()
