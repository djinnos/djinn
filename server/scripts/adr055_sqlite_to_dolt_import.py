#!/usr/bin/env python3
"""ADR-055 SQLite -> MySQL/Dolt migration export and verification helper.

This tool focuses on reproducible, inspectable migration validation for the
core note/task/session relational state. It does not attempt to package a full
production cutover. Instead it:

1. exports selected SQLite tables into TSV files suitable for MySQL/Dolt import,
2. records expected row counts and file hashes in a manifest,
3. generates import SQL for both commit and rollback-backed dry-run modes, and
4. optionally executes the rollback-backed validation flow through the `mysql`
   CLI and fails loudly on row-count mismatches.

The generated SQL targets the staged ADR-055 schema in
`server/crates/djinn-db/sql/mysql_schema.sql`.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
MYSQL_SCHEMA_PATH = ROOT / "crates" / "djinn-db" / "sql" / "mysql_schema.sql"
DEFAULT_OUTPUT_DIR = ROOT / "tmp" / "adr055-migration-artifacts"

# Parent-first load order. Delete order is the reverse of this list.
TABLES: list[str] = [
    "projects",
    "tasks",
    "task_blockers",
    "task_activity_log",
    "notes",
    "note_links",
    "sessions",
    "task_memory_refs",
    "epic_memory_refs",
    "session_messages",
    "note_associations",
    "consolidated_note_provenance",
    "consolidation_run_metrics",
]


@dataclass(frozen=True)
class MysqlCliConfig:
    database: str
    defaults_file: str | None
    host: str | None
    port: int | None
    user: str | None
    socket: str | None
    command: str
    password_env: str | None


class MigrationError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build ADR-055 SQLite export/import artifacts and optionally validate them against MySQL/Dolt."
    )
    parser.add_argument("--sqlite", required=True, help="Path to the source SQLite database.")
    parser.add_argument(
        "--output-dir",
        default=str(DEFAULT_OUTPUT_DIR),
        help=f"Directory for generated exports and SQL (default: {DEFAULT_OUTPUT_DIR}).",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Delete and recreate the output directory if it already exists.",
    )
    parser.add_argument(
        "--initialize-schema",
        action="store_true",
        help="Prepend mysql_schema.sql to generated import scripts. Use only against an empty scratch database.",
    )
    parser.add_argument(
        "--validate-live",
        action="store_true",
        help="Execute the generated rollback-backed import validation via the mysql CLI and fail on row-count mismatch.",
    )
    parser.add_argument(
        "--mysql-command",
        default="mysql",
        help="mysql-compatible client command to use for live validation (default: mysql).",
    )
    parser.add_argument("--mysql-database", help="Target MySQL/Dolt database name for --validate-live.")
    parser.add_argument("--mysql-defaults-file", help="Optional mysql defaults file for auth/SSL settings.")
    parser.add_argument("--mysql-host", help="Optional mysql host.")
    parser.add_argument("--mysql-port", type=int, help="Optional mysql port.")
    parser.add_argument("--mysql-user", help="Optional mysql user.")
    parser.add_argument("--mysql-socket", help="Optional mysql unix socket.")
    parser.add_argument(
        "--mysql-password-env",
        default="MYSQL_PWD",
        help="Environment variable that already contains the MySQL password (default: MYSQL_PWD).",
    )
    return parser.parse_args()


def ensure_paths(sqlite_path: Path, output_dir: Path, force: bool) -> None:
    if not sqlite_path.exists():
        raise MigrationError(f"SQLite database not found: {sqlite_path}")
    if not MYSQL_SCHEMA_PATH.exists():
        raise MigrationError(f"MySQL schema snapshot not found: {MYSQL_SCHEMA_PATH}")
    if output_dir.exists():
        if not force:
            raise MigrationError(
                f"Output directory already exists: {output_dir}. Pass --force to replace it."
            )
        shutil.rmtree(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "exports").mkdir(parents=True, exist_ok=True)


def table_columns(conn: sqlite3.Connection, table: str) -> list[str]:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    if not rows:
        raise MigrationError(f"Table {table!r} not found in SQLite source.")
    return [str(row[1]) for row in rows]


def sql_literal(value: str) -> str:
    return "'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'"


def normalize_cell(value: Any) -> str:
    if value is None:
        return r"\N"
    if isinstance(value, bytes):
        raise MigrationError(
            "Binary columns are not part of this ADR-055 core-state export helper; "
            "encountered an unexpected BLOB value."
        )
    if isinstance(value, bool):
        return "1" if value else "0"
    return str(value)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def export_tables(sqlite_path: Path, output_dir: Path) -> dict[str, Any]:
    conn = sqlite3.connect(sqlite_path)
    conn.row_factory = sqlite3.Row
    manifest_tables: list[dict[str, Any]] = []

    try:
        for table in TABLES:
            columns = table_columns(conn, table)
            export_path = output_dir / "exports" / f"{table}.tsv"
            query = f"SELECT {', '.join(columns)} FROM {table}"
            count = 0

            with export_path.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.writer(
                    handle,
                    delimiter="\t",
                    lineterminator="\n",
                    quoting=csv.QUOTE_MINIMAL,
                    escapechar="\\",
                )
                for row in conn.execute(query):
                    writer.writerow([normalize_cell(row[column]) for column in columns])
                    count += 1

            manifest_tables.append(
                {
                    "table": table,
                    "columns": columns,
                    "row_count": count,
                    "export_file": str(export_path.relative_to(output_dir)),
                    "sha256": sha256_file(export_path),
                }
            )
    finally:
        conn.close()

    manifest = {
        "source_sqlite": str(sqlite_path.resolve()),
        "mysql_schema": str(MYSQL_SCHEMA_PATH.resolve()),
        "tables": manifest_tables,
        "generated_by": "server/scripts/adr055_sqlite_to_dolt_import.py",
    }
    return manifest


def build_import_sql(manifest: dict[str, Any], output_dir: Path, *, initialize_schema: bool, rollback: bool) -> str:
    lines: list[str] = [
        "-- Generated by ADR-055 SQLite -> MySQL/Dolt migration helper.",
        "-- This script is intended for validation and auditability, not unattended production cutover.",
        "SET NAMES utf8mb4;",
        "SET FOREIGN_KEY_CHECKS = 0;",
    ]
    if initialize_schema:
        lines.extend(
            [
                "",
                "-- Initialize the staged ADR-055 schema on an empty scratch database.",
                MYSQL_SCHEMA_PATH.read_text(encoding="utf-8").rstrip(),
            ]
        )
    lines.extend(
        [
            "",
            "START TRANSACTION;",
            "",
            "-- Clear imported tables in child-first order so the load is reproducible.",
        ]
    )
    for table in reversed(TABLES):
        lines.append(f"DELETE FROM `{table}`;")

    lines.extend(["", "-- Load exports in parent-first order."])
    for entry in manifest["tables"]:
        export_path = (output_dir / entry["export_file"]).resolve()
        columns = ", ".join(f"`{column}`" for column in entry["columns"])
        lines.extend(
            [
                (
                    "LOAD DATA LOCAL INFILE "
                    f"{sql_literal(str(export_path))} INTO TABLE `{entry['table']}` "
                    "CHARACTER SET utf8mb4 "
                    "FIELDS TERMINATED BY '\\t' OPTIONALLY ENCLOSED BY '\"' ESCAPED BY '\\\\' "
                    "LINES TERMINATED BY '\\n' "
                    f"({columns});"
                ),
            ]
        )

    lines.extend(["", "-- Emit machine-readable counts for verifier parsing."])
    for entry in manifest["tables"]:
        expected = int(entry["row_count"])
        table = entry["table"]
        lines.append(
            f"SELECT 'VERIFY_COUNT', '{table}', {expected}, COUNT(*) FROM `{table}`;"
        )

    lines.extend(
        [
            "",
            ("ROLLBACK;" if rollback else "COMMIT;"),
            "SET FOREIGN_KEY_CHECKS = 1;",
            "",
        ]
    )
    return "\n".join(lines)


def build_count_sql(manifest: dict[str, Any]) -> str:
    lines = [
        "-- Generated row-count checklist for ADR-055 import verification.",
        "SET NAMES utf8mb4;",
    ]
    for entry in manifest["tables"]:
        expected = int(entry["row_count"])
        table = entry["table"]
        lines.append(
            f"SELECT '{table}' AS table_name, {expected} AS expected_rows, COUNT(*) AS actual_rows FROM `{table}`;"
        )
    lines.append("")
    return "\n".join(lines)


def build_readme(manifest: dict[str, Any], output_dir: Path, initialize_schema: bool) -> str:
    source = manifest["source_sqlite"]
    tables = "\n".join(
        f"- `{entry['table']}` — {entry['row_count']} rows — `{entry['export_file']}` — `{entry['sha256']}`"
        for entry in manifest["tables"]
    )
    schema_note = (
        "`001_import_dry_run.sql` and `002_import_commit.sql` include the staged schema snapshot from\n"
        f"`{MYSQL_SCHEMA_PATH}` because `--initialize-schema` was used."
        if initialize_schema
        else
        "Schema initialization is not embedded. Apply `server/crates/djinn-db/sql/mysql_schema.sql` to an\n"
        "empty scratch database first, or rerun the helper with `--initialize-schema`."
    )
    return f"""# ADR-055 SQLite -> MySQL/Dolt import artifacts

