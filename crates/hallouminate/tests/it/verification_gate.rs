use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

fn run_git(worktree: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(worktree)
        .output()
        .expect("run git")
}

fn assert_success(output: Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn common_dir(worktree: &Path) -> PathBuf {
    let output = run_git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    assert!(
        output.status.success(),
        "resolve absolute git common dir failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("git common dir is UTF-8")
            .trim(),
    )
}

fn verification_command(
    repo_root: &Path,
    worktree: &Path,
    path: &OsString,
    lock: &Path,
    marker: &Path,
    invocations: &Path,
    args: &[&str],
) -> Command {
    let mut command = Command::new("python3");
    command
        .arg(repo_root.join("scripts/verify.py"))
        .args(args)
        .current_dir(worktree)
        .env("PATH", path)
        .env("VERIFY_TEST_LOCK", lock)
        .env("VERIFY_TEST_MARKER", marker)
        .env("VERIFY_TEST_INVOCATIONS", invocations);
    command
}

fn just_command(
    worktree: &Path,
    path: &OsString,
    lock: &Path,
    marker: &Path,
    invocations: &Path,
    args: &[&str],
) -> Command {
    let mut command = Command::new("just");
    command
        .args(args)
        .current_dir(worktree)
        .env("PATH", path)
        .env("VERIFY_TEST_LOCK", lock)
        .env("VERIFY_TEST_MARKER", marker)
        .env("VERIFY_TEST_INVOCATIONS", invocations);
    command
}

fn write_fake_cargo(bin: &Path, script: &str) -> PathBuf {
    let cargo = bin.join("cargo");
    fs::write(&cargo, script).expect("write fake cargo");
    let mut permissions = fs::metadata(&cargo)
        .expect("fake cargo metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cargo, permissions).expect("make fake cargo executable");
    cargo
}

fn parse_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(path).expect("read JSONL");
    let mut records = Vec::new();
    for line in text.lines() {
        records.push(serde_json::from_str(line).expect("parse JSONL record"));
    }
    records
}

#[test]
fn verification_gate_contract_is_enforced_through_just_and_linked_worktrees() {
    let temp = tempfile::tempdir().expect("tempdir");
    let primary = temp.path().join("primary");
    fs::create_dir(&primary).expect("create primary worktree");
    assert_success(run_git(&primary, &["init", "-b", "main"]), "git init");
    assert_success(
        run_git(&primary, &["config", "user.email", "verify@example.test"]),
        "configure git email",
    );
    assert_success(
        run_git(&primary, &["config", "user.name", "Verification Test"]),
        "configure git name",
    );
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::create_dir(primary.join("scripts")).expect("create scripts directory");
    fs::copy(
        repo_root.join("scripts/verify.py"),
        primary.join("scripts/verify.py"),
    )
    .expect("copy verification script");
    fs::copy(repo_root.join("justfile"), primary.join("justfile")).expect("copy justfile");
    fs::write(primary.join("README.md"), "verification fixture\n").expect("write fixture");
    assert_success(run_git(&primary, &["add", "."]), "git add");
    assert_success(
        run_git(&primary, &["commit", "-m", "fixture"]),
        "git commit",
    );
    let head_output = run_git(&primary, &["rev-parse", "HEAD"]);
    assert!(
        head_output.status.success(),
        "resolve fixture HEAD failed: {}",
        String::from_utf8_lossy(&head_output.stderr)
    );
    let head_sha = String::from_utf8(head_output.stdout)
        .expect("HEAD sha is UTF-8")
        .trim()
        .to_owned();

    let linked = temp.path().join("linked");
    let output = Command::new("git")
        .args(["worktree", "add", "-b", "linked"])
        .arg(&linked)
        .current_dir(&primary)
        .output()
        .expect("add linked worktree");
    assert_success(output, "add linked worktree");

    let primary_common_dir = common_dir(&primary);
    let linked_common_dir = common_dir(&linked);
    assert_eq!(
        primary_common_dir, linked_common_dir,
        "linked worktrees must resolve the same common dir"
    );
    let lock = primary_common_dir.join("hallouminate-verify.lock");
    let log = primary_common_dir.join("hallouminate-verify.jsonl");

    let bin = temp.path().join("bin");
    fs::create_dir(&bin).expect("create fake bin");
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        r#"#!/usr/bin/env python3
import fcntl
import json
import os
from pathlib import Path
import sys
import time

command = sys.argv[1:]
lock_path = Path(os.environ["VERIFY_TEST_LOCK"])
marker = Path(os.environ["VERIFY_TEST_MARKER"])
invocations = Path(os.environ["VERIFY_TEST_INVOCATIONS"])

with lock_path.open("a+") as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        acquired = True
    except BlockingIOError:
        acquired = False
    if command[0] == "fmt":
        fmt_leased = os.environ.get("VERIFY_TEST_FMT_LEASED") == "1"
        if fmt_leased and acquired:
            raise SystemExit("cargo fmt ran outside the verification lease")
        if acquired:
            fcntl.flock(lock, fcntl.LOCK_UN)
    elif acquired:
        fcntl.flock(lock, fcntl.LOCK_UN)
        raise SystemExit("compiler-heavy cargo command ran outside the verification lease")

record = {
    "command": command,
    "jobs": os.environ.get("CARGO_BUILD_JOBS"),
    "worktree": os.getcwd(),
}
with invocations.open("a") as stream:
    stream.write(json.dumps(record) + "\n")

if command[0] == os.environ.get("VERIFY_TEST_FAIL_COMMAND"):
    raise SystemExit(int(os.environ["VERIFY_TEST_FAIL_STATUS"]))

if command[0] != "fmt" and "VERIFY_TEST_SLEEP" in os.environ:
    try:
        marker.mkdir()
    except FileExistsError:
        raise SystemExit("compiler-heavy cargo commands overlapped")
    try:
        time.sleep(float(os.environ["VERIFY_TEST_SLEEP"]))
    finally:
        marker.rmdir()
"#,
    )
    .expect("write fake cargo");
    let mut permissions = fs::metadata(&cargo)
        .expect("fake cargo metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cargo, permissions).expect("make fake cargo executable");

    let marker = temp.path().join("cargo-running");
    let invocation_log = temp.path().join("invocations.jsonl");
    let existing_path = std::env::var_os("PATH").expect("PATH");
    let mut paths = vec![bin.into_os_string()];
    for path in std::env::split_paths(&existing_path) {
        paths.push(path.into_os_string());
    }
    let path = std::env::join_paths(paths).expect("build PATH");

    let first = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[],
    )
    .env("CARGO_BUILD_JOBS", "8")
    .env("VERIFY_TEST_SLEEP", "0.25")
    .spawn()
    .expect("spawn first verification");
    for _attempt in 0..1_000 {
        if marker.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        marker.exists(),
        "first verification did not enter fake cargo"
    );

    let second = verification_command(
        &repo_root,
        &linked,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[],
    )
    .env("VERIFY_TEST_SLEEP", "0.25")
    .output()
    .expect("run second verification");
    let first = first
        .wait_with_output()
        .expect("wait for first verification");
    assert_success(first, "full verification");
    assert_success(second, "second full verification");

    assert!(
        lock.is_file(),
        "verification lock must live in git common dir"
    );
    assert!(
        log.is_file(),
        "verification log must live in git common dir"
    );

    let invocations = parse_jsonl(&invocation_log);
    let full_expected = [
        serde_json::json!(["fmt", "--all", "--check"]),
        serde_json::json!([
            "clippy",
            "--locked",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings"
        ]),
        serde_json::json!(["build", "--locked", "--all-targets"]),
        serde_json::json!(["test", "--locked"]),
    ];
    assert_eq!(invocations.len(), full_expected.len() * 2);
    let mut commands_by_worktree = BTreeMap::<String, Vec<serde_json::Value>>::new();
    for invocation in &invocations {
        let worktree = invocation["worktree"]
            .as_str()
            .expect("worktree")
            .to_owned();
        commands_by_worktree
            .entry(worktree)
            .or_default()
            .push(invocation["command"].clone());
        if invocation["command"] != full_expected[0] {
            assert_eq!(invocation["jobs"], "1");
        }
    }
    assert_eq!(commands_by_worktree.len(), 2);
    for commands in commands_by_worktree.values() {
        assert_eq!(commands, &full_expected);
    }

    let records = parse_jsonl(&log);
    let mut waits = 0;
    let mut starts = 0;
    let mut finishes = 0;
    let mut waited = false;
    let mut worktrees = BTreeSet::new();
    let full_command = serde_json::json!([
        [
            "cargo",
            "clippy",
            "--locked",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings"
        ],
        ["cargo", "build", "--locked", "--all-targets"],
        ["cargo", "test", "--locked"]
    ]);
    let mut full_records = 0;
    for record in records {
        assert!(
            record["timestamp"]
                .as_str()
                .expect("timestamp string")
                .ends_with('Z')
        );
        assert_eq!(record["command"], full_command);
        full_records += 1;
        assert_eq!(record["head"].as_str().expect("HEAD string"), head_sha);
        assert_eq!(
            record["state_digest"]
                .as_str()
                .expect("state digest string")
                .len(),
            64
        );
        assert!(record["pid"].as_u64().expect("PID") > 0);
        worktrees.insert(
            record["worktree"]
                .as_str()
                .expect("worktree string")
                .to_owned(),
        );
        match record["event"].as_str() {
            Some("wait") => waits += 1,
            Some("start") => {
                starts += 1;
                assert_eq!(record["jobs"], 1);
                if record["wait_seconds"].as_f64().expect("wait duration") > 0.1 {
                    waited = true;
                }
            }
            Some("finish") => {
                finishes += 1;
                assert_eq!(record["exit_status"], 0);
                assert!(record["duration_seconds"].as_f64().expect("duration") > 0.0);
            }
            Some(other) => panic!("unexpected event: {other}"),
            None => panic!("event must be a string"),
        }
    }
    assert_eq!((waits, starts, finishes), (2, 2, 2));
    assert!(waited, "the second worktree must record time waiting");
    assert_eq!(worktrees.len(), 2);
    assert_eq!(full_records, 6);

    let credential_commands: &[(&[&str], serde_json::Value, &str)] = &[
        (
            &["cargo", "publish", "--token", "publish-secret-one"],
            serde_json::json!(["cargo", "publish", "--token", "<redacted>"]),
            "publish-secret-one",
        ),
        (
            &["cargo", "publish", "--token", "publish-secret-two"],
            serde_json::json!(["cargo", "publish", "--token", "<redacted>"]),
            "publish-secret-two",
        ),
        (
            &["cargo", "publish", "--token=publish-secret-attached"],
            serde_json::json!(["cargo", "publish", "--token=<redacted>"]),
            "publish-secret-attached",
        ),
        (
            &["cargo", "login", "login-secret"],
            serde_json::json!(["cargo", "login", "<redacted>"]),
            "login-secret",
        ),
    ];
    let mut command_digests = BTreeSet::new();
    let mut exact_command_digests = Vec::new();
    for (arguments, redacted_command, secret) in credential_commands {
        fs::write(&log, "").expect("reset verification log");
        let mut permissions = fs::metadata(&log)
            .expect("verification log metadata")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&log, permissions).expect("broaden verification log permissions");

        let output = verification_command(
            &repo_root,
            &primary,
            &path,
            &lock,
            &marker,
            &invocation_log,
            arguments,
        )
        .output()
        .expect("run credential-bearing verification");
        assert_success(output, "credential-bearing verification");

        let log_text = fs::read_to_string(&log).expect("read verification log");
        assert!(
            !log_text.contains(secret),
            "verification evidence must not contain Cargo credentials"
        );
        assert_eq!(
            fs::metadata(&log)
                .expect("verification log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "verification evidence must be owner-only"
        );

        let records = parse_jsonl(&log);
        assert_eq!(records.len(), 3);
        let command_digest = records[0]["command_digest"]
            .as_str()
            .expect("command digest")
            .to_owned();
        assert_eq!(command_digest.len(), 64);
        for byte in command_digest.bytes() {
            assert!(
                byte.is_ascii_hexdigit(),
                "command digest must be hexadecimal"
            );
        }
        for record in &records {
            assert_eq!(record["command"], *redacted_command);
            assert_eq!(record["command_digest"], command_digest);
        }
        exact_command_digests.push(command_digest.clone());
        command_digests.insert(command_digest);
    }
    assert_eq!(
        command_digests.len(),
        credential_commands.len(),
        "exact commands must have distinct reusable identities"
    );
    assert_ne!(
        exact_command_digests[0], exact_command_digests[1],
        "different secrets must retain distinct exact command identities"
    );

    fs::write(&invocation_log, "").expect("reset invocation log");
    let targeted = just_command(
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[
            "verify", "cargo", "test", "gate", "--", "-j", "4", "-j4", "-j=4", "--jobs", "4",
            "--jobs=4",
        ],
    )
    .output()
    .expect("run targeted just verification");
    assert_success(targeted, "targeted just verification");
    let invocations = parse_jsonl(&invocation_log);
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0]["command"],
        serde_json::json!([
            "test", "gate", "--", "-j", "4", "-j4", "-j=4", "--jobs", "4", "--jobs=4"
        ])
    );
    assert_eq!(invocations[0]["jobs"], "1");

    fs::write(&invocation_log, "").expect("reset invocation log");
    let ci = just_command(&primary, &path, &lock, &marker, &invocation_log, &["ci"])
        .output()
        .expect("run just ci");
    assert_success(ci, "just ci");
    let invocations = parse_jsonl(&invocation_log);
    assert_eq!(invocations.len(), full_expected.len());
    for index in 0..invocations.len() {
        assert_eq!(invocations[index]["command"], full_expected[index]);
    }

    fs::write(&invocation_log, "").expect("reset invocation log");
    let llm = just_command(&primary, &path, &lock, &marker, &invocation_log, &["llm"])
        .env("VERIFY_TEST_FMT_LEASED", "1")
        .output()
        .expect("run just llm");
    assert_success(llm, "just llm");
    let fix_expected = [
        serde_json::json!([
            "clippy",
            "--fix",
            "--all-targets",
            "--all-features",
            "--allow-dirty",
            "--allow-staged"
        ]),
        serde_json::json!(["fmt", "--all"]),
        serde_json::json!(["fmt", "--all", "--check"]),
        full_expected[1].clone(),
        full_expected[2].clone(),
        full_expected[3].clone(),
    ];
    let invocations = parse_jsonl(&invocation_log);
    assert_eq!(invocations.len(), fix_expected.len());
    for index in 0..invocations.len() {
        assert_eq!(invocations[index]["command"], fix_expected[index]);
        assert_eq!(invocations[index]["jobs"], "1");
    }

    let rejected = just_command(
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &["verify", "--fix", "extra"],
    )
    .output()
    .expect("run rejected fix verification");
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("does not accept"),
        "rejection must explain the --fix contract"
    );

    fs::write(&invocation_log, "").expect("reset invocation log");
    fs::write(&log, "").expect("reset verification log");
    let failed = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[],
    )
    .env("VERIFY_TEST_FAIL_COMMAND", "clippy")
    .env("VERIFY_TEST_FAIL_STATUS", "42")
    .output()
    .expect("run failing verification");
    assert_eq!(failed.status.code(), Some(42));
    let invocations = parse_jsonl(&invocation_log);
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0]["command"], full_expected[0]);
    assert_eq!(invocations[1]["command"], full_expected[1]);
    let records = parse_jsonl(&log);
    assert_eq!(records.len(), 3);
    assert_eq!(records[2]["event"], "finish");
    assert_eq!(records[2]["exit_status"], 42);

    fs::write(&invocation_log, "").expect("reset invocation log");
    fs::write(&log, "").expect("reset verification log");
    for _attempt in 0..2 {
        let output = verification_command(
            &repo_root,
            &primary,
            &path,
            &lock,
            &marker,
            &invocation_log,
            &["cargo", "check", "digest"],
        )
        .output()
        .expect("run unchanged-state verification");
        assert_success(output, "unchanged-state verification");
    }
    fs::write(primary.join("state-change.txt"), "changed\n").expect("write state change");
    let output = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &["cargo", "check", "digest"],
    )
    .output()
    .expect("run changed-state verification");
    assert_success(output, "changed-state verification");

    let mut state_digests = Vec::new();
    for record in parse_jsonl(&log) {
        match record["event"].as_str() {
            Some("start") => state_digests.push(
                record["state_digest"]
                    .as_str()
                    .expect("state digest")
                    .to_owned(),
            ),
            Some("wait") | Some("finish") => {}
            Some(other) => panic!("unexpected event: {other}"),
            None => panic!("event must be a string"),
        }
    }
    assert_eq!(state_digests.len(), 3);
    for digest in &state_digests {
        assert_eq!(digest.len(), 64);
        for byte in digest.bytes() {
            assert!(byte.is_ascii_hexdigit(), "state digest must be hexadecimal");
        }
    }
    assert_eq!(state_digests[0], state_digests[1]);
    assert_ne!(state_digests[1], state_digests[2]);

    fs::write(&invocation_log, "").expect("reset invocation log");
    let job_overrides: &[&[&str]] = &[
        &["cargo", "test", "-j", "4"],
        &["cargo", "test", "-j4"],
        &["cargo", "test", "-j=4"],
        &["cargo", "test", "--jobs", "4"],
        &["cargo", "test", "--jobs=4"],
    ];
    for arguments in job_overrides {
        let rejected = verification_command(
            &repo_root,
            &primary,
            &path,
            &lock,
            &marker,
            &invocation_log,
            arguments,
        )
        .output()
        .expect("run rejected job override");
        assert_eq!(rejected.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("job override"),
            "rejection must explain the caller job override"
        );
    }

    let config_overrides: &[&[&str]] = &[
        &["cargo", "--config", "build.jobs=8", "test"],
        &["cargo", "--config=build.jobs=8", "test"],
        &["cargo", "--config", "cargo-config.toml", "test"],
    ];
    for arguments in config_overrides {
        let rejected = verification_command(
            &repo_root,
            &primary,
            &path,
            &lock,
            &marker,
            &invocation_log,
            arguments,
        )
        .output()
        .expect("run rejected Cargo config override");
        assert_eq!(rejected.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("config override"),
            "rejection must explain the Cargo config override"
        );
    }

    let rejected = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &["python3", "-V"],
    )
    .output()
    .expect("run rejected non-Cargo verification");
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("must start with cargo"),
        "rejection must explain the targeted command contract"
    );
    assert_eq!(
        parse_jsonl(&invocation_log).len(),
        0,
        "rejected commands must not invoke Cargo"
    );
}

