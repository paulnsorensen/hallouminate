from contextlib import contextmanager
from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import select
import subprocess
import time
from typing import Any, Iterator


def stop(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=10)


@dataclass(frozen=True)
class Application:
    binary: Path
    repo: Path
    environment: dict[str, str]

    def cli(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.binary), *arguments],
            cwd=self.repo,
            env=self.environment,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    @contextmanager
    def mcp(self) -> Iterator["Mcp"]:
        with (self.repo.parent / "mcp.log").open("ab") as log:
            process = subprocess.Popen(
                [str(self.binary), "serve"],
                cwd=self.repo,
                env=self.environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=log,
            )
            try:
                client = Mcp(process)
                response = client.rpc("initialize", {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "bdd-tests", "version": "0.0.0"},
                })
                assert "error" not in response, response
                client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
                yield client
            finally:
                stop(process)
                if process.stdin is not None:
                    process.stdin.close()
                if process.stdout is not None:
                    process.stdout.close()


@dataclass
class Mcp:
    process: subprocess.Popen[bytes]
    sequence: int = 0
    pending: bytes = field(default=b"", repr=False)

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def rpc(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.sequence += 1
        self.send({
            "jsonrpc": "2.0", "id": self.sequence,
            "method": method, "params": params,
        })
        assert self.process.stdout is not None
        deadline = time.monotonic() + 30
        while True:
            assert time.monotonic() < deadline, f"MCP timeout for {method}"
            if b"\n" not in self.pending:
                remaining = deadline - time.monotonic()
                assert remaining > 0, f"MCP timeout for {method}"
                ready, _, _ = select.select([self.process.stdout], [], [], remaining)
                assert ready, f"MCP timeout for {method}"
                chunk = os.read(self.process.stdout.fileno(), 65536)
                assert chunk, f"MCP closed stdout for {method}"
                self.pending += chunk
                continue
            line, self.pending = self.pending.split(b"\n", 1)
            response = json.loads(line)
            if response.get("id") == self.sequence:
                assert response["jsonrpc"] == "2.0", response
                return response

    def call(self, app: Application, name: str, **arguments: Any) -> dict[str, Any]:
        return self.rpc("tools/call", {
            "name": name, "arguments": {"cwd": str(app.repo), **arguments},
        })


def tool_result(response: dict[str, Any]) -> dict[str, Any]:
    assert "error" not in response, response
    result = response["result"]
    assert result.get("isError", False) is False, result
    return result["structuredContent"]
