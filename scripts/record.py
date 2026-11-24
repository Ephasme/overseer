"""Records one real streamed completion as a fixture.

Usage: python3 scripts/record.py <fixture-name> <prompt>

Reads base_url and model from OVERSEER_CONFIG (default ~/.config/overseer/config.toml) and the key
from OVERSEER_API_KEY, sends one streaming chat completion, and writes the raw text/event-stream
body to crates/core/tests/fixtures/llm/<fixture-name>.sse. Recorded fixtures replace hand-written
ones when the provider's shape matters, which it does for anything about usage, reasoning or
mid-stream errors.
"""

import json
import os
import pathlib
import sys
import tomllib
import urllib.request

if len(sys.argv) != 3:
    sys.exit("usage: record.py <fixture-name> <prompt>")

name, prompt = sys.argv[1], sys.argv[2]
config_path = pathlib.Path(
    os.environ.get(
        "OVERSEER_CONFIG", pathlib.Path.home() / ".config/overseer/config.toml"
    )
)
config = tomllib.loads(config_path.read_text())
body = {
    "model": config["model"],
    "stream": True,
    "stream_options": {"include_usage": True},
    "messages": [{"role": "user", "content": prompt}],
}
headers = {"content-type": "application/json"}
key = os.environ.get("OVERSEER_API_KEY", "")
if key:
    headers["authorization"] = f"Bearer {key}"
request = urllib.request.Request(
    config["base_url"].rstrip("/") + "/chat/completions",
    data=json.dumps(body).encode(),
    headers=headers,
)
fixture = pathlib.Path("crates/core/tests/fixtures/llm") / f"{name}.sse"
with urllib.request.urlopen(request, timeout=120) as response:
    fixture.write_bytes(response.read())
print(fixture)
