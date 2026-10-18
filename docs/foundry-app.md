# Set up Foundry browser sign-in

Bladerunner signs in to Palantir AIP in one of two ways:

- **Personal token.** No administrator setup required. Each user creates a token
  under Account > Settings > Tokens and pastes it at sign-in.
- **Browser sign-in (OAuth).** Opens your browser and refreshes tokens
  automatically. An administrator registers Bladerunner once on your Foundry
  using the steps below.

An administrator creates the app in **Developer Console**, then shares its
client ID with users who want browser sign-in.

## Who can do this

- Registering needs the **Manage OAuth 2.0 clients** permission in the
  organization that will own the app.
- Enabling the app for an organization needs organization admin rights.

## Settings

In **Developer Console**, create a new application with these settings:

| Setting | Value |
|---|---|
| Name | Bladerunner |
| Description | Coding agent that calls AIP language models on behalf of the signed-in user. |
| Save location | Any project you control. It decides who can edit the app's settings, not what Bladerunner can access. |
| Ontology SDK | None ("No, I will not use an Ontology SDK"), if the wizard asks. |
| Client type | **Public client** (native or single-page application). |
| Grant types | **Authorization code** only. Leave client credentials off. |
| Redirect URLs | `http://127.0.0.1/` and `https://<your-foundry-hostname>/multipass/api/oauth2/callback` |
| Restrictions | **Unrestricted**. Don't add ontology, dataset, or admin permissions. |
| Organization | **Enabled** for each organization whose users will sign in. |

Notes:

- `http://127.0.0.1/` has no port on purpose. Foundry ignores the port of a
  loopback redirect, and Bladerunner listens on whichever port is free.
- The second redirect URL is for the paste flow, which Bladerunner uses inside
  SSH sessions and with `--no-browser`. Foundry shows a code to paste instead
  of redirecting.
- Unrestricted mode doesn't widen access. A token's access is limited to what
  Bladerunner asks for at sign-in, which is only `api:use-language-models-execute`
  (call any AIP model the user can use) and `offline_access` (a refresh token),
  and to what the user could already do.

Copy the app's **client ID** once it's created. It is not a secret.

## Sign in

Give users your Foundry hostname, the address in the browser when Foundry is
open (e.g. `acme.palantirfoundry.com`), and the client ID. Each user signs in
once, in the app:

```text
/login palantir-aip acme.palantirfoundry.com <client-id>
```

or from a terminal:

```bash
bladerunner login --palantir-aip --hostname acme.palantirfoundry.com --client-id <client-id>
```

Both save the hostname and client ID to `~/.bladerunner/config.toml`, so later
sign-ins are just `/login palantir-aip`. To pin them for everyone on a machine,
put them in the managed config instead:

```toml
[palantir_aip]
hostname = "acme.palantirfoundry.com"
client_id = "<client-id>"
```

## Sign in with a personal token

```text
/login palantir-aip token acme.palantirfoundry.com
```

or `bladerunner login --palantir-aip --token`, or export Palantir's SDK
variables before starting Bladerunner:

```bash
export FOUNDRY_HOSTNAME=acme.palantirfoundry.com
export FOUNDRY_TOKEN=<personal token>
```

When several credentials exist, Bladerunner uses its own browser sign-in first,
then a token saved at sign-in, then `FOUNDRY_TOKEN`.

## Troubleshooting

- **The browser shows an error about the redirect URL.** Check that
  `http://127.0.0.1/` is listed exactly, with the trailing slash.
- **Foundry refuses the app or shows no consent screen.** An admin has to
  enable the app for your organization.
- **Revoking access.** Each user can revoke Bladerunner under Foundry Settings >
  Authorized applications. `bladerunner logout --palantir-aip` removes the local
  credential.
