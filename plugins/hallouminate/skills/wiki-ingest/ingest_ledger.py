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
ENCODING = "utf-8-sig"
COMPLETION_ACTIONS = frozenset({"ingested", "skipped-duplicate-hash"})


def unreadable(exc: Exception, path: Path, role: str) -> fromargs.CliError:
    return fromargs.contract_error(exc, context=f"cannot read {role} {path}")


def read_text(path: Path, role: str) -> str:
    try:
        return path.read_text(encoding=ENCODING)
    except (OSError, UnicodeError) as exc:
        raise unreadable(exc, path, role) from exc


def read_ledger(path: Path | None) -> str | None:
    """Return the ledger text, or None when there is no ledger file."""
    if path is None:
        return None
    try:
        return path.read_text(encoding=ENCODING)
    except FileNotFoundError:
        return None
    except (OSError, UnicodeError) as exc:
        raise unreadable(exc, path, "ledger") from exc


def normalize(text: str) -> str:
    return " ".join(text.split())


def row_fields(row: str) -> tuple[str, str] | None:
    """Return the source id and action of a log row, or None for a non-row line."""
    fields = row.split(FIELD_SEPARATOR)
    if len(fields) < 3:
        return None
    return fields[1].strip(), fields[2].strip()


@app.command
def check(source: Path, *, ledger: Path | None = None) -> dict[str, str | int | None]:
    """Hash the SOURCE file and report how the LEDGER file logs it.

    The ledger status is `hit` when a row marks the source complete (action
    `ingested` or `skipped-duplicate-hash`), `partial` when rows log the source
    without a completion row, `miss` when no row logs it, and `absent` when
    there is no ledger file.
    """
    normalized = normalize(read_text(source, "source"))
    if not normalized:
        raise fromargs.CliError("source is empty after whitespace normalization")
    source_id = hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:SOURCE_ID_LENGTH]

    ledger_text = read_ledger(ledger)
    if ledger_text is None:
        return {"source_id": source_id, "ledger": "absent", "matches": 0, "first_match": None}

    rows: list[str] = []
    status = "miss"
    for row in ledger_text.splitlines():
        fields = row_fields(row)
        if fields is None:
            continue
        row_source_id, action = fields
        if row_source_id != source_id:
            continue
        rows.append(row.strip())
        if action in COMPLETION_ACTIONS:
            status = "hit"
        elif status == "miss":
            status = "partial"
    return {
        "source_id": source_id,
        "ledger": status,
        "matches": len(rows),
        "first_match": rows[0] if rows else None,
    }


def main() -> int:
    return app.run()


if __name__ == "__main__":
    raise SystemExit(main())
