"""Allocate a release for a main commit and stamp the build's Cargo version."""

import json
import pathlib
import re
import subprocess
import sys

NUMBER = r"(0|[1-9][0-9]{0,8})"
TAG = re.compile(rf"v{NUMBER}\.{NUMBER}\.{NUMBER}(?:-preview\.{NUMBER})?")


def key(tag):
    match = TAG.fullmatch(tag)
    if match is None:
        return None
    major, minor, patch, preview = match.groups()
    return int(major), int(minor), int(patch), preview is None, int(preview or 0)


def root_version(metadata):
    manifest = pathlib.Path("Cargo.toml").resolve()
    packages = [
        p for p in metadata["packages"]
        if p["name"] == "lazygtd" and pathlib.Path(p["manifest_path"]).resolve() == manifest
    ]
    if len(packages) != 1:
        raise ValueError("missing or ambiguous root lazygtd Cargo package")
    return packages[0]["version"]


def next_tag(source_version, refs, published, commit, is_ancestor):
    source = "v" + source_version
    if key(source) is None:
        raise ValueError("unsupported lazygtd release version")

    published = {tag for tag in published if key(tag) is not None}
    own_tags = [tag for tag, target in refs.items() if target == commit and key(tag) is not None]
    if published.intersection(own_tags):
        return ""
    if published:
        latest = refs[max(published, key=key)]
        if latest != commit and is_ancestor(commit, latest):
            return ""
    if own_tags:
        return max(own_tags, key=key)

    reserved = [key(tag) for tag in refs if key(tag) is not None]
    if not reserved or key(source) > max(reserved):
        return source
    major, minor, patch, stable, preview = max(reserved)
    if stable:
        return f"v{major}.{minor}.{patch + 1}"
    return f"v{major}.{minor}.{patch}-preview.{preview + 1}"


def read_refs(lines):
    refs = {}
    for line in lines:
        tag, commit, peeled = line.split("\t")
        refs[tag] = peeled or commit
    return refs


def is_ancestor(older, newer):
    result = subprocess.run(["git", "merge-base", "--is-ancestor", older, newer], check=False)
    if result.returncode not in (0, 1):
        result.check_returncode()
    return result.returncode == 0


def stamp(tag, directory):
    if key(tag) is None:
        raise ValueError("unsupported lazygtd release tag")
    changed = {}
    for name in ("Cargo.toml", "Cargo.lock"):
        path = directory / name
        contents, found = re.subn(
            r'(?m)^name = "lazygtd"\nversion = "[^"\n]+"$',
            f'name = "lazygtd"\nversion = "{tag[1:]}"',
            path.read_text(encoding="utf-8"),
        )
        if found != 1:
            raise ValueError(f"missing or ambiguous lazygtd version in {path}")
        changed[path] = contents
    for path, contents in changed.items():
        path.write_text(contents, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    if sys.argv[1] == "stamp":
        stamp(sys.argv[2], pathlib.Path.cwd())
    else:
        _, _, metadata_path, published_path, refs_path, commit = sys.argv
        metadata = json.loads(pathlib.Path(metadata_path).read_bytes())
        published = pathlib.Path(published_path).read_text(encoding="utf-8").splitlines()
        refs = read_refs(pathlib.Path(refs_path).read_text(encoding="utf-8").splitlines())
        print(next_tag(root_version(metadata), refs, published, commit, is_ancestor))
