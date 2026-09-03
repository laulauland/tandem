#!/usr/bin/env python3
"""Validate package boundaries and derive dependency-ordered publication."""

import argparse
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent

# Internal production edges only. Tests may depend on both sides of a seam.
DEPENDENCIES = {
    "jj-tandem-protocol": set(),
    "jj-tandem-wal": set(),
    "jj-tandem-storage": set(),
    "jj-tandem-jj": set(),
    "jj-tandem-client": {"jj-tandem-protocol", "jj-tandem-jj"},
    "jj-tandem-workspace": {"jj-tandem-client", "jj-tandem-protocol", "jj-tandem-jj"},
    "jj-tandem-repository": {"jj-tandem-protocol", "jj-tandem-wal", "jj-tandem-storage", "jj-tandem-jj"},
    "jj-tandem-server": {"jj-tandem-protocol", "jj-tandem-repository"},
    "jj-tandem": {"jj-tandem-client", "jj-tandem-workspace", "jj-tandem-server"},
}


def validate(metadata, expected_version=None):
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    published = {name: p for name, p in packages.items() if p["publish"] != []}
    if set(published) != set(DEPENDENCIES):
        raise ValueError("publishable package set differs from the approved workspace boundaries")
    cli = packages["jj-tandem"]
    if not any(t["name"] == "tandem" and "bin" in t["kind"] for t in cli["targets"]):
        raise ValueError("jj-tandem must own the tandem binary")
    version = expected_version or cli["version"]
    if not version.startswith("0."):
        raise ValueError("Tandem remains pre-1.0")
    graph = {}
    for name, package in published.items():
        if package["version"] != version:
            raise ValueError(f"{name}: expected lockstep version {version}")
        graph[name] = set()
        for dep in package["dependencies"]:
            if dep["kind"] == "dev":
                continue
            target = dep["name"]
            if target in packages or dep.get("path"):
                if target not in DEPENDENCIES[name]:
                    raise ValueError(f"forbidden production dependency: {name} -> {target}")
                member_path = Path(packages[target]["manifest_path"]).parent
                if not dep.get("path") or Path(dep["path"]).resolve() != member_path.resolve():
                    raise ValueError(f"{name} -> {target}: must use the workspace path")
                if dep["req"] != f"^{version}":
                    raise ValueError(f"{name} -> {target}: expected dependency version {version}")
                graph[name].add(target)
    order = []
    while graph:
        ready = sorted(name for name, deps in graph.items() if not deps)
        if not ready:
            raise ValueError("cyclic production dependencies")
        order.extend(ready)
        graph = {name: deps - set(ready) for name, deps in graph.items() if name not in ready}
    return version, order


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version")
    parser.add_argument("--print-publish-order", action="store_true")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT, text=True))
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    if "package" in manifest:
        raise ValueError("workspace root must be virtual")
    version, order = validate(metadata, args.version)
    for name in order:
        if name == "jj-tandem":
            continue
        dep = manifest["workspace"]["dependencies"][name]
        if dep.get("package") != name or dep.get("version") != version:
            raise ValueError(f"{name}: workspace dependency needs explicit package and lockstep version")
    print("\n".join(order) if args.print_publish_order else f"workspace checks passed ({len(order)} production packages, {version})")


if __name__ == "__main__":
    main()
