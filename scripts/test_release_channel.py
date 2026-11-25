import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
TARGET = "x86_64-unknown-linux-gnu"
OTHER = "aarch64-unknown-linux-gnu"


def guard_script() -> str:
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index("      - name: Guard release channel")
    run = lines.index("        run: |", start) + 1
    assert lines[start:run].count("          HALLOUMINATE_GUARD_TARGET: ${{ join(matrix.targets, ' ') }}") == 1
    end = next(
        index for index in range(run, len(lines))
        if lines[index].startswith("      - ")
    )
    script = "\n".join(line[10:] for line in lines[run:end])
    return script


def archive(
    root: Path, target: str, channel: str, binary_names: tuple[str, ...] = ("hallouminate",)
) -> Path:
    path = root / "target/distrib" / f"hallouminate-{target}.tar.xz"
    path.parent.mkdir(parents=True, exist_ok=True)
    binary = (
        "#!/bin/sh\n"
        f"printf '%s\\n' '{target}' >> \"$INVOCATIONS\"\n"
        f"printf 'hallouminate 1.0.0 ({channel})\\n'\n"
    ).encode()
    with tarfile.open(path, "w:xz") as tar:
        for name in binary_names:
            info = tarfile.TarInfo(f"hallouminate-{target}/{name}")
            info.mode = 0o755
            info.size = len(binary)
            tar.addfile(info, io.BytesIO(binary))
    return path


class ReleaseChannelGuardTests(unittest.TestCase):
    def run_guard(self, root: Path, targets: str = TARGET) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        marker = root / "invocations"
        result = subprocess.run(
            ["bash", "-c", guard_script()],
            cwd=root,
            env={**os.environ, "INVOCATIONS": str(marker), "HALLOUMINATE_GUARD_TARGET": targets},
            capture_output=True,
            text=True,
        )
        calls = marker.read_text().splitlines() if marker.exists() else []
        return result, calls

    def test_uses_only_own_archive_in_mixed_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _ = archive(root, OTHER, "stable")
            _ = archive(root, TARGET, "dev")
            result, calls = self.run_guard(root)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(calls, [TARGET])
            self.assertIn("expected the stable channel", result.stdout)

    def test_accepts_own_stable_archive_in_mixed_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _ = archive(root, OTHER, "dev")
            _ = archive(root, TARGET, "stable")
            result, calls = self.run_guard(root)
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertEqual(calls, [TARGET])
            self.assertIn("hallouminate 1.0.0 (stable)", result.stdout)

    def test_missing_or_corrupt_own_archive_fails_without_fallback(self) -> None:
        for corrupt in (False, True):
            with self.subTest(corrupt=corrupt), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _ = archive(root, OTHER, "stable")
                if corrupt:
                    own = root / "target/distrib" / f"hallouminate-{TARGET}.tar.xz"
                    _ = own.write_bytes(b"not an xz archive")
                result, calls = self.run_guard(root)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual(calls, [])

    def test_multiple_targets_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _ = archive(root, OTHER, "stable")
            _ = archive(root, TARGET, "stable")
            result, calls = self.run_guard(root, f"{TARGET} {OTHER}")
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(calls, [])

    def test_invalid_target_cannot_run_a_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _ = archive(root, TARGET, "stable")
            result, calls = self.run_guard(root, f"{TARGET}; $(touch injected)")
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(calls, [])
            self.assertFalse((root / "injected").exists())

    def test_missing_or_ambiguous_binary_fails_closed(self) -> None:
        for binary_names in ((), ("hallouminate", "nested/hallouminate")):
            with self.subTest(binary_names=binary_names), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _ = archive(root, TARGET, "stable", binary_names)
                result, calls = self.run_guard(root)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual(calls, [])
                self.assertIn("expected one hallouminate binary", result.stderr)


if __name__ == "__main__":
    _ = unittest.main()