#[test]
fn verification_log_rotates_when_oversized() {
    let temp = tempfile::tempdir().expect("tempdir");
    let primary = temp.path().join("primary");
    fs::create_dir(&primary).expect("create primary worktree");
    assert_success(run_git(&primary, &["init", "-b", "main"]), "git init");
    assert_success(
        run_git(&primary, &["config", "user.email", "verify@example.test"]),
        "configure git email",
    );
    assert_success(
        run_git(&primary, &["config", "user.name", "Verification Test"]),
        "configure git name",
    );
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::create_dir(primary.join("scripts")).expect("create scripts directory");
    fs::copy(
        repo_root.join("scripts/verify.py"),
        primary.join("scripts/verify.py"),
    )
    .expect("copy verification script");
    fs::write(primary.join("README.md"), "verification fixture\n").expect("write fixture");
    assert_success(run_git(&primary, &["add", "."]), "git add");
    assert_success(
        run_git(&primary, &["commit", "-m", "fixture"]),
        "git commit",
    );

    let primary_common_dir = common_dir(&primary);
    let lock = primary_common_dir.join("hallouminate-verify.lock");
    let log = primary_common_dir.join("hallouminate-verify.jsonl");

    let bin = temp.path().join("bin");
    fs::create_dir(&bin).expect("create fake bin");
    write_fake_cargo(&bin, "#!/usr/bin/env python3\nimport sys\nsys.exit(0)\n");

    let marker = temp.path().join("cargo-running");
    let invocation_log = temp.path().join("invocations.jsonl");
    let existing_path = std::env::var_os("PATH").expect("PATH");
    let mut paths = vec![bin.into_os_string()];
    for path in std::env::split_paths(&existing_path) {
        paths.push(path.into_os_string());
    }
    let path = std::env::join_paths(paths).expect("build PATH");

    let oversized = fs::File::create(&log).expect("create oversized log");
    oversized
        .set_len(LOG_MAX_BYTES + 1)
        .expect("size oversized log");
    drop(oversized);

    let output = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[],
    )
    .output()
    .expect("run verification");
    assert_success(output, "verification with pre-existing oversized log");

    let rotated = primary_common_dir.join("hallouminate-verify.jsonl.1");
    assert!(rotated.is_file(), "oversized log must be rotated to .1");
    assert_eq!(
        fs::metadata(&rotated).expect("rotated log metadata").len(),
        LOG_MAX_BYTES + 1,
        "rotated log must retain the pre-existing oversized content"
    );

    let records = parse_jsonl(&log);
    assert!(
        !records.is_empty(),
        "fresh log must contain this run's records"
    );
    for record in &records {
        assert!(
            record["event"].is_string(),
            "fresh log must parse clean, with no giant prefix from the rotated generation"
        );
    }
}