Generated by `server/scripts/adr055_sqlite_to_dolt_import.py`.

## Source

- SQLite database: `{source}`
- MySQL/Dolt schema snapshot: `{MYSQL_SCHEMA_PATH}`

## Exported tables and expected row counts

{tables}

## Generated files

- `001_import_dry_run.sql` — imports exported rows inside a transaction, emits `VERIFY_COUNT` markers, then rolls back.
- `002_import_commit.sql` — same import flow, but commits at the end for non-dry-run rehearsal or manual promotion.
- `003_verify_counts.sql` — simple count queries against the target schema.
- `manifest.json` — source row counts, export column order, and SHA-256 digests.

## Recommended validation workflow

1. Create an empty scratch MySQL/Dolt database.
2. {schema_note}
3. Run the dry-run script through a mysql-compatible client with `--local-infile=1` enabled.
4. Confirm every `VERIFY_COUNT` row reports identical expected and actual values.
5. If any count differs, stop: the generated helper treats that as a failed import validation.
6. Only after a clean dry-run should you consider using `002_import_commit.sql` against another disposable target.

## Rollback / safety guidance

- `001_import_dry_run.sql` ends with `ROLLBACK;`, so it validates import behavior without leaving imported rows behind.
- Keep rehearsals on a disposable scratch database or scratch Dolt branch until counts match.
- `002_import_commit.sql` is provided only so the exact same load order can be inspected or replayed after dry-run success.
- This helper intentionally avoids altering the active SQLite runtime and does not automate production cutover.
"""


def write_artifacts(manifest: dict[str, Any], output_dir: Path, initialize_schema: bool) -> None:
    (output_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    (output_dir / "001_import_dry_run.sql").write_text(
        build_import_sql(manifest, output_dir, initialize_schema=initialize_schema, rollback=True),
        encoding="utf-8",
    )
    (output_dir / "002_import_commit.sql").write_text(
        build_import_sql(manifest, output_dir, initialize_schema=initialize_schema, rollback=False),
        encoding="utf-8",
    )
    (output_dir / "003_verify_counts.sql").write_text(
        build_count_sql(manifest),
        encoding="utf-8",
    )
    (output_dir / "README.md").write_text(
        build_readme(manifest, output_dir, initialize_schema),
        encoding="utf-8",
    )


def mysql_cli_args(config: MysqlCliConfig) -> list[str]:
    args = [config.command, "--batch", "--raw", "--skip-column-names", "--local-infile=1"]
    if config.defaults_file:
        args.append(f"--defaults-extra-file={config.defaults_file}")
    if config.host:
        args.extend(["--host", config.host])
    if config.port is not None:
        args.extend(["--port", str(config.port)])
    if config.user:
        args.extend(["--user", config.user])
    if config.socket:
        args.extend(["--socket", config.socket])
    args.append(config.database)
    return args


def run_live_validation(output_dir: Path, manifest: dict[str, Any], config: MysqlCliConfig) -> None:
    if shutil.which(config.command) is None:
        raise MigrationError(
            f"mysql client command not found on PATH: {config.command}. Install it or skip --validate-live."
        )

    env = os.environ.copy()
    if config.password_env:
        password = env.get(config.password_env)
        if not password:
            raise MigrationError(
                f"Environment variable {config.password_env} is required for --validate-live but is not set."
            )
        env["MYSQL_PWD"] = password

    script_path = output_dir / "001_import_dry_run.sql"
    cmd = mysql_cli_args(config)
    with script_path.open("rb") as handle:
        result = subprocess.run(
            cmd,
            stdin=handle,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            check=False,
        )

    stdout = result.stdout.decode("utf-8", errors="replace")
    stderr = result.stderr.decode("utf-8", errors="replace")
    (output_dir / "live_validation.stdout").write_text(stdout, encoding="utf-8")
    (output_dir / "live_validation.stderr").write_text(stderr, encoding="utf-8")

    if result.returncode != 0:
        raise MigrationError(
            "mysql dry-run validation failed before count comparison. "
            f"See {output_dir / 'live_validation.stderr'} for stderr."
        )

    expected = {entry["table"]: int(entry["row_count"]) for entry in manifest["tables"]}
    actual: dict[str, int] = {}
    for line in stdout.splitlines():
        parts = line.split("\t")
        if len(parts) == 4 and parts[0] == "VERIFY_COUNT":
            _, table, expected_value, actual_value = parts
            actual[table] = int(actual_value)
            if int(expected_value) != expected.get(table):
                raise MigrationError(
                    f"Verifier output for {table} reported unexpected expected-count value {expected_value}."
                )

    missing = sorted(set(expected) - set(actual))
    if missing:
        raise MigrationError(
            "Dry-run validation did not emit VERIFY_COUNT rows for: " + ", ".join(missing)
        )

    mismatches = []
    for table in TABLES:
        if expected[table] != actual[table]:
            mismatches.append((table, expected[table], actual[table]))

    if mismatches:
        details = "; ".join(
            f"{table}: expected {expected_rows}, actual {actual_rows}"
            for table, expected_rows, actual_rows in mismatches
        )
        raise MigrationError(f"Row-count verification failed: {details}")

    print("Live dry-run validation succeeded. Row counts matched for all ADR-055 core tables.")


def build_mysql_config(args: argparse.Namespace) -> MysqlCliConfig:
    if not args.mysql_database:
        raise MigrationError("--mysql-database is required with --validate-live.")
    return MysqlCliConfig(
        database=args.mysql_database,
        defaults_file=args.mysql_defaults_file,
        host=args.mysql_host,
        port=args.mysql_port,
        user=args.mysql_user,
        socket=args.mysql_socket,
        command=args.mysql_command,
        password_env=args.mysql_password_env,
    )


def main() -> int:
    args = parse_args()
    sqlite_path = Path(args.sqlite).resolve()
    output_dir = Path(args.output_dir).resolve()

    try:
        ensure_paths(sqlite_path, output_dir, args.force)
        manifest = export_tables(sqlite_path, output_dir)
        write_artifacts(manifest, output_dir, args.initialize_schema)
        print(f"Wrote ADR-055 migration artifacts to {output_dir}")

        if args.validate_live:
            config = build_mysql_config(args)
            run_live_validation(output_dir, manifest, config)
        else:
            print("Dry-run artifacts generated only. Re-run with --validate-live to execute rollback-backed import verification.")
        return 0
    except MigrationError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
