"""Print the release tag only if this source is newer than all published tags."""

import json
import pathlib
import re
import sys

NUMBER = r"(0|[1-9][0-9]{0,8})"
TAG = re.compile(rf"v{NUMBER}\.{NUMBER}\.{NUMBER}(?:-preview\.{NUMBER})?")


def key(tag):
    match = TAG.fullmatch(tag)
    if match is None:
        return None
    major, minor, patch, preview = match.groups()
    return int(major), int(minor), int(patch), preview is None, int(preview or 0)


def next_tag(metadata, published):
    manifest = pathlib.Path("Cargo.toml").resolve()
    packages = [
        p for p in metadata["packages"]
        if p["name"] == "lazygtd" and pathlib.Path(p["manifest_path"]).resolve() == manifest
    ]
    if len(packages) != 1:
        raise ValueError("missing or ambiguous root lazygtd Cargo package")
    tag = "v" + packages[0]["version"]
    candidate = key(tag)
    if candidate is None:
        raise ValueError("unsupported lazygtd release version")
    versions = [version for tag in published if (version := key(tag)) is not None]
    return tag if not versions or candidate > max(versions) else ""


if __name__ == "__main__":
    metadata = json.loads(pathlib.Path(sys.argv[1]).read_bytes())
    published = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8").splitlines()
    print(next_tag(metadata, published))
