#!/usr/bin/env python3
"""Attribute traced snapshots without summing nested timing scopes.

Input is retained benchmark evidence, never a live service. Pairing requires
one publisher per repository, complete traces, and matching RPC sequences.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
from collections import Counter, defaultdict
from datetime import datetime
from itertools import pairwise
from pathlib import Path


def events(path):
    for line in Path(path).read_text().splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict) and "fields" in event:
            yield event


def stats(values):
    ordered = sorted(values)
    if not ordered:
        return {"count": 0}
    return {
        "count": len(ordered),
        "mean": sum(ordered) / len(ordered),
        **{
            f"p{p}": ordered[max(0, math.ceil(len(ordered) * p / 100) - 1)]
            for p in (50, 95, 99)
        },
        "min": ordered[0],
        "max": ordered[-1],
    }


def client_snapshots(path):
    pending, complete = {}, defaultdict(list)
    route_workspaces = {}
    for event in events(path):
        workspace = event.get("span", {}).get("workspace")
        fields = event["fields"]
        if "profile_http_headers_us" in fields:
            repository_route = fields["request_path"].split("/api/", 1)[0]
            if workspace:
                route_workspaces[repository_route] = workspace
            else:
                # jj may issue object requests on threads without the outer
                # snapshot span. Their repository URL still identifies the
                # one active writer in this measurement.
                workspace = route_workspaces.get(repository_route)
        if workspace is None:
            continue
        if "profile_http_headers_us" in fields and fields["request_path"].endswith(
            "/writer"
        ):
            pending[workspace] = []
        if workspace not in pending:
            continue
        pending[workspace].append(event)
        if fields.get("profile_phase") == "working_copy_finish":
            complete[workspace].append(pending.pop(workspace))
    return complete


def server_publishes(path):
    pending, blocks = defaultdict(list), defaultdict(list)
    result = defaultdict(list)
    last_response = {}
    for event in events(path):
        fields = event["fields"]
        repository = fields.get("repository") or event.get("span", {}).get("repository")
        if not repository:
            continue
        pending[repository].append(event)
        if "profile_host_request_us" not in fields:
            continue
        ended = datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00"))
        if repository in last_response:
            gap_us = (ended - last_response[repository]).total_seconds() * 1_000_000
            fields["overlap_with_prior_response_us"] = max(
                0, fields["profile_host_request_us"] - gap_us
            )
        last_response[repository] = ended
        request = {"events": pending.pop(repository), "response": fields}
        # A claim starts each snapshot. Earlier setup or unchanged attempts
        # are outside this successful snapshot's timer.
        if fields["request_path"].endswith("/writer"):
            blocks[repository] = []
        blocks[repository].append(request)
        if fields["request_path"] == "/api/heads" and fields["http_method"] == "POST":
            responses = [
                e["fields"]
                for e in request["events"]
                if e["fields"].get("rpc_method") == "updateOpHeads"
                and e["fields"].get("message") == "rpc response"
            ]
            if not responses:
                raise ValueError("head request lacks publish result")
            response = responses[-1]
            # Keep failed CAS attempts in the same block as their successful retry.
            if response["ok"]:
                result[repository].append(
                    {
                        "operation_id": response["new_id"],
                        "requests": blocks.pop(repository),
                    }
                )
    return result


def attribute(client, server, elapsed_ms):
    http_events = [e for e in client if "profile_http_headers_us" in e["fields"]]
    http = [e["fields"] for e in http_events]
    for previous, current in pairwise(http_events):
        time_of = lambda e: datetime.fromisoformat(
            e["timestamp"].replace("Z", "+00:00")
        )
        gap_us = (time_of(current) - time_of(previous)).total_seconds() * 1_000_000
        assert gap_us + 1000 >= current["fields"]["profile_http_headers_us"], (
            "overlapping client HTTP calls; additive attribution requires interval unions"
        )
    requests = server["requests"]
    assert all(
        r["response"].get("overlap_with_prior_response_us", 0) <= 1000 for r in requests
    ), "overlapping requests in repository; attribution requires request identities"
    assert len(http) == len(requests), (
        "request count mismatch",
        len(http),
        len(requests),
    )
    assert all(
        c["request_path"].split("/api/", 1)[-1]
        == s["response"]["request_path"].split("/api/", 1)[-1]
        for c, s in zip(http, requests)
    ), "RPC sequence mismatch"
    phases, phase_http = {}, {}
    waiting = 0
    for event in client:
        f = event["fields"]
        waiting += f.get("profile_http_headers_us", 0) / 1000
        if "profile_phase" in f:
            phases[f["profile_phase"]] = f["duration_us"] / 1000
            phase_http[f["profile_phase"]] = waiting
            waiting = 0
    assert len(phases) == 6 and waiting == 0, "incomplete snapshot phase trace"
    timer_gap = elapsed_ms - sum(phases.values())
    assert timer_gap >= -0.01, "snapshot timer is shorter than its recorded phases"
    host_events = [e["fields"] for request in requests for e in request["events"]]
    assert any("lock_wait_us" in f for f in host_events), "missing lock-wait coverage"
    assert any("lock_hold_us" in f for f in host_events), "missing lock-hold coverage"
    assert any(
        "admission_wait_ms" in f
        and "queue_depth" in f
        and f.get("message") == "rpc request"
        for f in host_events
    ), "missing publish-admission coverage"
    assert any(
        "cas_retries" in e["fields"]
        and e["fields"].get("message") == "op-head update succeeded"
        for e in client
    ), "missing client retry coverage"
    host_phases = defaultdict(float)
    for f in host_events:
        if f.get("message") == "host publish phase":
            host_phases[f["profile_phase"]] += f["duration_us"] / 1000
    assert len(host_phases) == 5, "incomplete publish phase trace"
    bucket = [f for f in host_events if "bucket_elapsed_us" in f]
    bucket_ms = sum(f["bucket_elapsed_us"] / 1000 for f in bucket)
    host_ms = sum(r["response"]["profile_host_request_us"] / 1000 for r in requests)
    http_ms = sum(c["profile_http_headers_us"] / 1000 for c in http)
    # These three terms are disjoint by subtraction. The two residuals are
    # unlocalized wall time, not claimed to be pure local CPU or pure network.
    local_residual = elapsed_ms - http_ms
    http_residual = http_ms - host_ms
    host_other = host_ms - bucket_ms
    assert min(local_residual, http_residual, host_other) > -0.01, (
        "negative attribution residual"
    )
    request_metrics = []
    for c, s in zip(http, requests):
        f = s["response"]
        path = re.sub(
            r"/workspaces/[^/]+/writer$",
            "/workspaces/{workspace}/writer",
            f["request_path"],
        )
        path = re.sub(r"/(ops|views)/[0-9a-f]+$", r"/\1/{id}", path)
        path = re.sub(
            r"/objects/(file|tree|commit)/[0-9a-f]+$", r"/objects/\1/{id}", path
        )
        label = f["http_method"] + " " + path
        request_metrics.append(
            {
                "route": label,
                "client_headers_ms": c["profile_http_headers_us"] / 1000,
                "host_ms": f["profile_host_request_us"] / 1000,
                "outside_host_ms": c["profile_http_headers_us"] / 1000
                - f["profile_host_request_us"] / 1000,
                "request_body_bytes": f["request_bytes"],
                "response_body_bytes": f.get("response_bytes"),
            }
        )
    assert all(r["response_body_bytes"] is not None for r in request_metrics), (
        "unmeasured response body length"
    )
    last_request = requests[-1]
    index_events = [
        e
        for e in last_request["events"]
        if e["fields"].get("bucket_key_class") == "index"
        and e["fields"].get("bucket_operation") == "compare_and_put"
    ]
    assert len(index_events) == 1, "index commit is not uniquely attributable"
    time_of = lambda e: datetime.fromisoformat(e["timestamp"].replace("Z", "+00:00"))
    ack_ready_ms = (
        time_of(last_request["events"][-1]) - time_of(index_events[0])
    ).total_seconds() * 1000
    sample = {
        "operation_id": server["operation_id"],
        "total_ms": elapsed_ms,
        "post_phase_timer_gap_ms": timer_gap,
        "workspace_phases_ms": phases,
        "http_within_workspace_phase_ms": phase_http,
        "workspace_phase_residual_ms": {k: phases[k] - phase_http[k] for k in phases},
        "client_http_headers_ms": http_ms,
        "host_ms": host_ms,
        "bucket_ms": bucket_ms,
        "host_non_bucket_ms": host_other,
        "local_residual_ms": local_residual,
        "http_outside_host_ms": http_residual,
        "unlocalized_ms": local_residual + http_residual,
        "host_publish_phases_ms": host_phases,
        "wal_encode_ms": sum(
            f["duration_us"] / 1000
            for f in host_events
            if f.get("profile_phase") == "wal_encode"
        ),
        "dispatch_ms": sum(
            r["response"]["profile_dispatch_us"] / 1000 for r in requests
        ),
        "index_to_response_ready_ms": ack_ready_ms,
        "lock_wait_ms": sum(f.get("lock_wait_us", 0) / 1000 for f in host_events),
        "lock_hold_ms": sum(f.get("lock_hold_us", 0) / 1000 for f in host_events),
        "publish_lock_hold_ms": sum(
            f.get("lock_hold_us", 0) / 1000
            for f in host_events
            if f.get("lock_operation") == "update_heads"
        ),
        "publish_admission_wait_ms": sum(
            f.get("admission_wait_ms", 0)
            for f in host_events
            if f.get("message") == "rpc request"
        ),
        "max_repository_publish_queue_depth": max(
            [f.get("queue_depth", 0) for f in host_events] + [0]
        ),
        "client_cas_retries": sum(
            e["fields"].get("cas_retries", 0)
            for e in client
            if e["fields"].get("message") == "op-head update succeeded"
        ),
        "bucket_cas_conflicts": sum(
            f.get("message") == "index object CAS conflict" for f in host_events
        ),
        "http_requests": len(http),
        "http_request_body_bytes": sum(
            r["request_body_bytes"] for r in request_metrics
        ),
        "http_response_body_bytes": sum(
            r["response_body_bytes"] for r in request_metrics
        ),
        "bucket_calls": len(bucket),
        "bucket_read_payload_bytes": sum(f["bucket_read_bytes"] for f in bucket),
        "bucket_attempted_write_payload_bytes": sum(
            f["bucket_write_bytes"] for f in bucket
        ),
        "bucket_operations": [
            {
                "operation": f["bucket_operation"],
                "key_class": f["bucket_key_class"],
                "ms": f["bucket_elapsed_us"] / 1000,
                "read_bytes": f["bucket_read_bytes"],
                "attempted_write_bytes": f["bucket_write_bytes"],
            }
            for f in bucket
        ],
        "requests": request_metrics,
    }
    # One additive budget, explicitly excluding nested explanatory tables.
    assert (
        abs(elapsed_ms - (bucket_ms + host_other + http_residual + local_residual))
        < 1e-6
    )
    return sample


def summary(samples):
    result = {"samples": len(samples)}
    for key, value in samples[0].items():
        if isinstance(value, (int, float)):
            result[key] = stats([s[key] for s in samples])
        elif isinstance(value, dict):
            result[key] = {k: stats([s[key][k] for s in samples]) for k in value}
    routes = defaultdict(list)
    operations = defaultdict(list)
    for sample in samples:
        for r in sample["requests"]:
            routes[r["route"]].append(r)
        for b in sample["bucket_operations"]:
            operations[b["operation"] + " " + b["key_class"]].append(b)
    result["http_routes"] = {
        route: {k: stats([r[k] for r in rs]) for k in rs[0] if k != "route"}
        for route, rs in routes.items()
    }
    result["bucket_operations"] = {
        op: {
            k: stats([r[k] for r in rs])
            for k in ("ms", "read_bytes", "attempted_write_bytes")
        }
        for op, rs in operations.items()
    }
    result["mean_additive_budget_ms"] = {
        k: result[k]["mean"]
        for k in (
            "bucket_ms",
            "host_non_bucket_ms",
            "http_outside_host_ms",
            "local_residual_ms",
        )
    }
    result["unlocalized_percent_of_total_mean"] = (
        100 * result["unlocalized_ms"]["mean"] / result["total_ms"]["mean"]
    )
    return result


def overlap(intervals):
    """Sweep host-clock intervals; touching endpoints do not overlap."""
    edges = sorted(
        (point, delta)
        for start, end in intervals
        for point, delta in ((start, 1), (end, -1))
    )
    active = maximum = 0
    union = concurrent = weighted = 0.0
    previous = edges[0][0] if edges else 0
    for point, delta in edges:
        elapsed = point - previous
        if active:
            union += elapsed
            weighted += elapsed * active
        if active > 1:
            concurrent += elapsed
        active += delta
        maximum = max(maximum, active)
        previous = point
    return {
        "maximum_concurrent": maximum,
        "active_union_ms": union,
        "overlap_ms": concurrent,
        "mean_concurrent_while_active": weighted / union if union else 0,
    }


def server_overlap(publishes):
    requests, buckets = [], []
    for publish in publishes:
        for request in publish["requests"]:
            for e in request["events"]:
                fields = e["fields"]
                end = (
                    datetime.fromisoformat(
                        e["timestamp"].replace("Z", "+00:00")
                    ).timestamp()
                    * 1000
                )
                if "profile_host_request_us" in fields:
                    requests.append(
                        (end - fields["profile_host_request_us"] / 1000, end)
                    )
                if "bucket_elapsed_us" in fields:
                    buckets.append((end - fields["bucket_elapsed_us"] / 1000, end))
    return {
        "host_requests": overlap(requests),
        "bucket_calls": overlap(buckets),
        "coverage": "All successful active-profile snapshots, including the large writer; host wall timestamps plus monotonic durations. Setup, warmups and recovery excluded.",
    }


def analyze(root, include_mixed):
    root = Path(root)
    warm = root / "warm"
    clients = client_snapshots(warm / "client-events.jsonl")
    servers = server_publishes(warm / "server-events.jsonl")
    assert len(clients) == len(servers) == 1
    cg = next(iter(clients.values()))
    sg = next(iter(servers.values()))
    assert len(cg) == 43 and len(sg) >= 43
    warm_reports = list((warm / "reports").glob("*.json"))
    assert len(warm_reports) == 1
    report = json.loads(warm_reports[0].read_text())
    assert len(report["snapshot_publish"]["samples_ms"]) == 40, (
        "warm report must contain 40 samples"
    )
    samples = [
        attribute(c, s, t)
        for c, s, t in zip(
            cg[3:], sg[-43:][3:], report["snapshot_publish"]["samples_ms"]
        )
    ]
    assert len(samples) == 40
    output = {
        "schema_version": 1,
        "quantiles": "nearest rank; p99 is the maximum for n=40",
        "instrumentation_source": json.loads((root / "build.json").read_text())[
            "revision"
        ],
        "warm": {"summary": summary(samples), "samples": samples},
        "limitations": [
            "Durations are inclusive nested scopes unless listed in mean_additive_budget_ms; do not add percentiles.",
            "Local residual includes response-body drain/decoding, jj work, filesystem work and tracing overhead; it is not pure CPU.",
            "HTTP outside host includes path to/from provider proxy, client queuing, transport and unmeasured edge work; it is not a measured one-way RTT.",
            "Body and ObjectStore payload bytes exclude headers, TLS and backend-internal retries; SDK retry count is not observed.",
            "One warm run and one mixed run do not isolate contention from payload/cache-policy/environment differences.",
            "Publish admission wait is not decoded-body admission wait. Body-admission wait and host queue depth are unobserved; waits recorded as zero are below timer resolution.",
            "Bucket write lengths are attempted payload bytes, not independently confirmed stored bytes.",
            "Instrumentation overhead has not been isolated with a matched uninstrumented control.",
        ],
    }
    if include_mixed:
        mixed = root / "benchmarks/mixed"
        clients = client_snapshots(mixed / "run.log")
        servers = server_publishes(mixed / "server-events.jsonl")
        mixed_reports = list((mixed / "reports").glob("*.json"))
        assert len(mixed_reports) == 1
        report = json.loads(mixed_reports[0].read_text())
        output["mixed"] = {}
        for profile in report["active"]:
            rows = []
            large = []
            full_small = []
            published_blocks = []
            offset = 0 if profile["profile"] == "burst" else 4
            for writer in profile["writers"]:
                index = offset + writer["writer"]
                workspace = f"load-{index:02}"
                cs = clients[workspace]
                repo = next(k for k in servers if k.endswith("/" + workspace))
                by_op = {s["operation_id"]: s for s in servers[repo]}
                published = [
                    a for a in writer["attempts"] if a["status"] == "published"
                ]
                assert all(
                    a["status"] == "published" for a in writer["attempts"][:40]
                ), (
                    "first 40 attempts include non-publishes; profile selection requires separate analysis"
                )
                assert all(
                    a["status"] in ("published", "Unchanged")
                    for a in writer["attempts"]
                ), "failed attempts require separate analysis"
                measured = cs if writer["large"] else cs[3:]
                assert len(measured) == len(published), (
                    "attempt pairing mismatch",
                    workspace,
                    len(measured),
                    len(published),
                )
                for ordinal, (c, a) in enumerate(zip(measured, published)):
                    published_blocks.append(by_op[a["operation_id"]])
                    row = attribute(c, by_op[a["operation_id"]], a["snapshot_ms"])
                    row["workspace"] = workspace
                    if writer["large"]:
                        large.append(row)
                    else:
                        full_small.append(row)
                        if ordinal < 40:
                            rows.append(row)
            assert len(rows) == 120
            output["mixed"][profile["profile"]] = {
                "summary": summary(rows),
                "samples": rows,
                "large_summary": summary(large),
                "large_samples": large,
                "workload_passed": profile["passed"],
                "attempt_status_counts": dict(
                    Counter(
                        a["status"] for w in profile["writers"] for a in w["attempts"]
                    )
                ),
                "overlap": server_overlap(published_blocks),
                "full_small_summary": summary(full_small),
                "per_small_writer": {
                    w: summary([r for r in rows if r["workspace"] == w])
                    for w in sorted({r["workspace"] for r in rows})
                },
            }
    paths = [
        warm / "client-events.jsonl",
        warm / "server-events.jsonl",
        root / "build.json",
        Path(__file__).resolve(),
        *warm_reports,
    ]
    if include_mixed:
        paths += [
            root / "benchmarks/mixed/run.log",
            root / "benchmarks/mixed/server-events.jsonl",
            *mixed_reports,
        ]
    output["sources"] = [
        {"path": str(p), "sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
        for p in paths
    ]
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence")
    parser.add_argument("--warm-only", action="store_true")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    result = analyze(args.evidence, not args.warm_only)
    Path(args.output).write_text(json.dumps(result, indent=2) + "\n")
