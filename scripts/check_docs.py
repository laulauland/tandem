#!/usr/bin/env python3
"""Check repository documentation links, paths, and retired terminology."""

from __future__ import annotations

import glob
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote


ROOT = Path(__file__).resolve().parent.parent

ACTIVE_DOCS = {
    ROOT / "README.md",
    ROOT / "AGENTS.md",
    ROOT / "ARCHITECTURE.md",
    ROOT / "docs/README.md",
    ROOT / "docs/reliability.md",
    ROOT / "docs/testing.md",
    ROOT / "docs/operations.md",
    ROOT / "docs/self-hosting.md",
    ROOT / "docs/images/README.md",
    ROOT / ".agents/skills/release/SKILL.md",
    ROOT / ".agents/skills/distributed-smoke/SKILL.md",
}

RETIRED_PATHS = (
    "docs/design-docs/",
    "docs/exec-plans/",
    "docs/product-specs/",
    "qa/",
)

RETIRED_TERMS = {
    "Cap'n Proto": re.compile(r"cap(?:'|’)?n proto|capnp", re.IGNORECASE),
    "line-JSON": re.compile(r"line[- ]json", re.IGNORECASE),
    "raw TCP": re.compile(r"raw tcp", re.IGNORECASE),
    "slice-number planning": re.compile(r"\bslice\s*\d+", re.IGNORECASE),
}

MARKDOWN_LINK = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
INLINE_CODE = re.compile(r"`([^`\n]+)`")
REPO_PATH_START = re.compile(
    r"^(?:\.agents/|\.github/|benches/|crates/|docs/|scripts/|skills/|src/|tests/|testing/|"
    r"AGENTS\.md$|ARCHITECTURE\.md$|Cargo\.toml$|README\.md$)"
)


def markdown_files() -> list[Path]:
    ignored = {".git", ".jj", "target"}
    return sorted(
        path
        for path in ROOT.rglob("*.md")
        if not any(part in ignored for part in path.relative_to(ROOT).parts)
    )


def local_target(raw: str) -> str | None:
    raw = raw.strip()
    if raw.startswith("<") and raw.endswith(">"):
        raw = raw[1:-1]
    else:
        raw = raw.split(maxsplit=1)[0]
    if re.match(r"^(?:[a-z][a-z0-9+.-]*:|#)", raw, re.IGNORECASE):
        return None
    return unquote(raw.split("#", 1)[0].split("?", 1)[0])


def check_links(errors: list[str]) -> None:
    for document in markdown_files():
        for match in MARKDOWN_LINK.finditer(document.read_text()):
            target = local_target(match.group(1))
            if not target:
                continue
            resolved = (document.parent / target).resolve()
            if not resolved.exists():
                errors.append(
                    f"{document.relative_to(ROOT)}: broken local link {match.group(1)!r}"
                )


def path_exists(document: Path, token: str) -> bool:
    token = token.rstrip(".,;:")
    if "::" in token:
        token = token.split("::", 1)[0]
    candidates = [ROOT / token, document.parent / token]
    if any(char in token for char in "*?["):
        return any(glob.glob(str(candidate)) for candidate in candidates)
    return any(candidate.exists() for candidate in candidates)


def check_inline_paths(errors: list[str]) -> None:
    for document in ACTIVE_DOCS:
        if not document.exists():
            errors.append(f"missing active document: {document.relative_to(ROOT)}")
            continue
        for code in INLINE_CODE.findall(document.read_text()):
            token = code.strip()
            if " " in token or not REPO_PATH_START.match(token):
                continue
            if not path_exists(document, token):
                errors.append(
                    f"{document.relative_to(ROOT)}: reference to missing repo path {token!r}"
                )


def tracked_files() -> list[Path]:
    output = subprocess.run(
        ["jj", "file", "list"], cwd=ROOT, check=True, text=True, capture_output=True
    ).stdout
    return [ROOT / line for line in output.splitlines() if line]


def check_retired_paths(errors: list[str]) -> None:
    for path in tracked_files():
        if path.resolve() == Path(__file__).resolve():
            continue
        if path.suffix not in {".md", ".rs", ".toml", ".yaml", ".yml", ".sh", ".py"}:
            continue
        text = path.read_text(errors="replace")
        for retired in RETIRED_PATHS:
            if retired in text:
                errors.append(
                    f"{path.relative_to(ROOT)}: references deleted path prefix {retired!r}"
                )


def check_retired_terms(errors: list[str]) -> None:
    for document in ACTIVE_DOCS:
        if not document.exists():
            continue
        text = document.read_text()
        for label, pattern in RETIRED_TERMS.items():
            if pattern.search(text):
                errors.append(
                    f"{document.relative_to(ROOT)}: retired active-doc terminology: {label}"
                )


def main() -> int:
    errors: list[str] = []

    check_links(errors)
    check_inline_paths(errors)
    check_retired_paths(errors)
    check_retired_terms(errors)

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(f"documentation checks passed ({len(markdown_files())} Markdown files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
