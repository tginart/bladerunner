# Homebrew distribution

The public `tginart/homebrew-tap` repository provides `Formula/bladerunner.rb`
for Apple Silicon macOS. The formula installs the existing release archive;
users do not need Rust or a source build.

After the release workflow publishes a verified release, it calls
`.github/workflows/homebrew.yml`. This workflow downloads the public macOS
archive and `SHA256SUMS`, verifies the archive, renders the formula, installs
and tests it on Apple Silicon, and pushes the formula to the tap. Updates
are serialized. An older release cannot replace a newer formula. A changed
checksum for an existing version fails; publish a new version instead.

The source repository secret `HOMEBREW_TAP_DEPLOY_KEY` contains a dedicated
SSH private key. Its public key is a write-enabled deploy key on the tap.
This key can write only to the tap, and no personal token is used in Actions.
Rotate the deploy key and the source secret together.

To retry an update without a new release:

```sh
gh workflow run homebrew.yml --repo tginart/bladerunner \
    -f release_tag=vX.Y.Z
```

The generator and launcher tests run with:

```sh
python3 -B -m unittest discover -s packaging/homebrew -v
```

`bladerunner.rb.in` is the source template. The generator requires Python 3.11
or later. Edit the template here; each release propagates it to the tap.
