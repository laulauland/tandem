#!/usr/bin/env python3
"""Correlate R2 connector attempts with the final 40 warm publishes."""

import argparse
import hashlib
import json
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

from profile_snapshot_latency import events, stats


def analyze(root):
    root = Path(root)
    path = root / "warm/server-events.jsonl"
    rows = list(events(path))
    responses = [
        e
        for e in rows
        if e["fields"].get("message") == "host HTTP response ready"
        and e["fields"].get("request_path") == "/api/heads"
        and e["fields"].get("http_method") == "POST"
    ]
    assert len(responses) >= 43
    responses = responses[-40:]
    attempts = [
        e for e in rows if e["fields"].get("message") == "S3 HTTP attempt finished"
    ]
    starts = [
        e for e in rows if e["fields"].get("message") == "S3 HTTP attempt started"
    ]
    assert Counter(
        (e["fields"]["call_id"], e["fields"]["attempt"]) for e in attempts
    ) == Counter((e["fields"]["call_id"], e["fields"]["attempt"]) for e in starts)
    sockets = set()
    for e in attempts:
        f = e["fields"]
        pair = (f.get("local_socket"), f.get("remote_socket"))
        f["socket_previously_observed"] = pair in sockets if all(pair) else None
        if all(pair):
            sockets.add(pair)
    samples = []
    for response in responses:
        end = (
            datetime.fromisoformat(
                response["timestamp"].replace("Z", "+00:00")
            ).timestamp()
            * 1_000_000
        )
        start = end - response["fields"]["profile_host_request_us"]
        selected = [
            e
            for e in attempts
            if start <= e["fields"]["start_epoch_us"]
            and e["fields"]["end_epoch_us"] <= end
        ]
        writes = []
        for key_class in ("wal", "index"):
            selected_writes = [
                e
                for e in selected
                if any(
                    s.get("key_class") == key_class
                    for s in [e.get("span", {}), *e.get("spans", [])]
                )
            ]
            puts = [e for e in selected_writes if e["fields"]["method"] == "PUT"]
            assert puts and len({e["fields"]["call_id"] for e in puts}) == 1, (
                "expected one logical PUT per key class"
            )
            assert [e["fields"]["attempt"] for e in puts] == list(
                range(1, len(puts) + 1)
            )
            attempts_out = []
            for e in selected_writes:
                f = dict(e["fields"])
                f.pop("message")
                f["start_utc"] = datetime.fromtimestamp(
                    f["start_epoch_us"] / 1_000_000, timezone.utc
                ).isoformat()
                f["end_utc"] = datetime.fromtimestamp(
                    f["end_epoch_us"] / 1_000_000, timezone.utc
                ).isoformat()
                attempts_out.append(f)
            writes.append(
                {
                    "key_class": key_class,
                    "retry_count": len(puts) - 1,
                    "followup_heads": sum(
                        e["fields"]["method"] == "HEAD" for e in selected_writes
                    ),
                    "attempts": attempts_out,
                }
            )
        samples.append(
            {
                "host_publish_ms": response["fields"]["profile_host_request_us"] / 1000,
                "writes": writes,
                "other_http_attempts": [
                    e["fields"]
                    for e in selected
                    if not any(
                        s.get("name") == "S3 adapter write"
                        for s in [e.get("span", {}), *e.get("spans", [])]
                    )
                ],
            }
        )
    summary = {}
    for key_class in ("wal", "index"):
        writes = [
            w for s in samples for w in s["writes"] if w["key_class"] == key_class
        ]
        puts = [a for w in writes for a in w["attempts"] if a["method"] == "PUT"]
        summary[key_class] = {
            "writes": len(writes),
            "put_attempts": len(puts),
            "attempt_headers_ms": stats([a["elapsed_us"] / 1000 for a in puts]),
            "retry_count": sum(w["retry_count"] for w in writes),
            "followup_heads": sum(w["followup_heads"] for w in writes),
            "response_status_counts": dict(Counter(str(a.get("status")) for a in puts)),
            "etag_present_count": sum(a.get("etag_present") is True for a in puts),
            "previously_observed_socket_count": sum(
                a["socket_previously_observed"] is True for a in puts
            ),
            "socket_information_missing": sum(
                a["socket_previously_observed"] is None for a in puts
            ),
            "retry_gap_ms": stats(
                [a["inter_attempt_us"] / 1000 for a in puts if a["attempt"] > 1]
            ),
        }
    report = next((root / "warm/reports").glob("*.json"))
    elapsed = json.loads(report.read_text())["snapshot_publish"]["samples_ms"]
    assert len(elapsed) == 40
    return {
        "schema_version": 1,
        "snapshot_ms": stats(elapsed),
        "summary": summary,
        "samples": samples,
        "coverage": [
            "The connector observes SDK transport invocations, ending at response headers or transport error.",
            "Inter-attempt gaps include SDK handling, signing, backoff and scheduling; the exact SDK sleep decision is not separately exposed.",
            "Previous outcome is the preceding response status or transport-error kind, not a captured SDK retry decision.",
            "Repeated local/remote socket pairs are evidence consistent with connection reuse. DNS/TCP/TLS durations and lower-transport transparent replays are not exposed.",
            "Network transit versus R2 service execution remains unresolved for an individual attempt.",
        ],
        "sources": [
            {"path": str(p), "sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
            for p in (path, report, root / "build.json", Path(__file__).resolve())
        ],
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    Path(args.output).write_text(json.dumps(analyze(args.evidence), indent=2) + "\n")
