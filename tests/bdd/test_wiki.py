from typing import Any

from pytest_bdd import given, scenarios, then, when

from .harness import Application, Mcp, tool_result

scenarios("features/wiki.feature")

CORPUS = "repo:demo:wiki"
PATH = "policies/retention.md"
CONTENT = "# Retention policy\n\nArchive records for seven years using retentionbeacon.\n"
LINK = "[retention](./retention.md)"


@given("a published retention policy", target_fixture="wiki_index_before")
def published_policy(app: Application, client: Mcp) -> str:
    publish_policy(app, client)
    return tool_result(client.call(
        app, "read_markdown", corpus=CORPUS, path="policies/index.md",
    ))["content"]


@when("I publish the retention policy")
def publish_policy(app: Application, client: Mcp) -> None:
    result = tool_result(client.call(
        app, "add_markdown", corpus=CORPUS, path=PATH, content=CONTENT,
    ))
    assert result["corpus"] == CORPUS, result
    assert result["path"] == PATH, result


@then("another client reads the exact retention policy")
def policy_readable(app: Application, client: Mcp) -> None:
    with app.mcp() as reader:
        result = tool_result(reader.call(app, "read_markdown", corpus=CORPUS, path=PATH))
    assert result["corpus"] == CORPUS, result
    assert result["path"] == PATH, result
    assert result["content"] == CONTENT, result
    assert result["bytes"] == len(CONTENT.encode()), result


@then("the policy is indexed")
def policy_indexed(app: Application, client: Mcp) -> None:
    result = tool_result(client.call(app, "corpus_stats", corpus=CORPUS))
    assert result["corpus"] == CORPUS, result
    assert result["indexed_files"] == 3, result
    assert result["total_chunks"] == 3, result
    assert result["unindexed_files"] == 0, result


def search_policy(app: Application, client: Mcp) -> dict[str, Any]:
    return tool_result(client.call(
        app, "ground", corpus=CORPUS, query="retentionbeacon",
    ))


@then("a search finds the retention policy")
def policy_found(app: Application, client: Mcp) -> None:
    result = search_policy(app, client)
    docs = result["docs"]
    assert list(docs) == [str(app.repo / ".hallouminate/wiki" / PATH)], result
    document = next(iter(docs.values()))
    assert document["corpus"] == CORPUS, document
    assert document["path"] == PATH, document
    assert any("retentionbeacon" in chunk["snippet"] for chunk in document["chunks"]), document
    assert result["warnings"] == [], result


@then("the wiki index links to the policy")
def policy_linked(app: Application, client: Mcp) -> None:
    index = tool_result(client.call(
        app, "read_markdown", corpus=CORPUS, path="policies/index.md",
    ))["content"]
    assert LINK in index, index
    root = tool_result(client.call(
        app, "read_markdown", corpus=CORPUS, path="index.md",
    ))["content"]
    assert "[policies/](./policies/index.md)" in root, root


@when("I publish a replacement without overwrite permission", target_fixture="rejection")
def replace_policy(app: Application, client: Mcp) -> dict[str, Any]:
    return client.call(
        app, "add_markdown", corpus=CORPUS, path=PATH,
        content="# Replacement\n\nThe original policy is lost.\n",
    )


@then("replacement is rejected because the document exists")
def replacement_rejected(rejection: dict[str, Any]) -> None:
    assert rejection["error"]["code"] == -32602, rejection
    assert "exists" in rejection["error"]["message"], rejection


@then("the wiki index remains unchanged")
def index_unchanged(app: Application, client: Mcp, wiki_index_before: str) -> None:
    result = tool_result(client.call(
        app, "read_markdown", corpus=CORPUS, path="policies/index.md",
    ))
    assert result["content"] == wiki_index_before, result


@when("I delete the retention policy")
def delete_policy(app: Application, client: Mcp) -> None:
    result = tool_result(client.call(app, "delete_markdown", corpus=CORPUS, path=PATH))
    assert result["corpus"] == CORPUS, result
    assert result["path"] == PATH, result


@then("another client cannot read the policy")
def policy_unreadable(app: Application, client: Mcp) -> None:
    with app.mcp() as reader:
        response = reader.call(app, "read_markdown", corpus=CORPUS, path=PATH)
    assert response["error"]["code"] == -32602, response
    assert PATH in response["error"]["message"], response
    assert "does not exist" in response["error"]["message"], response


@then("the policy is absent from search")
def policy_not_found(app: Application, client: Mcp) -> None:
    result = search_policy(app, client)
    assert result["docs"] == {}, result
    assert result["warnings"] == [], result


@then("the wiki index does not link to the policy")
def policy_not_linked(app: Application, client: Mcp) -> None:
    result = tool_result(client.call(
        app, "read_markdown", corpus=CORPUS, path="policies/index.md",
    ))
    assert LINK not in result["content"], result
