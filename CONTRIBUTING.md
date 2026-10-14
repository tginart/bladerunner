# Contributing

Report reproducible bugs and propose focused changes through
[GitHub issues](https://github.com/tginart/bladerunner/issues) and pull requests.
Keep third-party licenses and attribution with their source. Contributions are
under Apache-2.0. Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Verification

The source manifest excludes Git metadata and generated build output. After
updating reviewed source files, archive the working tree to a fresh directory,
refresh the manifest there, and run the checker. Commit the resulting manifest
along with the source changes. To verify an existing commit:

```sh
BLADERUNNER_CHECK_SOURCE=$(mktemp -d)
BLADERUNNER_CHECK_OUTPUT=$(mktemp -d)
git archive HEAD | tar -x -C "$BLADERUNNER_CHECK_SOURCE"
python3 -B "$BLADERUNNER_CHECK_SOURCE/tools_verify_release.py" \
  --output "$BLADERUNNER_CHECK_OUTPUT" --build --tests
```

The verifier checks source hashes, file modes, public documentation links,
credential markers, workspace metadata, Python utilities, Codex/AIP
authentication contracts, CLI parsing, and installer/updater behavior. It builds
the CLI and executes credential-free help, version, and model-list checks.
Source archives have normalized metadata and reproducible hashes.

These checks do not exercise paid provider requests, every harness feature, or
unsupported binary platforms. Never add account credentials, local installation
state, private tenant identifiers, or real user sessions to a test fixture.
Credential-detection tests may contain reviewed synthetic token strings; the
checker pins their exact file hashes.

Public and development histories are separate. Accepted public changes must be
carried into the canonical development source before the next exported release;
public releases should never merge unrelated development history.
