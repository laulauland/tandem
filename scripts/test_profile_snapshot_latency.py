"""Offline checks for attribution failures that could produce plausible bad data."""

import copy
import json
import tempfile
import unittest
from pathlib import Path

from profile_snapshot_latency import attribute, client_snapshots, overlap, stats


def event(fields, timestamp="2026-09-15T00:00:00.000000Z"):
    return {
        "timestamp": timestamp,
        "fields": fields,
        "span": {"repository": "test/repo"},
    }


def fixture():
    phases = [
        ("writer_check", 10),
        ("repo_prepare", 10),
        ("scan_and_tree", 30),
        ("commit_prepare", 10),
        ("transaction_publish", 30),
        ("working_copy_finish", 10),
    ]
    client, requests = [], []
    clock_ms = 0
    for index, (phase, duration) in enumerate(phases):
        clock_ms += duration
        if index < 5:
            path = "/api/heads" if index == 4 else f"/api/request-{index}"
            client.append(
                event(
                    {
                        "profile_http_headers_us": (duration - 1) * 1000,
                        "request_path": path,
                    },
                    f"2026-09-15T00:00:00.{clock_ms * 1000:06}Z",
                )
            )
            response = {
                "request_path": path,
                "http_method": "POST",
                "profile_host_request_us": 20000 if index == 4 else 1000,
                "profile_dispatch_us": 100,
                "request_bytes": 5,
                "response_bytes": 2,
            }
            requests.append({"response": response, "events": [event(response)]})
        client.append(event({"profile_phase": phase, "duration_us": duration * 1000}))
    host = [
        event(
            {
                "message": "host publish phase",
                "profile_phase": phase,
                "duration_us": ms * 1000,
            }
        )
        for phase, ms in [
            ("validation", 1),
            ("wal", 10),
            ("index_commit", 5),
            ("local_apply", 1),
            ("post_apply_ack_prepare", 1),
        ]
    ]
    host += [
        event(
            {
                "bucket_elapsed_us": 15000,
                "bucket_operation": "compare_and_put",
                "bucket_key_class": "index",
                "bucket_read_bytes": 0,
                "bucket_write_bytes": 100,
            }
        )
    ]
    host += [
        event({"lock_wait_us": 0, "lock_hold_us": 20000}),
        event({"message": "rpc request", "admission_wait_ms": 0, "queue_depth": 0}),
    ]
    client.append(event({"message": "op-head update succeeded", "cas_retries": 0}))
    requests[-1]["events"] = host + [
        event(requests[-1]["response"], "2026-09-15T00:00:00.001000Z")
    ]
    return client, {"operation_id": "synthetic-operation", "requests": requests}


class AttributionTests(unittest.TestCase):
    def test_worker_thread_http_is_attributed_to_active_repository(self):
        claim = event(
            {
                "request_path": "/test/repo/api/workspaces/agent/writer",
                "profile_http_headers_us": 100,
            }
        )
        claim["span"] = {"workspace": "agent"}
        upload = event(
            {
                "request_path": "/test/repo/api/objects/file",
                "profile_http_headers_us": 200,
            }
        )
        upload.pop("span")
        finish = event({"profile_phase": "working_copy_finish", "duration_us": 1})
        finish["span"] = {"workspace": "agent"}
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "trace.jsonl"
            path.write_text("\n".join(json.dumps(e) for e in (claim, upload, finish)))
            snapshots = client_snapshots(path)
        self.assertEqual(len(snapshots["agent"]), 1)
        self.assertEqual(
            sum(
                "profile_http_headers_us" in e["fields"] for e in snapshots["agent"][0]
            ),
            2,
        )

    def test_nested_scopes_are_not_added_twice(self):
        client, server = fixture()
        sample = attribute(client, server, 100)
        self.assertEqual(
            [
                sample[k]
                for k in (
                    "bucket_ms",
                    "host_non_bucket_ms",
                    "http_outside_host_ms",
                    "local_residual_ms",
                )
            ],
            [15, 9, 61, 15],
        )
        self.assertEqual(sample["index_to_response_ready_ms"], 1)
        self.assertEqual(sample["workspace_phase_residual_ms"]["scan_and_tree"], 1)

    def test_missing_rpc_is_rejected(self):
        client, server = fixture()
        server["requests"].pop(0)
        with self.assertRaisesRegex(AssertionError, "request count mismatch"):
            attribute(client, server, 100)

    def test_absent_wait_metric_is_not_reported_as_zero(self):
        client, server = fixture()
        for e in server["requests"][-1]["events"]:
            e["fields"].pop("lock_wait_us", None)
        with self.assertRaisesRegex(AssertionError, "missing lock-wait coverage"):
            attribute(client, server, 100)

    def test_wrong_snapshot_timer_is_rejected(self):
        client, server = fixture()
        with self.assertRaisesRegex(AssertionError, "snapshot timer"):
            attribute(client, server, 97)

    def test_repeated_validation_is_accumulated(self):
        client, server = fixture()
        duplicate = copy.deepcopy(server["requests"][-1]["events"][0])
        server["requests"][-1]["events"].insert(0, duplicate)
        self.assertEqual(
            attribute(client, server, 100)["host_publish_phases_ms"]["validation"], 2
        )

    def test_same_repository_request_overlap_is_rejected(self):
        client, server = fixture()
        server["requests"][0]["response"]["overlap_with_prior_response_us"] = 10000
        with self.assertRaisesRegex(AssertionError, "overlapping requests"):
            attribute(client, server, 100)

    def test_nearest_rank_tail_is_explicit_for_small_samples(self):
        values = stats(range(1, 41))
        self.assertEqual((values["p50"], values["p95"], values["p99"]), (20, 38, 40))

    def test_overlap_does_not_add_parallel_wall_time(self):
        result = overlap([(0, 10), (5, 15), (15, 20)])
        self.assertEqual(result["active_union_ms"], 20)
        self.assertEqual(result["overlap_ms"], 5)
        self.assertEqual(result["maximum_concurrent"], 2)
        self.assertEqual(result["mean_concurrent_while_active"], 1.25)


if __name__ == "__main__":
    unittest.main()
