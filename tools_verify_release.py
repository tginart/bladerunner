"""Verify the exact public snapshot from a disposable build and test directory."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

sys.dont_write_bytecode = True
from tools_check_release import check


def source_archive(root, target):
    with target.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
        with tarfile.open(fileobj=zipped, mode="w") as bundle:
            for p in sorted(root.rglob("*")):
                if p.is_file():
                    data = p.read_bytes()
                    entry = tarfile.TarInfo("bladerunner/" + p.relative_to(root).as_posix())
                    entry.size = len(data)
                    entry.mode = 0o755 if p.stat().st_mode & 0o111 else 0o644
                    entry.mtime = 0
                    bundle.addfile(entry, io.BytesIO(data))


def verify(root, output, build=False, tests=False, binary=None):
    root, output = root.resolve(), output.resolve()
    if output.is_relative_to(root):
        raise ValueError("Verification evidence must be outside the candidate")
    integrity = check(root)
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "running", "integrity": integrity, "checks": [], "skips": [
        "Live Codex and Foundry account requests require separate authenticated acceptance checks.",
        "Desktop, Intel macOS, Linux and Windows binary releases are outside this CLI beta matrix."]}
    env = {key: value for key, value in os.environ.items()
           if not any(marker in key.upper() for marker in ("TOKEN", "API_KEY", "SENTRY_DSN", "GROK_AUTH", "TELEMETRY_BUILD"))}
    env.update(PYTHONDONTWRITEBYTECODE="1", DISABLE_TELEMETRY="1", DISABLE_ERROR_REPORTING="1",
               BLADERUNNER_DISABLE_AUTOUPDATER="1", GROK_DISABLE_AUTOUPDATER="1",
               GROK_VERSION=(root / "VERSION").read_text().strip(), CMAKE_POLICY_VERSION_MINIMUM="3.5")

    def save():
        (output / "verification.json").write_text(json.dumps(report, indent=2) + "\n")

    def run(label, args, cwd, extra=None, require_tests=False):
        print("Verifying " + label, flush=True)
        with (output / (label + ".log")).open("w") as log:
            result = subprocess.run(args, cwd=cwd, env={**env, **(extra or {})}, stdout=log,
                                    stderr=subprocess.STDOUT, timeout=7200)
        report["checks"].append({"name": label, "exit_code": result.returncode})
        save()
        if result.returncode:
            raise RuntimeError(label + " failed; inspect the corresponding verification log")
        if require_tests:
            counts = re.findall(r"test result: ok\. (\d+) passed", (output / (label + ".log")).read_text())
            if not counts or sum(map(int, counts)) == 0:
                raise RuntimeError(label + " selected no passing tests")
            report["checks"][-1]["passed_tests"] = sum(map(int, counts))
            save()

    try:
        with tempfile.TemporaryDirectory(prefix="bladerunner-public-verification-") as temp:
            scratch = Path(temp)
            work = scratch / "source"
            shutil.copytree(root, work)
            env["BLADERUNNER_HOME"] = str(scratch / "state")
            env["GROK_HOME"] = str(scratch / "state")
            run("workspace-metadata", ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], work)
            if os.name != "nt":
                run("installer-shell-syntax", ["sh", "-n", "install.sh"], work)
            run("session-reader", [sys.executable, "-B", "-m", "unittest", "discover", "-s", "scripts", "-p", "test_session_reader.py"], work)
            run("research-auditors", [sys.executable, "-B", "-m", "unittest", "discover", "-s", "bundled/skills/deep-research/scripts", "-p", "test_*.py"], work)
            if tests:
                for label, package, selector in [
                    ("codex-catalog", "xai-grok-models", "codex_catalog_only_offers_requested_gpt6_models"),
                    ("foundry-auth", "xai-grok-shell", "auth::palantir_aip"),
                    ("codex-auth", "xai-grok-shell", "auth::openai_codex"),
                    ("cli-contract", "xai-grok-pager", "app::cli::tests"),
                ]:
                    run(label, ["cargo", "test", "-p", package, "--lib", selector, "--locked"], work, require_tests=True)
                run("installer-updater-contract", ["cargo", "test", "-p", "xai-grok-update", "--features",
                    "community-build,community-update-test-hooks", "--test", "test_community_update", "--locked"], work, require_tests=True)
            else:
                report["skips"].append("Rust runtime contract tests were not requested.")
            if build:
                run("cli-build", ["cargo", "build", "-p", "xai-grok-pager-bin", "--features", "community-build", "--locked"], work)
                target = Path(env.get("CARGO_TARGET_DIR", work / "target"))
                binary = target / "debug" / ("bladerunner.exe" if os.name == "nt" else "bladerunner")
            if binary:
                binary = Path(binary).resolve()
                smoke_env = {"HOME": str(scratch / "home"), "USERPROFILE": str(scratch / "home")}
                (scratch / "home").mkdir()
                for label, args in [("version", ["--version"]), ("help", ["--help"]),
                                    ("agent-help", ["agent", "--help"]), ("models", ["models"])]:
                    run("cli-" + label, [str(binary), *args], scratch, smoke_env)
                version = (output / "cli-version.log").read_text()
                if "bladerunner " + env["GROK_VERSION"] not in version:
                    raise ValueError("CLI version does not match source VERSION")
                help_text = (output / "cli-help.log").read_text()
                if any(line.strip().startswith("serve ") for line in help_text.splitlines()):
                    raise ValueError("Unexpected inference-serving command")
            else:
                report["skips"].append("CLI binary build and execution were not requested.")
        archive = output / ("bladerunner-" + env["GROK_VERSION"] + "-source.tar.gz")
        source_archive(root, archive)
        repeat = output / "reproducibility.tar.gz"
        source_archive(root, repeat)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        if digest != hashlib.sha256(repeat.read_bytes()).hexdigest():
            raise ValueError("Source archive is not reproducible")
        repeat.unlink()
        report["archive_integrity"] = check(root, [archive])
        report["source_archive"] = {"name": archive.name, "bytes": archive.stat().st_size, "sha256": digest}
        report["status"] = "passed"
        save()
        return report
    except BaseException:
        report["status"] = "failed"
        save()
        raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--tests", action="store_true")
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.root, args.output, args.build, args.tests, args.binary), indent=2))
