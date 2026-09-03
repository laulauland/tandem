#!/usr/bin/env python3
"""Measure branch-local rebuilds by touching sources without changing bytes."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
COMMAND = ["cargo", "build", "-p", "jj-tandem", "--bin", "tandem", "--timings", "--message-format=json"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--record", type=Path, help="explicit path for retained JSON evidence")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT, text=True))
    names = {p["id"]: p["name"] for p in metadata["packages"]}

    def build():
        started = time.monotonic()
        result = subprocess.run(COMMAND, cwd=ROOT, text=True, capture_output=True, check=True)
        artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        rebuilt = sorted({names[a["package_id"]] for a in artifacts
                          if a.get("reason") == "compiler-artifact" and not a["fresh"] and a["package_id"] in names})
        return {"seconds": round(time.monotonic() - started, 3), "rebuilt": rebuilt}

    build()  # establish the same profile and feature selection for both probes
    probes = []
    for source, expected, forbidden in [
        ("crates/server/src/http.rs", "jj-tandem-server", {"jj-tandem-client", "jj-tandem-workspace"}),
        ("crates/client/src/backend.rs", "jj-tandem-client", {"jj-tandem-repository", "jj-tandem-server"}),
    ]:
        (ROOT / source).touch()
        result = build()
        rebuilt = set(result["rebuilt"])
        if expected not in rebuilt or rebuilt & forbidden:
            raise RuntimeError(f"cache boundary failed after touching {source}: {result}")
        probes.append({"source_touch": source, **result})
    report = {
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "command": COMMAND,
        "probes": probes,
        "verdict": "pass",
        "note": "Warm dev-profile invalidation evidence, not a cold-build speed comparison. Cargo HTML timings remain in target/cargo-timings.",
    }
    destination = args.record or Path(metadata["target_directory"]) / "benchmarks/workspace-build-cache.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"Evidence: {destination}")


if __name__ == "__main__":
    main()
