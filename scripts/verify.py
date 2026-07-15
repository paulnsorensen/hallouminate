#!/usr/bin/env python3

import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

JOBS = 1
LOCK_NAME = "hallouminate-verify.lock"
LOG_NAME = "hallouminate-verify.jsonl"
FMT_CHECK = ["cargo", "fmt", "--all", "--check"]
HEAVY_COMMANDS = [
    ["cargo", "clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ["cargo", "build", "--locked", "--all-targets"],
    ["cargo", "test", "--locked"],
]
FIX_COMMANDS = [
    [
        "cargo",
        "clippy",
        "--fix",
        "--all-targets",
        "--all-features",
        "--allow-dirty",
        "--allow-staged",
    ],
    ["cargo", "fmt", "--all"],
    FMT_CHECK,
    *HEAVY_COMMANDS,
]


def git_output(*args: str) -> bytes:
    return subprocess.run(
        ["git", *args],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout


def git_common_dir() -> Path:
    path = Path(git_output("rev-parse", "--git-common-dir").decode().strip())
    if not path.is_absolute():
        path = Path.cwd() / path
    return path.resolve()


def repository_state() -> tuple[str, str]:
    head = git_output("rev-parse", "HEAD").decode().strip()
    digest = hashlib.sha256()
    digest.update(head.encode())
    digest.update(b"\0tracked\0")
    digest.update(git_output("diff", "--binary", "--no-ext-diff", "HEAD", "--"))
    digest.update(b"\0untracked\0")
    untracked = git_output("ls-files", "--others", "--exclude-standard", "-z")
    for relative in untracked.split(b"\0"):
        if not relative:
            continue
        digest.update(relative)
        digest.update(b"\0")
        path = Path(os.fsdecode(relative))
        if path.is_symlink():
            digest.update(os.fsencode(os.readlink(path)))
        elif path.is_file():
            digest.update(path.read_bytes())
        digest.update(b"\0")
    return head, digest.hexdigest()


def utc_timestamp() -> str:
    timestamp = datetime.datetime.now(datetime.timezone.utc)
    return timestamp.isoformat(timespec="milliseconds").replace("+00:00", "Z")



def command_digest(command: object) -> str:
    encoded = json.dumps(command, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def redact_command(command: object) -> object:
    if not isinstance(command, list):
        return command
    if not all(isinstance(argument, str) for argument in command):
        return [redact_command(argument) for argument in command]

    redacted = []
    index = 0
    while index < len(command):
        argument = command[index]
        if argument == "--token":
            redacted.append(argument)
            if index + 1 < len(command):
                redacted.append("<redacted>")
                index += 2
                continue
        elif argument.startswith("--token="):
            redacted.append("--token=<redacted>")
            index += 1
            continue
        else:
            redacted.append(argument)
        index += 1

    try:
        login_index = redacted.index("login", 1)
    except ValueError:
        return redacted
    if login_index + 1 < len(redacted):
        redacted[login_index + 1 :] = ["<redacted>"]
    return redacted

def append_record(log: Path, event: str, command: object, **fields: object) -> None:
    head, state_digest = repository_state()
    record = {
        "timestamp": utc_timestamp(),
        "event": event,
        "worktree": str(Path.cwd().resolve()),
        "command": redact_command(command),
        "command_digest": command_digest(command),
        "head": head,
        "state_digest": state_digest,
        "pid": os.getpid(),
        **fields,
    }
    line = (json.dumps(record, separators=(",", ":"), sort_keys=True) + "\n").encode()
    descriptor = os.open(log, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        view = memoryview(line)
        while view:
            written = os.write(descriptor, view)
            view = view[written:]
    finally:
        os.close(descriptor)


def reject_concurrency_overrides(command: list[str]) -> None:
    try:
        delimiter = command.index("--")
    except ValueError:
        delimiter = len(command)
    for argument in command[1:delimiter]:
        if (
            argument == "-j"
            or argument == "--jobs"
            or argument.startswith("-j")
            or argument.startswith("--jobs=")
        ):
            raise ValueError(
                f"caller job override {argument!r} is not allowed; "
                f"the verification lease sets CARGO_BUILD_JOBS={JOBS}"
            )
        if argument == "--config" or argument.startswith("--config="):
            raise ValueError(
                f"caller Cargo config override {argument!r} is not allowed; "
                "the verification gate owns job concurrency"
            )


def run_leased(commands: list[list[str]], command_record: object) -> int:
    common_dir = git_common_dir()
    lock_path = common_dir / LOCK_NAME
    log_path = common_dir / LOG_NAME
    wait_started = time.monotonic()
    append_record(log_path, "wait", command_record)

    with lock_path.open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        wait_seconds = time.monotonic() - wait_started
        started = time.monotonic()
        append_record(
            log_path,
            "start",
            command_record,
            wait_seconds=round(wait_seconds, 6),
            jobs=JOBS,
        )

        status = 0
        environment = os.environ.copy()
        environment["CARGO_BUILD_JOBS"] = str(JOBS)
        try:
            for command in commands:
                status = subprocess.run(command, env=environment).returncode
                if status != 0:
                    break
        except KeyboardInterrupt:
            status = 130
        except OSError as error:
            print(f"verify: {error}", file=sys.stderr)
            status = 127
        finally:
            append_record(
                log_path,
                "finish",
                command_record,
                exit_status=status,
                duration_seconds=round(time.monotonic() - started, 6),
            )
            fcntl.flock(lock, fcntl.LOCK_UN)
        return status


def main(arguments: list[str]) -> int:
    if arguments and arguments[0] == "--fix":
        if len(arguments) != 1:
            print("verify: --fix does not accept a targeted command", file=sys.stderr)
            return 2
        return run_leased(FIX_COMMANDS, FIX_COMMANDS)

    if arguments:
        if arguments[0] != "cargo":
            print("verify: targeted command must start with cargo", file=sys.stderr)
            return 2
        try:
            reject_concurrency_overrides(arguments)
        except ValueError as error:
            print(f"verify: {error}", file=sys.stderr)
            return 2
        return run_leased([arguments], arguments)

    status = subprocess.run(FMT_CHECK).returncode
    if status != 0:
        return status
    return run_leased(HEAVY_COMMANDS, HEAVY_COMMANDS)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
