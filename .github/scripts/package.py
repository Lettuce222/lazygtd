"""Build release archives from already-built cargo binaries (CI Python only)."""

import argparse
import io
import json
import pathlib
import re
import subprocess
import tarfile
import zipfile

TAG = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-preview\.[0-9]+)?")
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")


def notices(target):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        text=True, encoding="utf-8",
    ))
    # Build/proc macro and target crates can contribute generated or linked code.
    dependencies = [p for p in metadata["packages"] if p["source"] is not None]
    output = [
        "Third-party notices",
        "License notices from Cargo's resolved package sources.",
        "The SQLite amalgamation is distributed by SQLite in the public domain:",
        "https://www.sqlite.org/copyright.html",
    ]
    bodies = {}
    for pkg in sorted(dependencies, key=lambda p: (p["name"], p["version"])):
        directory = pathlib.Path(pkg["manifest_path"]).parent
        files = {
            p for p in directory.rglob("*")
            if p.is_file() and not p.is_symlink() and p.name.lower().startswith(LICENSE_PREFIXES)
        }
        if pkg["license_file"]:
            candidate = directory / pkg["license_file"]
            if not candidate.is_file() or not candidate.resolve().is_relative_to(directory.resolve()):
                raise ValueError("invalid explicit license path")
            files.add(candidate)
        if not files:
            raise ValueError(f"no upstream license files for {pkg['name']} {pkg['version']}")
        output.extend(("", f"{pkg['name']} {pkg['version']}: {pkg['license'] or 'see included terms'}"))
        for path in sorted(files):
            text = path.read_text(encoding="utf-8", errors="replace").strip()
            if not text:
                continue
            if text not in bodies:
                bodies[text] = f"License text {len(bodies) + 1}"
            output.append(f"  {path.relative_to(directory)} -> {bodies[text]}")
    for text, label in bodies.items():
        output.extend(("", f"=== {label} ===", text))
    return ("\n".join(output) + "\n").encode("utf-8")


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--tag", required=True)
    cli.add_argument("--platform", required=True)
    cli.add_argument("--target", required=True)
    parsed = cli.parse_args()
    if not TAG.fullmatch(parsed.tag) or not re.fullmatch(r"[a-z0-9_-]+", parsed.platform):
        raise ValueError("invalid release tag or platform")
    windows = parsed.platform.startswith("windows-")
    binary = "lazygtd.exe" if windows else "lazygtd"
    path = pathlib.Path("target", parsed.target, "release", binary).resolve(strict=True)
    version = subprocess.check_output([path, "--version"], text=True, encoding="utf-8").strip()
    if version != f"lazygtd {parsed.tag[1:]}":
        raise ValueError("Cargo package version does not match tag")
    sample = json.loads(subprocess.check_output([path, "--demo", "list", "--json"]))
    if len(sample) < 2 or not any(item["box_id"] == "inbox" for item in sample):
        raise ValueError("release binary failed in-memory SQLite smoke test")
    output = pathlib.Path("dist")
    output.mkdir(exist_ok=True)
    entries = [
        (binary, path.read_bytes()),
        ("LICENSE", pathlib.Path("LICENSE").read_bytes()),
        ("THIRD_PARTY_NOTICES.txt", notices(parsed.target)),
    ]
    stem = f"lazygtd-{parsed.tag}-{parsed.platform}"
    if windows:
        with zipfile.ZipFile(output / (stem + ".zip"), "w", zipfile.ZIP_DEFLATED) as archive:
            for name, data in entries:
                archive.writestr(name, data)
    else:
        with tarfile.open(output / (stem + ".tar.gz"), "w:gz") as archive:
            for name, data in entries:
                info = tarfile.TarInfo(name)
                info.mode = 0o755 if name == binary else 0o644
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))


if __name__ == "__main__":
    main()
