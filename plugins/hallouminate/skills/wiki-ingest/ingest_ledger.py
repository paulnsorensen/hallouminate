"""Compute a wiki-ingest source id and look it up in the log.md ledger."""

from __future__ import annotations

import hashlib
from pathlib import Path

import fromargs

app = fromargs.App(
    "ingest-ledger",
    help="Hash a wiki-ingest source and check the log.md ledger for it.",
)

SOURCE_ID_LENGTH = 16
FIELD_SEPARATOR = "·"


def read_text(path: Path, role: str) -> str:
    try:
        return path.read_text(encoding="utf-8-sig")
    except (OSError, UnicodeError) as exc:
        raise fromargs.contract_error(exc, context=f"cannot read {role} {path}") from exc


def normalize(text: str) -> str:
    return " ".join(text.split())


def row_source_id(row: str) -> str | None:
    fields = row.split(FIELD_SEPARATOR)
    if len(fields) < 3:
        return None
    return fields[1].strip()


@app.command
def check(source: Path, *, ledger: Path | None = None) -> dict[str, str | int | None]:
    """Hash the SOURCE file and report whether the LEDGER file already logs it."""
    normalized = normalize(read_text(source, "source"))
    if not normalized:
        raise fromargs.CliError("source is empty after whitespace normalization")
    source_id = hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:SOURCE_ID_LENGTH]

    if ledger is None or not ledger.exists():
        return {"source_id": source_id, "ledger": "absent", "matches": 0, "first_match": None}

    rows = [
        row.strip()
        for row in read_text(ledger, "ledger").splitlines()
        if row_source_id(row) == source_id
    ]
    return {
        "source_id": source_id,
        "ledger": "hit" if rows else "miss",
        "matches": len(rows),
        "first_match": rows[0] if rows else None,
    }


def main() -> int:
    return app.run()


if __name__ == "__main__":
    raise SystemExit(main())
