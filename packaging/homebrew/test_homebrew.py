"""
Purpose: Verify release checksums, version order, and the Homebrew launcher.
Input: Synthetic releases and isolated user directories.
Output: Test results without network requests or changes to user state.
"""

import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

from update_formula import TEMPLATE_PATH, update_formula


class HomebrewTest(unittest.TestCase):
    """
    Purpose: Test formula updates and launcher behavior with temporary files.
    Input: A synthetic archive, manifest, executable, and bundle.
    Output: Assertions for package integrity and user data preservation.
    """

    def setUp(self) -> None:
        """
        Purpose: Create an isolated release and launcher for each test.
        Input: The formula template.
        Output: Temporary package files and a separate user state path.
        """
        self.temp = tempfile.TemporaryDirectory(prefix="homebrew test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "Formula/bladerunner.rb"
        self.sums = self.root / "SHA256SUMS"
        self.archive = self._release("1.2.3")
        self.libexec = self.root / "opt/bladerunner/libexec"
        (self.libexec / "bundled/skills").mkdir(parents=True)
        binary = self.libexec / "bladerunner"
        binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n', encoding="utf-8")
        binary.chmod(0o755)
        template = TEMPLATE_PATH.read_text(encoding="utf-8")
        launcher = textwrap.dedent(template.split("<<~EOS\n", 1)[1].split("    EOS", 1)[0])
        self.launcher = self.root / "launcher"
        self.launcher.write_text(
            launcher.replace("#{opt_libexec}", str(self.libexec)), encoding="utf-8"
        )
        self.state = self.root / "user state"
        self.env = {**os.environ, "BLADERUNNER_HOME": str(self.state)}

    def test_verified_formula_is_idempotent(self) -> None:
        self.assertTrue(self._update())
        self.assertFalse(self._update())
        formula = self.output.read_text(encoding="utf-8")
        self.assertIn("/releases/download/v1.2.3/", formula)
        self.assertNotIn("@SHA256@", formula)

    def test_bad_checksum_does_not_replace_formula(self) -> None:
        self._update()
        previous = self.output.read_bytes()
        self.archive.write_bytes(b"corrupt archive")
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            self._update()
        self.assertEqual(previous, self.output.read_bytes())

    def test_missing_and_duplicate_checksums_are_rejected(self) -> None:
        manifest = self.sums.read_text(encoding="utf-8")
        for content in ("", manifest * 2):
            self.sums.write_text(content, encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "exactly one checksum"):
                self._update()
        self.assertFalse(self.output.exists())

    def test_late_release_does_not_downgrade_formula(self) -> None:
        self._update()
        self.archive = self._release("1.2.2")
        self.assertFalse(self._update(version="1.2.2"))
        self.assertIn(
            "/releases/download/v1.2.3/", self.output.read_text(encoding="utf-8")
        )

    def test_republished_version_with_new_bytes_is_rejected(self) -> None:
        self._update()
        self.archive = self._release("1.2.3", content=b"different bytes")
        with self.assertRaisesRegex(ValueError, "must not change"):
            self._update()

    def test_invalid_versions_are_rejected(self) -> None:
        for version in ("1.2.3-beta", "01.2.3", "1.2.3\n", "../1.2.3"):
            with self.assertRaisesRegex(ValueError, "stable X.Y.Z"):
                self._update(version=version)

    def test_first_launch_and_repeated_launch(self) -> None:
        for _ in range(2):
            result = self._launch("--version", "argument with spaces")
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual("--version\nargument with spaces\n", result.stdout)
        self.assertEqual(
            (self.libexec / "bundled").resolve(), (self.state / "bundled").resolve()
        )

    def test_migration_preserves_bundle_and_configuration(self) -> None:
        (self.state / "bundled").mkdir(parents=True)
        (self.state / "bundled/previous").write_text("keep", encoding="utf-8")
        (self.state / "config.toml").write_text("keep config", encoding="utf-8")
        result = self._launch("--version")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual("keep", (self.state / "bundled.before-homebrew/previous").read_text())
        self.assertEqual("keep config", (self.state / "config.toml").read_text())

    def test_simultaneous_first_launches_share_one_bundle(self) -> None:
        processes = [
            subprocess.Popen(
                ["sh", str(self.launcher), "--version"], env=self.env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            for _ in range(3)
        ]
        for process in processes:
            stdout, stderr = process.communicate(timeout=15)
            self.assertEqual(0, process.returncode, stderr)
            self.assertEqual("--version\n", stdout)
        self.assertTrue((self.state / "bundled").is_symlink())
        self.assertFalse((self.state / ".homebrew-bundle-lock").exists())

    def test_backup_collision_preserves_both_directories(self) -> None:
        (self.state / "bundled").mkdir(parents=True)
        (self.state / "bundled.before-homebrew").mkdir()
        result = self._launch("--version")
        self.assertEqual(1, result.returncode)
        self.assertIn("already exists", result.stderr)
        self.assertFalse((self.state / "bundled").is_symlink())

    def test_update_redirects_to_homebrew_without_state_changes(self) -> None:
        result = self._launch("update")
        self.assertEqual(1, result.returncode)
        self.assertIn("brew upgrade tginart/tap/bladerunner", result.stderr)
        self.assertFalse(self.state.exists())

    def _release(self, version: str, *, content: bytes = b"archive") -> Path:
        archive = self.root / f"bladerunner-{version}-aarch64-apple-darwin.tar.gz"
        archive.write_bytes(content)
        self.sums.write_text(
            f"{hashlib.sha256(content).hexdigest()}  {archive.name}\n", encoding="utf-8"
        )
        return archive

    def _update(self, *, version: str = "1.2.3") -> bool:
        return update_formula(
            version=version, archive=self.archive, sums=self.sums, output=self.output
        )

    def _launch(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sh", str(self.launcher), *args],
            env=self.env, capture_output=True, text=True, check=False,
        )


if __name__ == "__main__":
    unittest.main()
