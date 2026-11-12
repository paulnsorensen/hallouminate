"""Black-box tests for the ingest-ledger CLI through its real entry point."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "ingest_ledger.py"
HELLO_ID = "64ec88ca00b268e5"


def run(*args: str, stdin: bytes = b"") -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), "check", *args],
        input=stdin,
        capture_output=True,
        check=False,
    )


class IngestLedgerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)

    def tearDown(self) -> None:
        self.dir.cleanup()

    def write(self, name: str, data: bytes) -> str:
        path = self.root / name
        path.write_bytes(data)
        return str(path)

    def ok(self, *args: str, stdin: bytes = b"") -> dict[str, object]:
        result = run(*args, stdin=stdin)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_whitespace_variants_hash_identically(self) -> None:
        spaced = self.write("spaced.md", b"  Hello\n\n world\t\n")
        bom = self.write("bom.md", b"\xef\xbb\xbfHello world")
        for path in (spaced, bom):
            self.assertEqual(
                self.ok(path),
                {"source_id": HELLO_ID, "ledger": "absent", "matches": 0, "first_match": None},
            )

    def test_case_and_markdown_change_the_id(self) -> None:
        lower = self.write("lower.md", b"hello world")
        bold = self.write("bold.md", b"**Hello** world")
        for path in (lower, bold):
            self.assertNotEqual(self.ok(path)["source_id"], HELLO_ID)

    def test_missing_ledger_is_absent(self) -> None:
        source = self.write("s.md", b"Hello world")
        result = self.ok(source, "--ledger", str(self.root / "log.md"))
        self.assertEqual(result["ledger"], "absent")

    def test_ledger_miss_and_exact_field_hit(self) -> None:
        source = self.write("s.md", b"Hello world")
        empty_log = self.write("log0.md", "# Ingest Log\n\n## Log\n".encode())
        self.assertEqual(
            self.ok(source, "--ledger", empty_log),
            {"source_id": HELLO_ID, "ledger": "miss", "matches": 0, "first_match": None},
        )
        first = f"2026-10-01 · {HELLO_ID} · merged · a.md · x"
        log = "\n".join(
            [
                "# Ingest Log",
                "## Log",
                f"2026-10-01 · zz{HELLO_ID} · merged · b.md · prefix only",
                first,
                f"- 2026-10-02 · {HELLO_ID} · skipped-duplicate-hash · — · y",
                f"mentions {HELLO_ID} outside the hash field",
            ]
        )
        hit_log = self.write("log1.md", log.encode())
        self.assertEqual(
            self.ok(source, "--ledger", hit_log),
            {"source_id": HELLO_ID, "ledger": "hit", "matches": 2, "first_match": first},
        )

    def test_merged_row_alone_is_partial(self) -> None:
        source = self.write("s.md", b"Hello world")
        row = f"2026-10-01 · {HELLO_ID} · merged · a.md · x"
        log = self.write("log.md", f"# Ingest Log\n\n## Log\n{row}\n".encode())
        self.assertEqual(
            self.ok(source, "--ledger", log),
            {"source_id": HELLO_ID, "ledger": "partial", "matches": 1, "first_match": row},
        )

    def test_ingested_row_is_a_hit(self) -> None:
        source = self.write("s.md", b"Hello world")
        merged = f"2026-10-01 · {HELLO_ID} · merged · a.md · x"
        ingested = f"2026-10-01 · {HELLO_ID} · ingested · — · 1 claim"
        log = self.write("log.md", f"## Log\n{merged}\n{ingested}\n".encode())
        self.assertEqual(
            self.ok(source, "--ledger", log),
            {"source_id": HELLO_ID, "ledger": "hit", "matches": 2, "first_match": merged},
        )

    def test_repeated_runs_are_identical(self) -> None:
        source = self.write("s.md", b"Hello world")
        log = self.write("log.md", f"d · {HELLO_ID} · merged · a · b".encode())
        first = run(source, "--ledger", log)
        second = run(source, "--ledger", log)
        self.assertEqual(first.stdout, second.stdout)
        self.assertEqual(json.loads(first.stdout)["source_id"], HELLO_ID)

    def test_empty_source_is_a_usage_error(self) -> None:
        result = run(self.write("empty.md", b" \n\t"))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, b"")

    def test_unreadable_inputs_are_contract_errors(self) -> None:
        bad = self.write("bad.md", b"\xff\xfe")
        source = self.write("s.md", b"Hello world")
        for args in ((bad,), (str(self.root / "missing.md"),), (source, "--ledger", str(self.root))):
            result = run(*args)
            self.assertEqual(result.returncode, 3, (args, result.stderr))
            self.assertEqual(result.stdout, b"")

    def test_symlink_loop_ledger_is_a_contract_error(self) -> None:
        source = self.write("s.md", b"Hello world")
        loop = self.root / "loop.md"
        loop.symlink_to(loop)
        result = run(source, "--ledger", str(loop))
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertEqual(result.stdout, b"")
        self.assertIn(b"cannot read ledger", result.stderr)


if __name__ == "__main__":
    unittest.main()
