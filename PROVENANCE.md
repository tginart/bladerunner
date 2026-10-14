# Provenance

Blade Runner is an independent Apache-2.0 coding-agent distribution derived from
[Hyper](https://github.com/DaviRain-Su/hyper-grok-build), an unofficial community
build of [Grok Build](https://github.com/xai-org/grok-build).

The imported Hyper baseline is commit
`3ed00bba93ec5893121f55c7166ed4425f1fdab9` (`1.0.1-r1`). Blade Runner adds its
own executable identity, isolated state directory, Codex/AIP provider defaults,
Foundry authentication, and harness behavior. Upstream package names and some
embedded upstream reference documentation remain for compatibility.

The public repository starts with a reviewed snapshot rather than importing
development history. Each source file is recorded in `PUBLIC_MANIFEST.json`.
Original upstream licensing and attribution remain in `LICENSE`, `NOTICE`,
`THIRD-PARTY-NOTICES`, and the relevant vendored source directories.

Model inference is provided externally. No bundled serving engine, model
weights, account credentials, or tenant configuration is part of this source
distribution. Blade Runner is not affiliated with or endorsed by OpenAI,
Palantir, xAI, SpaceXAI, or Moonshot AI.
