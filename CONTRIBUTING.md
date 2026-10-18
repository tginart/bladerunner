# Contributing

Report reproducible bugs and propose focused changes through
[GitHub issues](https://github.com/tginart/bladerunner/issues) and pull requests.
Keep third-party licenses and attribution with their source. Contributions are
under Apache-2.0. Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Build from source

Install the Rust toolchain specified in `rust-toolchain.toml`, a C/C++ compiler,
CMake, and [DotSlash](https://dotslash-cli.com/). On macOS, install Xcode Command
Line Tools.

```sh
GROK_VERSION="$(cat VERSION)" cargo build \
    -p xai-grok-pager-bin --features community-build --release --locked
target/release/bladerunner --version
```

Prebuilt beta releases support Apple Silicon macOS.

## Pull requests

Explain the problem, your change, and how you tested it. Run the tests for the
packages you change and include the results in your pull request.

Use synthetic test data. Keep credentials, local Foundry settings, and real user
sessions out of commits.
