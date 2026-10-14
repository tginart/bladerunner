# Updates and removal

The installer and built-in updater use releases from
[tginart/bladerunner](https://github.com/tginart/bladerunner/releases). Downloads
are checked against the release's `SHA256SUMS`. Keep the installed executable,
bundled files, and license notices together.

Re-run the installer to install the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/tginart/bladerunner/main/install.sh | sh
```

To select a particular release, save the script and pass a version:

```sh
curl -fsSL https://raw.githubusercontent.com/tginart/bladerunner/main/install.sh -o /tmp/bladerunner-install.sh
sh /tmp/bladerunner-install.sh --version v1.1.20
```

The binary release beta supports Apple Silicon macOS. The presence of inherited
Linux/Windows source and installer code does not mean those binaries are
published or verified for the beta.

For removal, sign out, remove `~/.bladerunner/bin` from your shell PATH, and remove
the installed binary link, downloads, and bundled files under `~/.bladerunner`.
Keep a backup of sessions and configuration if needed. Removing the entire
directory also deletes local credentials, history, logs, and configuration.
