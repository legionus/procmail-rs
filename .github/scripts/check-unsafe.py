# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

"""Check the narrowly approved project-owned unsafe Rust usage."""

import json
import pathlib
import re
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
ALLOW_PATTERN = re.compile(r"#\s*\[\s*allow\s*\(\s*unsafe_code\s*\)\s*\]")
MARKER_PATTERN = re.compile(r"// SAFETY-AUDIT: blocks=([0-9]+) expressions=([0-9]+)")
MODULE_PATTERN = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)
BLOCK_PATTERN = re.compile(r"\bunsafe\s*\{")


def fail(message: str) -> None:
    print(f"unsafe audit failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_project_expression_count(report_path: pathlib.Path) -> int:
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot read cargo-geiger JSON: {error}")

    matches = [
        entry
        for entry in report.get("packages", [])
        if entry.get("package", {}).get("id", {}).get("name") == "procmail-rs"
        and "Path" in entry.get("package", {}).get("id", {}).get("source", {})
    ]
    if len(matches) != 1:
        fail(f"expected one local procmail-rs package, found {len(matches)}")

    try:
        count = matches[0]["unsafety"]["used"]["exprs"]["unsafe_"]
    except (KeyError, TypeError):
        fail("cargo-geiger JSON has an unsupported schema")
    if not isinstance(count, int):
        fail("cargo-geiger unsafe expression count is not an integer")
    return count


def git_grep(pattern: str) -> list[tuple[pathlib.Path, int, str]]:
    result = subprocess.run(
        ["git", "grep", "-n", "-e", pattern, "--", "src"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode not in (0, 1):
        fail(f"git grep failed: {result.stderr.strip()}")

    matches = []
    for record in result.stdout.splitlines():
        path, line, text = record.split(":", 2)
        matches.append((ROOT / path, int(line), text))
    return matches


def discover_approved_modules() -> dict[pathlib.Path, tuple[int, int]]:
    markers = git_grep("SAFETY-AUDIT:")
    allowances = git_grep("allow(unsafe_code)")
    approved = {}
    matched_allowances = set()

    for marker_path, marker_line, marker_text in markers:
        if marker_path != ROOT / "src/lib.rs":
            fail(f"unsafe audit marker is outside src/lib.rs: {marker_path.relative_to(ROOT)}")

        marker = MARKER_PATTERN.fullmatch(marker_text.strip())
        if marker is None:
            fail(f"malformed unsafe audit marker at src/lib.rs:{marker_line}")

        lines = marker_path.read_text(encoding="utf-8").splitlines()
        if marker_line + 1 >= len(lines):
            fail(f"incomplete unsafe audit declaration at src/lib.rs:{marker_line}")
        if ALLOW_PATTERN.fullmatch(lines[marker_line].strip()) is None:
            fail(f"unsafe audit marker is not followed by an allowance at src/lib.rs:{marker_line}")

        module = MODULE_PATTERN.fullmatch(lines[marker_line + 1].strip())
        if module is None:
            fail(f"unsafe_code allowance is not followed by a module at src/lib.rs:{marker_line + 1}")

        source = ROOT / "src" / f"{module.group(1)}.rs"
        if not source.is_file() or source in approved:
            fail(f"unsafe audit module is missing or duplicated: {source.relative_to(ROOT)}")

        approved[source] = (int(marker.group(1)), int(marker.group(2)))
        matched_allowances.add((marker_path, marker_line + 1))

    actual_allowances = {(path, line) for path, line, _ in allowances}
    if actual_allowances != matched_allowances:
        fail("an unsafe_code allowance has no adjacent safety audit marker")
    if not approved:
        fail("no approved unsafe modules were found")
    return approved


def check_source_policy() -> dict[pathlib.Path, tuple[int, int]]:
    approved = discover_approved_modules()
    for source, (expected_blocks, _) in approved.items():
        count = len(BLOCK_PATTERN.findall(source.read_text(encoding="utf-8")))
        if count != expected_blocks:
            relative = source.relative_to(ROOT)
            fail(f"{relative} has {count} unsafe blocks; expected {expected_blocks}")
    return approved


def main() -> None:
    if len(sys.argv) != 2:
        fail(f"usage: {pathlib.Path(sys.argv[0]).name} GEIGER_JSON")

    approved = check_source_policy()
    actual = load_project_expression_count(pathlib.Path(sys.argv[1]))
    expected = sum(expressions for _, expressions in approved.values())
    if actual != expected:
        fail(f"project uses {actual} unsafe expressions; expected {expected}")

    print(
        f"unsafe audit passed: {len(approved)} approved modules, "
        f"{expected} expressions"
    )


if __name__ == "__main__":
    main()
