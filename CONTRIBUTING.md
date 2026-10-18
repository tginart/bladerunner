# Contributing

Report reproducible bugs and propose focused changes through
[GitHub issues](https://github.com/tginart/bladerunner/issues) and pull requests.
Keep third-party licenses and attribution with their source. Contributions are
under Apache-2.0. Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Build from source

Install the Rust toolchain specified in `rust-toolchain.toml`, a C/C++ compiler,
CMake, and [DotSlash](https://dotslash-cli.com/). On macOS, install Xcode Command
Line Tools. DotSlash resolves the protobuf compiler in `bin/protoc`.

```sh
GROK_VERSION="$(cat VERSION)" cargo build -p xai-grok-pager-bin --features community-build --release --locked
target/release/bladerunner --version
```

Prebuilt beta releases support Apple Silicon macOS.

## Verify changes

Include updated `PUBLIC_MANIFEST.json` entries with source changes. Use
`tools_check_release.py` with `--refresh-manifest` and the source archive's path,
then copy the manifest back into your checkout. Keep Git metadata and build
output out of the archive.

To verify a committed change:

```sh
BLADERUNNER_CHECK_SOURCE=$(mktemp -d)
BLADERUNNER_CHECK_OUTPUT=$(mktemp -d)
git archive HEAD | tar -x -C "$BLADERUNNER_CHECK_SOURCE"
python3 -B "$BLADERUNNER_CHECK_SOURCE/tools_verify_release.py" \
  --output "$BLADERUNNER_CHECK_OUTPUT" --build --tests
```

The verifier builds the CLI and checks source integrity, authentication
contracts, and installer behavior. It does not make signed-in provider requests.
Use synthetic fixtures; keep credentials, local Foundry settings, and real user
sessions out of tests and commits.
