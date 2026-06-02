#!/usr/bin/env python3

import argparse
import gc
import os
import sqlite3
import statistics
import time
from dataclasses import dataclass
from pathlib import Path


LOCAL_ACTION_CACHE_V4_SQL = (
    "SELECT action_digest, outputs_fingerprint, output_values "
    "FROM local_action_cache_v4"
)
LOCAL_ACTION_CACHE_V5_SQL = (
    "SELECT cache_key, action_key_digest, input_metadata_digest, "
    "action_fingerprint, outputs_fingerprint, output_values "
    "FROM local_action_cache_v5"
)
MATERIALIZER_STATE_SQL = (
    "SELECT path, artifact_type, digest_size, entry_hash, entry_hash_kind, "
    "file_is_executable, symlink_target, last_access_time, parent_path "
    "FROM materializer_state"
)


@dataclass(frozen=True)
class Case:
    name: str
    db_path: Path
    sql: str


@dataclass
class Sample:
    elapsed: float
    rows: int
    bytes_read: int


def human_bytes(size: int) -> str:
    units = ["B", "KiB", "MiB", "GiB"]
    value = float(size)
    for unit in units:
        if value < 1024.0 or unit == units[-1]:
            return f"{value:.1f}{unit}" if unit != "B" else f"{int(value)}B"
        value /= 1024.0
    raise AssertionError("unreachable")


def sqlite_uri(path: Path) -> str:
    # Open read-only in place. This intentionally does not copy DBs; WAL/SHM files
    # remain owned by the running Buck daemon if one is active.
    return f"file:{path}?mode=ro"


def value_size(value) -> int:
    if value is None:
        return 0
    if isinstance(value, (bytes, bytearray, memoryview)):
        return len(value)
    return len(str(value))


def run_case(case: Case) -> Sample:
    start = time.perf_counter()
    rows = 0
    bytes_read = 0
    with sqlite3.connect(sqlite_uri(case.db_path), uri=True) as conn:
        conn.execute("PRAGMA query_only = ON")
        conn.execute("PRAGMA busy_timeout = 30000")
        for row in conn.execute(case.sql):
            rows += 1
            bytes_read += sum(value_size(value) for value in row)
    return Sample(time.perf_counter() - start, rows, bytes_read)


def project_cases(project: Path) -> list[Case]:
    cache = project / "buck-out/v2/cache"
    local_action_cache = cache / "local_action_cache/db.sqlite"
    materializer_state = cache / "materializer_state/db.sqlite"
    return [
        Case(
            f"{project.name}:local_action_cache_v4",
            local_action_cache,
            LOCAL_ACTION_CACHE_V4_SQL,
        ),
        Case(
            f"{project.name}:local_action_cache_v5",
            local_action_cache,
            LOCAL_ACTION_CACHE_V5_SQL,
        ),
        Case(
            f"{project.name}:materializer_state",
            materializer_state,
            MATERIALIZER_STATE_SQL,
        ),
    ]


def percentile(samples: list[float], p: float) -> float:
    if not samples:
        return 0.0
    if len(samples) == 1:
        return samples[0]
    ordered = sorted(samples)
    index = round((len(ordered) - 1) * p)
    return ordered[index]


def print_results(results: dict[str, list[Sample]]) -> None:
    headers = ["case", "rows", "bytes", "median", "min", "p90", "runs"]
    rows = []
    for name, samples in results.items():
        elapsed = [sample.elapsed for sample in samples]
        first = samples[0]
        rows.append(
            [
                name,
                f"{first.rows:,}",
                human_bytes(first.bytes_read),
                f"{statistics.median(elapsed):.3f}s",
                f"{min(elapsed):.3f}s",
                f"{percentile(elapsed, 0.90):.3f}s",
                str(len(samples)),
            ]
        )

    widths = [len(header) for header in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    print("  ".join(header.ljust(widths[i]) for i, header in enumerate(headers)))
    print("  ".join("-" * width for width in widths))
    for row in rows:
        print("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Benchmark Buck2's current eager SQLite load shape for local action "
            "cache and deferred materializer state."
        )
    )
    parser.add_argument(
        "--project",
        action="append",
        type=Path,
        default=[],
        help="Project root containing buck-out/v2/cache. May be passed multiple times.",
    )
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        help=(
            "Substring filter for benchmark case names. May be passed multiple "
            "times, for example --case buildbuddy:materializer_state."
        ),
    )
    parser.add_argument("--skip-missing", action="store_true")
    args = parser.parse_args()

    projects = args.project or [
        Path.home() / "Code/buildbuddy",
        Path.home() / "Code/bazel",
    ]
    cases = []
    for project in projects:
        cases.extend(project_cases(project))
    if args.case:
        filters = tuple(args.case)
        cases = [case for case in cases if any(f in case.name for f in filters)]

    results: dict[str, list[Sample]] = {}
    for case in cases:
        if not case.db_path.exists():
            if args.skip_missing:
                continue
            raise FileNotFoundError(case.db_path)
        samples = []
        for _ in range(args.runs):
            gc.collect()
            samples.append(run_case(case))
        results[case.name] = samples

    print_results(results)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
