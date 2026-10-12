from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from threading import Barrier
import subprocess
import tempfile
import time
from typing import Any, Iterator

import pytest
from pytest_bdd import scenarios, then, when

from .harness import Application, Mcp, stop, tool_result


scenarios("features/knowledge.feature")

CORPUS = "repo:demo:wiki"
PATH = "guides/answer.md"
OLD = "# Answer\n\nThe answer is cobaltbeacon.\n"
NEW = "# Answer\n\nThe answer is amberbeacon.\n"


@pytest.fixture
def story(app: Application) -> Iterator[dict[str, Any]]:
    state: dict[str, Any] = {"processes": []}
    yield state
    for process in state["processes"]:
        stop(process)
    app.cli("daemon", "stop")
    if "socket_dir" in state:
        state["socket_dir"].cleanup()


def publish(app: Application, client: Mcp, path: str = PATH, content: str = OLD, **options: Any) -> Any:
    return tool_result(client.call(
        app, "add_markdown", corpus=CORPUS, path=path, content=content, **options,
    ))


def read(app: Application, client: Mcp, path: str = PATH, **options: Any) -> str:
    return tool_result(client.call(app, "read_markdown", path=path, **options))["content"]


def found(app: Application, client: Mcp, fact: str, corpus: str = CORPUS) -> list[str]:
    result = tool_result(client.call(app, "ground", corpus=corpus, query=fact, **{"match": "phrase"}))
    assert result["warnings"] == [], result
    documents = list(result["docs"].values())
    for document in documents:
        assert document["corpus"] == corpus, document
        assert any(fact in chunk["snippet"] for chunk in document["chunks"]), document
    return [document["path"] for document in documents]


