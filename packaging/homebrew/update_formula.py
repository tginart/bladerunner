"""
Purpose: Create the Homebrew formula from a verified release archive.
Input: A stable version, the release archive, and the SHA256SUMS file.
Output: A formula pinned to that release. An older release does not replace it.
"""

import argparse
import hashlib
from pathlib import Path
import re

TEMPLATE_PATH = Path(__file__).with_name("bladerunner.rb.in")
VERSION_PATTERN = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"


def update_formula(
    *, version: str, archive: Path, sums: Path, output: Path
) -> bool:
    """
    Purpose: Verify a release and write its formula without a version downgrade.
    Input: The version, archive path, checksum manifest path, and output path.
    Output: True if the formula changes. Raise ValueError for invalid input.
    """
    version = version.removeprefix("v")
    version_key = _version_key(version)
    asset = f"bladerunner-{version}-aarch64-apple-darwin.tar.gz"
    if archive.name != asset:
        raise ValueError(f"Expected archive named {asset}")
    checksums = []
    for line in sums.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line)
        if match and match[2] == asset:
            checksums.append(match[1].lower())
    if len(checksums) != 1:
        raise ValueError(f"Expected exactly one checksum for {asset}")
    with archive.open("rb") as release_file:
        digest = hashlib.file_digest(release_file, "sha256").hexdigest()
    if digest != checksums[0]:
        raise ValueError(f"SHA-256 mismatch for {asset}")

    previous = output.read_text(encoding="utf-8") if output.exists() else ""
    if previous:
        old_version = re.search(r"/releases/download/v([0-9.]+)/", previous)
        if old_version is None:
            raise ValueError("Existing formula has no versioned release URL")
        if _version_key(old_version[1]) > version_key:
            return False
        old_digest = re.search(r'^  sha256 "([0-9a-f]{64})"$', previous, re.MULTILINE)
        if old_version[1] == version and (
            old_digest is None or old_digest[1] != digest
        ):
            raise ValueError("A published version must not change its archive checksum")

    formula = TEMPLATE_PATH.read_text(encoding="utf-8")
    formula = formula.replace("@VERSION@", version).replace("@SHA256@", digest)
    if formula == previous:
        return False
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(formula, encoding="utf-8")
    return True


def main() -> None:
    """
    Purpose: Read command arguments and update the formula.
    Input: Command arguments from the process.
    Output: The formula file and a short status message.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="Stable release version or tag")
    parser.add_argument("--archive", required=True, type=Path, help="macOS release archive")
    parser.add_argument("--sums", required=True, type=Path, help="Release SHA256SUMS file")
    parser.add_argument("--output", required=True, type=Path, help="Destination formula")
    args = parser.parse_args()
    changed = update_formula(**vars(args))
    print("Updated Homebrew formula" if changed else "Homebrew formula is already current")


def _version_key(version: str) -> tuple[int, ...]:
    if re.fullmatch(VERSION_PATTERN, version) is None:
        raise ValueError(f"Expected a stable X.Y.Z version, got {version!r}")
    return tuple(int(part) for part in version.split("."))


if __name__ == "__main__":
    main()