#[test]
fn fmt_check_failure_is_logged_before_default_mode_returns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let primary = temp.path().join("primary");
    fs::create_dir(&primary).expect("create primary worktree");
    assert_success(run_git(&primary, &["init", "-b", "main"]), "git init");
    assert_success(
        run_git(&primary, &["config", "user.email", "verify@example.test"]),
        "configure git email",
    );
    assert_success(
        run_git(&primary, &["config", "user.name", "Verification Test"]),
        "configure git name",
    );
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::create_dir(primary.join("scripts")).expect("create scripts directory");
    fs::copy(
        repo_root.join("scripts/verify.py"),
        primary.join("scripts/verify.py"),
    )
    .expect("copy verification script");
    fs::write(primary.join("README.md"), "verification fixture\n").expect("write fixture");
    assert_success(run_git(&primary, &["add", "."]), "git add");
    assert_success(
        run_git(&primary, &["commit", "-m", "fixture"]),
        "git commit",
    );

    let primary_common_dir = common_dir(&primary);
    let lock = primary_common_dir.join("hallouminate-verify.lock");
    let log = primary_common_dir.join("hallouminate-verify.jsonl");

    let bin = temp.path().join("bin");
    fs::create_dir(&bin).expect("create fake bin");
    write_fake_cargo(
        &bin,
        "#!/usr/bin/env python3\nimport sys\nif sys.argv[1:] == [\"fmt\", \"--all\", \"--check\"]:\n    sys.exit(1)\nsys.exit(0)\n",
    );

    let marker = temp.path().join("cargo-running");
    let invocation_log = temp.path().join("invocations.jsonl");
    let existing_path = std::env::var_os("PATH").expect("PATH");
    let mut paths = vec![bin.into_os_string()];
    for path in std::env::split_paths(&existing_path) {
        paths.push(path.into_os_string());
    }
    let path = std::env::join_paths(paths).expect("build PATH");

    let output = verification_command(
        &repo_root,
        &primary,
        &path,
        &lock,
        &marker,
        &invocation_log,
        &[],
    )
    .output()
    .expect("run default-mode verification");
    assert!(
        !output.status.success(),
        "default-mode verification must fail when fmt --all --check fails"
    );

    let records = parse_jsonl(&log);
    let fmt_check_record = records
        .iter()
        .find(|record| record["event"] == "fmt-check")
        .expect("evidence log must contain an fmt-check record");
    assert_eq!(fmt_check_record["exit_status"], 1);
}