def wait_running(app: Application, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        assert process.poll() is None, "daemon exited before readiness"
        status = app.cli("daemon", "status")
        if status.returncode == 0 and status.stdout.startswith("running"):
            return
        time.sleep(0.05)
    pytest.fail("daemon did not become ready")


@when("the daemon restarts after a page is published")
def restart(app: Application, client: Mcp, daemon: subprocess.Popen[bytes], story: dict[str, Any]) -> None:
    publish(app, client)
    stop(daemon)
    assert not Path(app.environment["HALLOUMINATE_SOCKET"]).exists()
    process = subprocess.Popen(
        [str(app.binary), "daemon"], cwd=app.repo, env=app.environment,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    story["processes"].append(process)
    wait_running(app, process)


@then("a new client reads and finds the published page")
def persisted(app: Application) -> None:
    with app.mcp() as reader:
        assert read(app, reader, corpus=CORPUS) == OLD
        assert found(app, reader, "cobaltbeacon") == [PATH]


@when("the owned daemon crashes after a page is published")
def crash(app: Application, client: Mcp, daemon: subprocess.Popen[bytes]) -> None:
    publish(app, client)
    daemon.kill()
    assert daemon.wait(timeout=10) != 0
    assert Path(app.environment["HALLOUMINATE_SOCKET"]).exists()


@then("restarting the daemon lets a new client read the page")
def recovered(app: Application, story: dict[str, Any]) -> None:
    process = subprocess.Popen(
        [str(app.binary), "daemon"], cwd=app.repo, env=app.environment,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    story["processes"].append(process)
    wait_running(app, process)
    with app.mcp() as reader:
        assert read(app, reader, corpus=CORPUS) == OLD
        assert found(app, reader, "cobaltbeacon") == [PATH]


@when("another daemon starts on the occupied socket")
def duplicate(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    publish(app, client)
    story["duplicate"] = subprocess.run(
        [str(app.binary), "daemon"], cwd=app.repo, env=app.environment,
        capture_output=True, text=True, timeout=10, check=False,
    )


@then("the second daemon exits and the first client still reads its page")
def duplicate_rejected(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    result = story["duplicate"]
    assert result.returncode != 0, result
    assert "already holds" in result.stderr, result
    assert read(app, client, corpus=CORPUS) == OLD
    assert found(app, client, "cobaltbeacon") == [PATH]


@when("a new client publishes without a running daemon")
def bootstrap(app: Application, story: dict[str, Any]) -> None:
    socket_dir = tempfile.TemporaryDirectory(prefix="hallou-auto-", dir="/tmp")
    story["socket_dir"] = socket_dir
    app.environment["XDG_RUNTIME_DIR"] = socket_dir.name
    app.environment.pop("HALLOUMINATE_SOCKET")
    with app.mcp() as author:
        publish(app, author)
    status = app.cli("daemon", "status")
    assert status.returncode == 0 and status.stdout.startswith("running"), status


@then("another client reads and finds the published page")
def bootstrapped_page(app: Application) -> None:
    with app.mcp() as reader:
        assert read(app, reader) == OLD
        assert found(app, reader, "cobaltbeacon") == [PATH]


def external_page(app: Application) -> Path:
    path = app.repo / ".hallouminate/wiki" / PATH
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def reindex(app: Application, client: Mcp) -> Any:
    return tool_result(client.call(app, "index", corpus=CORPUS))


@when("I create a page outside the wiki tool and index the wiki")
def external_create(app: Application, client: Mcp) -> None:
    external_page(app).write_text(OLD)
    reindex(app, client)


@then("the new page is readable and searchable")
def external_visible(app: Application, client: Mcp) -> None:
    assert read(app, client) == OLD
    assert found(app, client, "cobaltbeacon") == [PATH]


@when("I change a published page outside the wiki tool")
def external_watched_update(app: Application, client: Mcp) -> None:
    publish(app, client)
    assert found(app, client, "cobaltbeacon") == [PATH]
    external_page(app).write_text(NEW)


@then("the changed fact becomes searchable without explicit indexing")
def watched_fact(app: Application, client: Mcp) -> None:
    deadline = time.monotonic() + 20
    latest: dict[str, Any] = {}
    while time.monotonic() < deadline:
        latest = tool_result(client.call(
            app, "ground", corpus=CORPUS, query="amberbeacon", **{"match": "phrase"},
        ))
        warnings = {warning["code"] for warning in latest["warnings"]}
        assert warnings <= {"index-reconciliation"}, latest
        if not warnings and latest["docs"]:
            assert found(app, client, "amberbeacon") == [PATH]
            assert found(app, client, "cobaltbeacon") == []
            assert read(app, client) == NEW
            stats = tool_result(client.call(app, "corpus_stats", corpus=CORPUS))
            assert stats["unindexed_files"] == 0, stats
            return
        time.sleep(0.1)
    pytest.fail(f"external edit did not become searchable: {latest}")


@when("I change an external page and index the wiki again")
def external_update(app: Application, client: Mcp) -> None:
    external_page(app).write_text(OLD)
    reindex(app, client)
    assert found(app, client, "cobaltbeacon") == [PATH]
    external_page(app).write_text(NEW)
    reindex(app, client)


@then("search finds the new fact and not the old fact")
def new_fact_only(app: Application, client: Mcp) -> None:
    assert read(app, client) == NEW
    assert found(app, client, "amberbeacon") == [PATH]
    assert found(app, client, "cobaltbeacon") == []


@when("I remove an external page and index the wiki again")
def external_delete(app: Application, client: Mcp) -> None:
    external_page(app).write_text(OLD)
    reindex(app, client)
    assert found(app, client, "cobaltbeacon") == [PATH]
    external_page(app).unlink()
    reindex(app, client)


@then("the removed page is absent from navigation and search")
def external_absent(app: Application, client: Mcp) -> None:
    files = tool_result(client.call(app, "list_files", corpus=CORPUS))
    assert PATH not in [entry["path"] for entry in files["files"]], files
    assert found(app, client, "cobaltbeacon") == []
    response = client.call(app, "read_markdown", corpus=CORPUS, path=PATH)
    assert response["error"]["code"] == -32602, response


@when("I index an unchanged wiki twice")
def repeated_index(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    publish(app, client)
    snapshots = [tool_result(client.call(app, "corpus_stats", corpus=CORPUS))]
    reindex(app, client)
    snapshots.append(tool_result(client.call(app, "corpus_stats", corpus=CORPUS)))
    reindex(app, client)
    snapshots.append(tool_result(client.call(app, "corpus_stats", corpus=CORPUS)))
    story["index_snapshots"] = snapshots


@then("the page remains readable and searchable without duplicate results")
def stable_index(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    assert read(app, client) == OLD
    assert found(app, client, "cobaltbeacon") == [PATH]
    snapshots = story["index_snapshots"]
    fields = ("indexed_files", "total_chunks", "unindexed_files")
    counts = [tuple(snapshot[field] for field in fields) for snapshot in snapshots]
    assert counts[0] == counts[1] == counts[2], snapshots
    assert snapshots[2]["unindexed_files"] == 0, snapshots


@when("I overwrite a published page")
def overwrite(app: Application, client: Mcp) -> None:
    publish(app, client)
    publish(app, client, content=NEW, overwrite=True)


@then("the replacement is readable and only its fact is searchable")
def replaced(app: Application, client: Mcp) -> None:
    new_fact_only(app, client)


@when("I append guidance under one heading")
def edit_section(app: Application, client: Mcp) -> None:
    publish(app, client, content="# Guide\n\n## Keep\n\nKeep the violetanchor.\n\n## Change\n\nOriginal.\n")
    publish(app, client, content="\nAdd greenbeacon.\n", under_heading="Change")


@then("the other section remains unchanged and the guidance is searchable")
def section_preserved(app: Application, client: Mcp) -> None:
    content = read(app, client)
    assert content.split("## Change", 1)[0] == "# Guide\n\n## Keep\n\nKeep the violetanchor.\n\n"
    assert content.count("Original.") == 1, content
    assert content.count("Add greenbeacon.") == 1, content
    assert found(app, client, "greenbeacon") == [PATH]
    assert found(app, client, "violetanchor") == [PATH]


@when("I replace a repeated phrase without choosing an occurrence")
def ambiguous(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    original = "# Repetition\n\nEcho phrase.\n\nEcho phrase.\n"
    publish(app, client, content=original)
    story["original"] = original
    story["rejection"] = client.call(
        app, "add_markdown", corpus=CORPUS, path=PATH,
        content="Changed phrase.", replace_match="Echo phrase.",
    )


@then("the edit is rejected and the page remains unchanged")
def ambiguous_rejected(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    assert story["rejection"]["error"]["code"] == -32602, story["rejection"]
    assert "match ambiguous" in story["rejection"]["error"]["message"], story["rejection"]
    assert read(app, client) == story["original"]
    assert found(app, client, "Changed phrase") == []


def parallel_publish(app: Application, paths: list[tuple[str, str]], **options: Any) -> None:
    ready = Barrier(2, timeout=10)

    def write(item: tuple[str, str]) -> Any:
        path, content = item
        with app.mcp() as author:
            ready.wait()
            return publish(app, author, path=path, content=content, **options)

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(write, paths))
    assert len(results) == 2


@when("two clients publish different pages together")
def concurrent_pages(app: Application, client: Mcp) -> None:
    parallel_publish(app, [
        ("authors/one.md", "# One\n\nOne holds silverbeacon.\n"),
        ("authors/two.md", "# Two\n\nTwo holds goldbeacon.\n"),
    ])


@then("both pages remain readable and searchable")
def both_pages(app: Application, client: Mcp) -> None:
    assert read(app, client, "authors/one.md") == "# One\n\nOne holds silverbeacon.\n"
    assert read(app, client, "authors/two.md") == "# Two\n\nTwo holds goldbeacon.\n"
    assert found(app, client, "silverbeacon") == ["authors/one.md"]
    assert found(app, client, "goldbeacon") == ["authors/two.md"]


@when("two clients append different guidance together")
def concurrent_appends(app: Application, client: Mcp) -> None:
    publish(app, client, content="# Shared\n\n## Notes\n\nStart.\n")
    parallel_publish(app, [
        (PATH, "\nAdd northbeacon.\n"),
        (PATH, "\nAdd southbeacon.\n"),
    ], under_heading="Notes")


@then("both additions remain in the section and in search")
def both_additions(app: Application, client: Mcp) -> None:
    content = read(app, client)
    assert content.startswith("# Shared\n\n## Notes\n\nStart.\n"), content
    assert content.count("northbeacon") == 1, content
    assert content.count("southbeacon") == 1, content
    assert found(app, client, "northbeacon") == [PATH]
    assert found(app, client, "southbeacon") == [PATH]


@when("two repositories publish different wiki facts")
def two_repos(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    other_root = app.repo.parent / "other"
    other_root.mkdir()
    (other_root / ".git").mkdir()
    other = Application(app.binary, other_root, app.environment)
    result = other.cli("init-repo", "other")
    assert result.returncode == 0, result.stderr
    publish(app, client)
    with other.mcp() as author:
        outcome = tool_result(author.call(
            other, "add_markdown", corpus="repo:other:wiki", path=PATH,
            content="# Answer\n\nThe answer is copperbeacon.\n",
        ))
        assert outcome["path"] == PATH, outcome
    story["other"] = other


@then("each repository reads its own page and pinned search stays separate")
def repos_isolated(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    other = story["other"]
    assert read(app, client) == OLD
    assert read(other, client, PATH) == "# Answer\n\nThe answer is copperbeacon.\n"
    assert found(app, client, "cobaltbeacon") == [PATH]
    assert found(app, client, "copperbeacon") == []
    assert found(other, client, "copperbeacon", "repo:other:wiki") == [PATH]
    assert found(other, client, "cobaltbeacon", "repo:other:wiki") == []
    local = tool_result(client.call(app, "ground", query="cobaltbeacon"))
    assert list(local["docs"]) == [str(app.repo / ".hallouminate/wiki" / PATH)], local
    assert next(iter(local["docs"].values()))["corpus"] == CORPUS, local
    remote = tool_result(client.call(other, "ground", query="copperbeacon"))
    assert list(remote["docs"]) == [str(other.repo / ".hallouminate/wiki" / PATH)], remote
    assert next(iter(remote["docs"].values()))["corpus"] == "repo:other:wiki", remote
    assert tool_result(client.call(app, "ground", query="copperbeacon"))["docs"] == {}
    assert tool_result(client.call(other, "ground", query="cobaltbeacon"))["docs"] == {}


@when("I publish a page in a nested topic")
def nested_page(app: Application, client: Mcp) -> None:
    publish(app, client)


@then("corpus, file, and tree navigation locate the page")
def navigation(app: Application, client: Mcp) -> None:
    corpora = tool_result(client.call(app, "list_corpora"))
    assert CORPUS in [entry["name"] for entry in corpora["corpora"]], corpora
    files = tool_result(client.call(app, "list_files"))
    assert PATH in [entry["path"] for entry in files["files"]], files
    tree = tool_result(client.call(app, "list_tree"))
    assert tree["corpus"] == CORPUS, tree
    guides = next(node for node in tree["root"]["subdirs"] if node["path"] == "guides")
    assert PATH in [entry["path"] for entry in guides["files"]], guides


@when("I publish a target and a page linking to it")
def link_target(app: Application, client: Mcp) -> None:
    publish(app, client)
    publish(app, client, path="guides/source.md", content="# Source\n\nSee [[guides/answer]].\n")


@then("backlinks name the linking page and not the target")
def links_back(app: Application, client: Mcp) -> None:
    result = tool_result(client.call(app, "backlinks", path=PATH))
    assert result["corpus"] == CORPUS, result
    assert result["path"] == PATH, result
    assert result["backlinks"] == ["guides/source.md"], result
    assert result.get("warnings", []) == [], result


@when("I publish a sourced page")
def sourced(app: Application, client: Mcp) -> None:
    publish(app, client, content=(
        "# Sourced answer\n\n## Decision\n\n"
        "Use indigoanswer.[^proof]\n\n[^proof]: reports/decision.md:42\n"
    ))


@then("reading and search preserve its citation context")
def cite(app: Application, client: Mcp) -> None:
    response = client.call(app, "read_markdown", path=PATH, line_numbers=True)
    result = tool_result(response)
    assert result["content"] == (
        "# Sourced answer\n\n## Decision\n\n"
        "Use indigoanswer.[^proof]\n\n[^proof]: reports/decision.md:42\n"
    )
    display = response["result"]["content"][0]["text"]
    assert "5\tUse indigoanswer.[^proof]" in display or "5  Use indigoanswer.[^proof]" in display, display
    response = client.call(app, "read_markdown", path=PATH, footnotes="only")
    assert "reports/decision.md:42" in response["result"]["content"][0]["text"]
    result = tool_result(client.call(app, "ground", corpus=CORPUS, query="indigoanswer"))
    document = next(iter(result["docs"].values()))
    assert document["path"] == PATH, document
    assert any(
        chunk["heading_path"] == ["Sourced answer"]
        and chunk["line_range"][0] <= 5 <= chunk["line_range"][1]
        and "indigoanswer" in chunk["snippet"]
        for chunk in document["chunks"]
    ), document


@when("I try to publish outside the wiki root")
def path_escape(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    outside = app.repo / ".hallouminate/outside.md"
    outside.write_text("Do not change this.\n")
    story["outside"] = outside
    story["rejection"] = client.call(
        app, "add_markdown", corpus=CORPUS, path="../outside.md",
        content="# Escape\n\nReplace the outside file.\n",
    )


@then("the request is rejected and the outside file stays unchanged")
def outside_unchanged(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    assert story["rejection"]["error"]["code"] == -32602, story["rejection"]
    assert "normal file components" in story["rejection"]["error"]["message"], story["rejection"]
    assert story["outside"].read_text() == "Do not change this.\n"
    assert found(app, client, "Replace the outside file") == []


@when("a batch contains a successful write, a rejected write, and another success")
def partial_batch(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    story["batch"] = tool_result(client.call(app, "add_markdown", corpus=CORPUS, items=[
        {"path": "batch/one.md", "content": "# One\n\nFirstbatchbeacon.\n"},
        {"path": "batch/one.md", "content": "# Conflict\n\nRejectedbatchbeacon.\n"},
        {"path": "batch/two.md", "content": "# Two\n\nSecondbatchbeacon.\n"},
    ]))


@then("the batch reports each outcome and both successful pages remain")
def batch_outcomes(app: Application, client: Mcp, story: dict[str, Any]) -> None:
    results = story["batch"]["results"]
    assert [item["index"] for item in results] == [0, 1, 2], results
    assert [item["path"] for item in results] == ["batch/one.md", "batch/one.md", "batch/two.md"], results
    assert results[0]["result"]["path"] == "batch/one.md", results
    assert results[1]["error"]["code"] == -32602, results
    assert results[2]["result"]["path"] == "batch/two.md", results
    assert read(app, client, "batch/one.md") == "# One\n\nFirstbatchbeacon.\n"
    assert read(app, client, "batch/two.md") == "# Two\n\nSecondbatchbeacon.\n"
    assert found(app, client, "Rejectedbatchbeacon") == []
