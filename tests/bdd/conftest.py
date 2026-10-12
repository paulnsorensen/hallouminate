from collections.abc import Iterator
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

import pytest
from pytest_bdd import given, parsers

from .harness import Application, Mcp, stop

ROOT = Path(__file__).resolve().parents[2]


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--hallouminate-bin",
        default=os.environ.get("HALLOUMINATE_TEST_BIN"),
        help="Absolute path to the built hallouminate binary. Fixtures never build it.",
    )


def pytest_configure(config: pytest.Config) -> None:
    if config.getoption("basetemp") is not None:
        return
    # Default pytest cleanup also visits other projects' numbered directories.
    temporary = tempfile.TemporaryDirectory(prefix="hallou-bdd-run-", dir="/tmp")
    config.option.basetemp = str(Path(temporary.name) / "pytest")
    config.add_cleanup(temporary.cleanup)


@pytest.fixture(scope="session")
def binary(pytestconfig: pytest.Config) -> Path:
    configured = pytestconfig.getoption("--hallouminate-bin")
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    path = Path(configured) if configured else target / "debug/hallouminate"
    path = path.resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        pytest.fail(f"Build the binary with just bdd first: {path}")
    return path


@pytest.fixture
def app(binary: Path, tmp_path: Path) -> Iterator[Application]:
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / ".git").mkdir()
    home = tmp_path / "home"
    home.mkdir()
    environment = {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "HF_HOME": os.environ.get("HF_HOME", str(Path.home() / ".cache/huggingface")),
        "HF_HUB_OFFLINE": "1",
        "TOKENIZERS_PARALLELISM": "false",
        "RAYON_NUM_THREADS": "1",
        "NO_COLOR": "1",
    }
    for kind in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME"):
        directory = tmp_path / kind.lower()
        directory.mkdir()
        environment[f"XDG_{kind}_HOME" if kind != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(directory)
    baseline = Path(environment["XDG_CONFIG_HOME"]) / "hallouminate/config.toml"
    baseline.parent.mkdir()
    baseline.write_text(
        f"[embeddings]\nenabled = false\ncache_dir = {json.dumps(str(tmp_path / 'models'))}\n"
        f"[storage]\nground_dir = {json.dumps(str(tmp_path / 'ground'))}\n"
    )
    # Unix socket paths must fit even when pytest's temporary root is long.
    with tempfile.TemporaryDirectory(prefix="hallou-bdd-", dir="/tmp") as sockets:
        environment["HALLOUMINATE_SOCKET"] = str(Path(sockets) / "daemon.sock")
        yield Application(binary, repo, environment)


@given("an empty repository")
def empty_repository(app: Application) -> None:
    assert not (app.repo / ".hallouminate").exists()


@given(parsers.parse('a repository registered as "{name}"'), target_fixture="original")
def registered_repository(app: Application, name: str) -> dict[str, bytes]:
    result = app.cli("init-repo", name)
    assert result.returncode == 0, result.stderr
    return {
        "config": (app.repo / ".hallouminate/config.toml").read_bytes(),
        "index": (app.repo / ".hallouminate/wiki/index.md").read_bytes(),
    }


@pytest.fixture
def daemon(app: Application) -> Iterator[subprocess.Popen[bytes]]:
    log_path = app.repo.parent / "daemon.log"
    with log_path.open("wb") as log:
        process = subprocess.Popen(
            [str(app.binary), "daemon"],
            cwd=app.repo, env=app.environment,
            stdout=log, stderr=log,
        )
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                assert process.poll() is None, log_path.read_text()
                status = app.cli("daemon", "status")
                if status.returncode == 0 and status.stdout.startswith("running"):
                    break
                time.sleep(0.05)
            else:
                pytest.fail(f"Daemon readiness timeout: {log_path.read_text()}")
            yield process
        finally:
            stop(process)


@pytest.fixture
def client(app: Application, daemon: subprocess.Popen[bytes]) -> Iterator[Mcp]:
    with app.mcp() as client:
        yield client
