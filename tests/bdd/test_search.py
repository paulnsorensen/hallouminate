from pathlib import Path
import subprocess
import time
from typing import Any

import pytest
from pytest_bdd import scenarios, then, when

from .harness import Application, Mcp, tool_result


scenarios("features/search.feature")

CORPUS = "repo:demo:wiki"
PATH = "guides/answer.md"
PAGE = "# Answer\n\nThe answer is cobaltbeacon.\n"
QUERY = "harbor lantern quarry"
GIT = (
    "git", "-c", "user.name=bdd", "-c", "user.email=bdd@example.invalid",
    "-c", "commit.gpgsign=false",
)


def publish(app: Application, client: Mcp, corpus: str, path: str, content: str) -> None:
    result = tool_result(client.call(app, "add_markdown", corpus=corpus, path=path, content=content))
    assert result["path"] == path, result


def ranked(app: Application, client: Mcp, query: str) -> dict[str, Any]:
    return tool_result(client.call(app, "ground", query=query))


def document(result: dict[str, Any], corpus: str, path: str) -> dict[str, Any] | None:
    for candidate in result["docs"].values():
        if candidate["corpus"] == corpus and candidate["path"] == path:
            return candidate
    return None


def git(cwd: Path, app: Application, *arguments: str) -> None:
    result = subprocess.run(
        [*GIT, *arguments], cwd=cwd, env=app.environment,
        capture_output=True, text=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr


@when("the repository declares a scratch corpus whose folder is absent")
def scratch_corpus(app: Application, client: Mcp) -> None:
    config = app.repo / ".hallouminate/config.toml"
    config.write_text(config.read_text() + (
        '\n[[corpus]]\nname = "demo-scratch"\npaths = [".cheese"]\nglobs = ["**/*.md"]\n'
    ))
    assert not (app.repo / ".cheese").exists()
    publish(app, client, CORPUS, PATH, PAGE)


@then("union search finds the wiki page without a failed pass for the scratch corpus")
def scratch_quiet(app: Application, client: Mcp) -> None:
    deadline = time.monotonic() + 20
    latest: dict[str, Any] = {}
    while time.monotonic() < deadline:
        latest = ranked(app, client, "cobaltbeacon")
        assert document(latest, CORPUS, PATH) is not None, latest
        scratch = []
        for warning in latest["warnings"]:
            assert warning["code"] != "ripgrep-failed", latest
            assert "failed reconciliation" not in warning["message"], latest
            if "demo-scratch" in warning["message"]:
                scratch.append(warning)
        # The watcher still reports the absent root, but reconciliation ends.
        if all("recovery state done" in warning["message"] for warning in scratch):
            return
        time.sleep(0.1)
    pytest.fail(f"scratch corpus reconciliation did not finish: {latest}")


@when("another corpus holds one page that weakly matches the query")
def foreign_corpus(app: Application, request: pytest.FixtureRequest) -> None:
    # The daemon reads the user configuration when it starts.
    foreign = app.repo.parent / "foreign"
    foreign.mkdir()
    filler = "Loudness targets keep streaming masters consistent. " * 20
    (foreign / "mastering.md").write_text(
        f"# Audio mastering\n\n{filler}A harbor appears once.\n"
    )
    baseline = Path(app.environment["XDG_CONFIG_HOME"]) / "hallouminate/config.toml"
    baseline.write_text(baseline.read_text() + (
        f'\n[[corpus]]\nname = "foreign"\npaths = ["{foreign}"]\nglobs = ["**/*.md"]\n'
    ))
    client: Mcp = request.getfixturevalue("client")
    tool_result(client.call(app, "index", corpus="foreign"))
    publish(app, client, CORPUS, "guides/lantern.md",
            "# Harbor lantern quarry\n\nThe harbor lantern lights the quarry road.\n")
    publish(app, client, CORPUS, "guides/gate.md",
            "# Gate notes\n\nThe harbor lantern marks the quarry gate.\n")


@then("union search ranks both local pages above the other corpus's page")
def local_pages_first(app: Application, client: Mcp) -> None:
    result = ranked(app, client, QUERY)
    codes = {warning["code"] for warning in result["warnings"]}
    assert not any(code.startswith("lexical-fallback") for code in codes), result
    foreign = document(result, "foreign", "mastering.md")
    lantern = document(result, CORPUS, "guides/lantern.md")
    gate = document(result, CORPUS, "guides/gate.md")
    assert foreign is not None and lantern is not None and gate is not None, result
    assert lantern["score"] > foreign["score"], result
    assert gate["score"] > foreign["score"], result


@when("I add a git worktree of a committed and indexed repository", target_fixture="worktree")
def fresh_worktree(app: Application, client: Mcp) -> Application:
    root = app.repo.parent / "origin"
    root.mkdir()
    git(root, app, "init", "-q")
    origin = Application(app.binary, root, app.environment)
    result = origin.cli("init-repo", "fresh")
    assert result.returncode == 0, result.stderr
    publish(origin, client, "repo:fresh:wiki", PATH, PAGE)
    assert document(ranked(origin, client, "cobaltbeacon"), "repo:fresh:wiki", PATH) is not None
    git(root, app, "add", "-A")
    git(root, app, "commit", "-q", "-m", "wiki")
    checkout = app.repo.parent / "checkout"
    git(root, app, "worktree", "add", "-q", str(checkout))
    return Application(app.binary, checkout, app.environment)


@then("the first search from the worktree finds the page and the index converges")
def worktree_search(client: Mcp, worktree: Application) -> None:
    first = ranked(worktree, client, "cobaltbeacon")
    assert document(first, "repo:fresh:wiki", PATH) is not None, first
    deadline = time.monotonic() + 20
    latest = first
    while time.monotonic() < deadline:
        latest = ranked(worktree, client, "cobaltbeacon")
        found = document(latest, "repo:fresh:wiki", PATH)
        assert found is not None, latest
        codes = {warning["code"] for warning in latest["warnings"]}
        indexed = all(
            not chunk["chunk_id"].startswith("lexical-fallback:") for chunk in found["chunks"]
        )
        if "index-coverage" not in codes and indexed:
            return
        time.sleep(0.1)
    pytest.fail(f"worktree index did not converge: {latest}")
