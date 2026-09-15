#!/usr/bin/env python3
"""Summarize the isolated matched prepared-publish experiment."""

import argparse
import hashlib
import json
import math
import statistics
from pathlib import Path


def stats(values):
    values = sorted(values)
    return {
        "count": len(values),
        "mean": statistics.mean(values),
        **{f"p{p}": values[math.ceil(len(values) * p / 100) - 1] for p in (50, 95, 99)},
    }


def summarize(root):
    manifest = json.loads((root / "matched/manifest.json").read_text())
    assert manifest["status"] == "passed"
    assert manifest["cold_recovery"] == "passed"
    rows = [
        json.loads(line)
        for line in (root / "matched/samples.jsonl").read_text().splitlines()
    ]
    concurrency = [row for row in rows if row.get("check") == "concurrency"]
    assert len(concurrency) == 1 and concurrency[0]["status"] == "passed"
    result = {"manifest": manifest, "concurrency": concurrency[0], "lanes": {}}
    for mode, requests in [("existing", 6), ("combined", 1)]:
        samples = sorted(
            (row for row in rows if row.get("mode") == mode),
            key=lambda row: row["round"],
        )
        assert [row["round"] for row in samples] == list(range(3, 43))
        assert all(row["requests"] == requests for row in samples)
        assert all(
            0 <= row["total_us"] - row["prepare_us"] - row["publish_us"] <= 1
            for row in samples
        )
        result["lanes"][mode] = {
            "timings_ms": {
                phase: stats([row[f"{phase}_us"] / 1000 for row in samples])
                for phase in ("prepare", "publish", "total")
            },
            "requests_per_publish": requests,
            "request_body_bytes": stats([row["request_body_bytes"] for row in samples]),
            "response_body_bytes": stats(
                [row["response_body_bytes"] for row in samples]
            ),
            "total_body_bytes": stats(
                [
                    row["request_body_bytes"] + row["response_body_bytes"]
                    for row in samples
                ]
            ),
            "samples": samples,
        }
    existing = result["lanes"]["existing"]
    combined = result["lanes"]["combined"]
    result["mean_total_reduction_percent"] = 100 * (
        1
        - combined["timings_ms"]["total"]["mean"]
        / existing["timings_ms"]["total"]["mean"]
    )
    result["mean_body_byte_reduction_percent"] = 100 * (
        1 - combined["total_body_bytes"]["mean"] / existing["total_body_bytes"]["mean"]
    )
    result["paired_total_difference_ms"] = stats(
        [
            (before["total_us"] - after["total_us"]) / 1000
            for before, after in zip(
                existing["samples"], combined["samples"], strict=True
            )
        ]
    )
    paths = [
        "build.json",
        "matched/manifest.json",
        "matched/samples.jsonl",
        "matched/server-events.jsonl",
        "matched/cold-recovery.jsonl",
    ]
    result["sources"] = {
        str(root / name): hashlib.sha256((root / name).read_bytes()).hexdigest()
        for name in paths
    }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(summarize(args.root), indent=2) + "\n")


if __name__ == "__main__":
    main()
