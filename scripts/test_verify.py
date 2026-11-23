import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import unittest

import verify


SCRIPTS = str(Path(__file__).resolve().parent)


def worker(code, *args, cwd=None):
    return subprocess.Popen(
        [sys.executable, "-u", "-c", code, *map(str, args)],
        cwd=cwd,
        env={**os.environ, "PYTHONPATH": SCRIPTS},
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


class VerificationTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("just"), "just is not installed")
    def test_just_preserves_arguments_and_compatibility_routes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copyfile(Path(SCRIPTS).parent / "justfile", root / "justfile")
            (root / "scripts").mkdir()
            (root / "scripts" / "verify.py").write_text(
                "import json, sys; print(json.dumps(sys.argv[1:]))\n"
            )
            for recipe, expected in (
                (["verify"], []),
                (["ci"], []),
                (["llm"], ["--fix"]),
                (["verify", "cargo", "test", "space and 'quote'", 'double"quote', "$literal"],
                 ["cargo", "test", "space and 'quote'", 'double"quote', "$literal"]),
            ):
                result = subprocess.run(
                    ["just", *recipe], cwd=root, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(result.stdout), expected)

    def test_lock_waits_and_recovers_after_process_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lease"
            code = (
                "import sys; from pathlib import Path; import verify\n"
                "print('attempt', flush=True)\n"
                "with verify.exclusive_lock(Path(sys.argv[1])):\n"
                " Path(sys.argv[2]).touch()\n"
                " print('acquired', flush=True)\n"
                " sys.stdin.readline()\n"
            )
            first = worker(code, path, Path(directory) / "first")
            second = None
            try:
                self.assertEqual(first.stdout.readline().strip(), "attempt")
                self.assertEqual(first.stdout.readline().strip(), "acquired")
                marker = Path(directory) / "second"
                second = worker(code, path, marker)
                self.assertEqual(second.stdout.readline().strip(), "attempt")
                with self.assertRaises(subprocess.TimeoutExpired):
                    second.wait(timeout=0.3)
                self.assertFalse(marker.exists(), "second process entered the held lease")
                first.kill()
                first.communicate(timeout=10)
                stdout, stderr = second.communicate("release\n", timeout=10)
                self.assertEqual(second.returncode, 0, stderr)
                self.assertEqual(stdout.strip(), "acquired")
            finally:
                for process in (first, second):
                    if process is not None:
                        if process.poll() is None:
                            process.kill()
                        process.communicate()

    def test_linked_worktrees_share_lease_and_records(self):
        with tempfile.TemporaryDirectory() as directory:
            primary = Path(directory) / "primary"
            linked = Path(directory) / "linked"
            primary.mkdir()

            def git(*args):
                result = subprocess.run(
                    ["git", "-C", str(primary), *args],
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

            git("init", "-b", "main")
            git("-c", "user.name=Test", "-c", "user.email=test@example.com",
                "commit", "--allow-empty", "-m", "initial")
            git("worktree", "add", "-b", "linked", str(linked))
            child = "import sys; print('running', flush=True); sys.stdin.readline()"
            code = (
                "import sys, verify\n"
                "command = [sys.executable, '-u', '-c', sys.argv[1]]\n"
                "raise SystemExit(verify.run_leased([command], command))\n"
            )
            first = worker(code, child, cwd=primary)
            second = None
            try:
                self.assertEqual(first.stdout.readline().strip(), "running")
                second = worker(code, child, cwd=linked)
                with self.assertRaises(subprocess.TimeoutExpired):
                    second.wait(timeout=0.3)
                first.communicate("release\n", timeout=10)
                stdout, stderr = second.communicate("release\n", timeout=10)
                self.assertEqual(first.returncode, 0)
                self.assertEqual(second.returncode, 0, stderr)
                self.assertEqual(stdout.strip(), "running")
                records = [json.loads(line) for line in
                           (primary / ".git" / verify.LOG_NAME).read_text().splitlines()]
                events = [record["event"] for record in records
                          if record["event"] != "wait"]
                self.assertEqual(events, ["start", "finish", "start", "finish"])
                for record in records:
                    if record["event"] == "finish":
                        self.assertEqual(record["exit_status"], 0)
            finally:
                for process in (first, second):
                    if process is not None:
                        if process.poll() is None:
                            process.kill()
                        process.communicate()
                git("worktree", "remove", "--force", str(linked))


if __name__ == "__main__":
    unittest.main()
