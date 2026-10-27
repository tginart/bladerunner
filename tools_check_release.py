"""Verify public source integrity and reject credential markers.

Findings contain file paths and categories only. Pattern checks supplement an
explicit reviewed export; they are not a complete security audit.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile

MANIFEST = "PUBLIC_MANIFEST.json"
FORBIDDEN_ROOTS = {".git", ".claude", ".cursor", ".grok", "engine", "desktop", "tools"}
PATTERNS = {
    "credential": rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{60,}|AKIA[A-Z0-9]{16}|sk-proj-[A-Za-z0-9_-]{40,}|eyJ[A-Za-z0-9_-]{20,}\.eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]+",
}
# Fixed synthetic credential-detection fixtures. Exact bytes are reviewed;
# changing a fixture requires re-review rather than a broad path exemption.
SYNTHETIC_FIXTURES = {
    "packages/coding-agent/xai-grok-shell/src/auth/device_code.rs": "95e9592d625ef228ff9b5d5981f0724cfbf1f77993eda7045f03655ab7c03116",
    "packages/coding-agent/xai-grok-shell/src/auth/flow.rs": "22f3af6de64ad06e763a193db2c5092d3c836c533bd08c04758fdf2ba22c1803",
    "packages/platform/xai-grok-secrets/src/sanitizer.rs": "446e92a068e3bda23becca3e48b611bfea975cb4670bd128c8e08fa84347f07b",
}
PRIMARY_DOCS = {"README.md", "CONTRIBUTING.md", "SECURITY.md",
                "docs/foundry-app.md", "docs/privacy.md", "docs/updates.md"}


def safe_name(name):
    p = PurePosixPath(name)
    return bool(name) and not p.is_absolute() and ".." not in p.parts and str(p) == name


def write_manifest(root):
    entries = []
    for p in sorted(root.rglob("*")):
        if p.is_symlink():
            raise ValueError("Symlinks are not public source files")
        if p.is_file() and p.name != MANIFEST:
            data = p.read_bytes()
            entries.append({"path": p.relative_to(root).as_posix(), "bytes": len(data),
                            "sha256": hashlib.sha256(data).hexdigest(),
                            "mode": "0755" if p.stat().st_mode & 0o111 else "0644"})
    entries.sort(key=lambda row: row["path"])
    (root / MANIFEST).write_text(json.dumps({"schema_version": 1,
        "scope": "All public source files except this manifest; no Git metadata.",
        "files": entries}, indent=2) + "\n")


def screen(data, name, findings):
    if SYNTHETIC_FIXTURES.get(name) == hashlib.sha256(data).hexdigest():
        return
    for category, pattern in PATTERNS.items():
        if re.search(pattern, data):
            findings.append({"path": name, "category": category})


def check(root, artifacts=()):
    root = root.resolve()
    manifest = json.loads((root / MANIFEST).read_text())
    if manifest.get("schema_version") != 1:
        raise ValueError("Unsupported manifest schema")
    rows = manifest["files"]
    names = [row["path"] for row in rows]
    if names != sorted(set(names)) or any(not safe_name(name) for name in names):
        raise ValueError("Unsafe, duplicate, or unsorted manifest paths")
    expected = set(names) | {MANIFEST}
    actual = set()
    for p in root.rglob("*"):
        name = p.relative_to(root).as_posix()
        if p.is_symlink() or p.parts[len(root.parts)] in FORBIDDEN_ROOTS:
            raise ValueError("Non-public source entry: " + name)
        if p.is_file():
            actual.add(name)
        elif not p.is_dir():
            raise ValueError("Non-regular source entry: " + name)
    if actual != expected:
        raise ValueError("Manifest file set mismatch: " + json.dumps({
            "unexpected": sorted(actual - expected), "missing": sorted(expected - actual)}))
    findings = []
    for row in rows:
        p = root / row["path"]
        data = p.read_bytes()
        mode = "0755" if stat.S_IMODE(p.stat().st_mode) & 0o111 else "0644"
        if len(data) != row["bytes"] or hashlib.sha256(data).hexdigest() != row["sha256"] or mode != row["mode"]:
            raise ValueError("Manifest integrity mismatch: " + row["path"])
        screen(data, row["path"], findings)
        if row["path"] in PRIMARY_DOCS:
            for link in re.findall(r"\]\(([^\s)]+)", data.decode()):
                if link.startswith(("https:", "http:", "mailto:", "#")):
                    continue
                target = (p.parent / link.split("#")[0]).resolve()
                if not target.is_relative_to(root) or not target.is_file():
                    findings.append({"path": row["path"], "category": "broken_local_link"})
    for artifact in artifacts:
        with tarfile.open(artifact, "r:gz") as bundle:
            members = bundle.getmembers()
            if len(members) > 10000 or sum(m.size for m in members) > 512 * 1024 * 1024:
                raise ValueError("Source archive exceeds size budget")
            for member in members:
                if not safe_name(member.name) or not (member.isfile() or member.isdir()):
                    raise ValueError("Unsafe source archive member")
                if member.uid or member.gid or member.uname or member.gname:
                    raise ValueError("Source archive contains ownership metadata")
                if member.isfile():
                    relative = member.name.removeprefix("bladerunner/")
                    screen(bundle.extractfile(member).read(), relative, findings)
    if findings:
        raise ValueError(json.dumps(findings, indent=2))
    return {"source_files": len(actual), "manifest": "passed", "privacy_patterns": "passed",
            "primary_document_links": "passed", "artifacts_screened": len(artifacts)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("artifacts", type=Path, nargs="*")
    parser.add_argument("--refresh-manifest", action="store_true")
    args = parser.parse_args()
    if args.refresh_manifest:
        write_manifest(args.root)
    print(json.dumps(check(args.root, args.artifacts), indent=2))
