from pathlib import Path
import subprocess
import tomllib

from pytest_bdd import parsers, scenarios, then, when

from .harness import Application

scenarios("features/repository.feature")


@when(parsers.parse('I register the repository as "{name}"'), target_fixture="registration")
def register(app: Application, name: str) -> subprocess.CompletedProcess[str]:
    return app.cli("init-repo", name)


@when(parsers.parse('I explicitly rename the repository to "{name}"'), target_fixture="registration")
def rename(app: Application, name: str) -> subprocess.CompletedProcess[str]:
    return app.cli("init-repo", name, "--force")


@then("registration succeeds")
def registration_succeeds(registration: subprocess.CompletedProcess[str]) -> None:
    assert registration.returncode == 0, registration.stderr


@then("a later process validates the registered repository")
def validate_repository(app: Application) -> None:
    result = app.cli("config", "validate")
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout.endswith("ok\n"), result.stdout
    assert "repo:demo:wiki" in result.stdout, result.stdout


@then("the wiki has the initial index")
def initial_index(app: Application) -> None:
    assert (app.repo / ".hallouminate/wiki/index.md").read_text() == (
        "# demo wiki\n\n<!-- HALLOUMINATE:INDEX-START -->\n"
        "<!-- HALLOUMINATE:INDEX-END -->\n"
    )


@then("registration is rejected because the configuration exists")
def registration_rejected(registration: subprocess.CompletedProcess[str]) -> None:
    assert registration.returncode == 1, registration
    assert "repo config already exists" in registration.stderr, registration.stderr
    assert "pass --force to overwrite" in registration.stderr, registration.stderr


@then("the repository configuration and wiki remain unchanged")
def repository_unchanged(app: Application, original: dict[str, bytes]) -> None:
    assert (app.repo / ".hallouminate/config.toml").read_bytes() == original["config"]
    assert (app.repo / ".hallouminate/wiki/index.md").read_bytes() == original["index"]


@then(parsers.parse('a later process sees the repository named "{name}"'))
def repository_named(app: Application, name: str) -> None:
    result = app.cli("config", "show")
    assert result.returncode == 0, result.stderr
    # Header and summary lines precede the rendered effective TOML.
    config = tomllib.loads(result.stdout[result.stdout.index("inherit_global_corpora ="):])
    repositories = config["repository"]
    assert len(repositories) == 1, repositories
    assert repositories[0]["name"] == name, repositories
    assert Path(repositories[0]["path"]).resolve() == app.repo.resolve()


@then("the wiki remains unchanged")
def wiki_unchanged(app: Application, original: dict[str, bytes]) -> None:
    assert (app.repo / ".hallouminate/wiki/index.md").read_bytes() == original["index"]
