"""Contract tests for the publication/boundary checker using actual metadata."""

import copy
import json
import subprocess
import unittest

from check_workspace import ROOT, validate


class WorkspaceChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT, text=True))

    def test_named_cli_and_dependency_order_are_independent_of_metadata_order(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"].reverse()
        version, order = validate(metadata)
        self.assertTrue(version.startswith("0."))
        self.assertEqual(order[-1], "jj-tandem")
        for package in metadata["packages"]:
            if package["name"] not in order:
                continue
            for dep in package["dependencies"]:
                if dep["kind"] != "dev" and dep["name"] in order:
                    self.assertLess(order.index(dep["name"]), order.index(package["name"]))

    def test_rejects_version_drift(self):
        metadata = copy.deepcopy(self.metadata)
        next(p for p in metadata["packages"] if p["name"] == "jj-tandem-wal")["version"] = "0.99.0"
        with self.assertRaisesRegex(ValueError, "lockstep version"):
            validate(metadata)

    def test_rejects_server_to_client_edge(self):
        metadata = copy.deepcopy(self.metadata)
        server = next(p for p in metadata["packages"] if p["name"] == "jj-tandem-server")
        server["dependencies"].append({"name": "jj-tandem-client", "kind": None, "path": "client", "req": "^0.4.0"})
        with self.assertRaisesRegex(ValueError, "forbidden production dependency"):
            validate(metadata)

    def test_rejects_private_package_publication(self):
        metadata = copy.deepcopy(self.metadata)
        next(p for p in metadata["packages"] if p["name"] == "jj-tandem-simulation")["publish"] = None
        with self.assertRaisesRegex(ValueError, "publishable package set"):
            validate(metadata)

    def test_registry_dependency_cannot_bypass_the_boundary(self):
        metadata = copy.deepcopy(self.metadata)
        server = next(p for p in metadata["packages"] if p["name"] == "jj-tandem-server")
        server["dependencies"].append({"name": "jj-tandem-client", "kind": None, "req": "^0.4.0"})
        with self.assertRaisesRegex(ValueError, "forbidden production dependency"):
            validate(metadata)

    def test_internal_dependency_must_resolve_to_the_workspace_member(self):
        metadata = copy.deepcopy(self.metadata)
        client = next(p for p in metadata["packages"] if p["name"] == "jj-tandem-client")
        del next(d for d in client["dependencies"] if d.get("path"))["path"]
        with self.assertRaisesRegex(ValueError, "workspace path"):
            validate(metadata)

    def test_rejects_unversioned_path_dependency(self):
        metadata = copy.deepcopy(self.metadata)
        client = next(p for p in metadata["packages"] if p["name"] == "jj-tandem-client")
        next(d for d in client["dependencies"] if d.get("path"))["req"] = "*"
        with self.assertRaisesRegex(ValueError, "dependency version"):
            validate(metadata)


if __name__ == "__main__":
    unittest.main()
