#!/usr/bin/python3
"""Stand-in for the model CLI. Exercise the configured MCP server, not a mock."""

import json
import os
from pathlib import Path
import subprocess
import sys


def exchange(process, method, params=None, request_id=None):
    message = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        message["params"] = params
    if request_id is not None:
        message["id"] = request_id
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()
    if request_id is None:
        return None
    for line in process.stdout:
        reply = json.loads(line)
        if reply.get("id") == request_id:
            return reply


def read_wiki(server):
    with subprocess.Popen(
        [server["command"], *server["args"]],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    ) as process:
        initialized = exchange(
            process, "initialize",
            {"protocolVersion": "2024-11-05", "capabilities": {},
             "clientInfo": {"name": "bench-fixture", "version": "1"}},
            1,
        )
        assert "result" in initialized, initialized
        exchange(process, "notifications/initialized")
        listing = exchange(process, "tools/call", {
            "name": "list_corpora", "arguments": {"cwd": "/workspace"}
        }, 2)
        corpora = listing["result"]["structuredContent"]["corpora"]
        assert corpora == [{"name": "benchmark-wiki", "paths": ["/knowledge/wiki"]}], listing
        location = {"cwd": "/workspace", "corpus": "benchmark-wiki", "path": "index.md"}
        attempted = exchange(process, "tools/call", {
            "name": "add_markdown", "arguments": {
                **location, "content": "# Should not persist", "overwrite": True,
            },
        }, 3)
        assert "error" in attempted or attempted.get("result", {}).get("isError"), attempted
        page = exchange(process, "tools/call", {
            "name": "read_markdown", "arguments": location,
        }, 4)
        content = page["result"]["structuredContent"]["content"]
        assert content == "# Frozen page\n\nprivate_knowledge_marker\n", content
        process.stdin.close()
        return content.splitlines()[-1]


args = sys.argv[1:]
if args == ["--version"]:
    print("2.1.0 (Claude Code)")
    sys.exit(0)

assert "--strict-mcp-config" in args, args
assert args[args.index("--setting-sources") + 1] == "", args
model = args[args.index("--model") + 1]
if model == "pinned-judge":
    assert "## Candidate answer" in sys.stdin.read()
    print(json.dumps({"result": "SCORE: 4\nRATIONALE: fixture"}))
    sys.exit(0)

assert model == "pinned-subject", args
prompt = args[args.index("-p") + 1]
if prompt == "emit invalid JSON":
    print("not JSON")
    sys.exit(0)
if prompt == "emit a process failure":
    print("fixture MCP startup failed", file=sys.stderr)
    sys.exit(3)
assert Path.cwd() == Path("/workspace")
assert Path("README.md").read_text() == "pinned public source\n"
assert not Path(".git").exists()
assert not Path("untracked.txt").exists()
assert not Path(".hallouminate").exists()
assert not Path("/knowledge/wiki").exists()
assert not Path("/model-cache").exists()
assert list(Path("/home").iterdir()) == [Path("/home/bench")]
try:
    Path("README.md").write_text("write test")
except OSError:
    pass
else:
    raise AssertionError("source is writable")

config = Path(args[args.index("--mcp-config") + 1])
servers = json.loads(config.read_text())["mcpServers"]
if servers:
    assert set(servers) == {"hallouminate"}
    assert os.environ["HALLOUMINATE_SOCKET"] == "/run/bench/daemon.sock"
    assert Path("/run/bench/daemon.sock").is_socket()
    status = subprocess.run(
        ["/opt/hallouminate", "daemon", "status"],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout
    assert status.startswith("running\n") and "  maintenance:" in status, status
    assert "  idle_exit:" not in status, status
    answer = read_wiki(servers["hallouminate"])
else:
    assert "HALLOUMINATE_SOCKET" not in os.environ
    assert not Path("/run/bench/daemon.sock").exists()
    assert not Path("/opt/hallouminate").exists()
    answer = "no wiki"
print(json.dumps({
    "result": f"pinned public source; {answer}",
    "usage": {"input_tokens": 11, "output_tokens": 22,
              "cache_read_input_tokens": 33, "cache_creation_input_tokens": 44},
}))
