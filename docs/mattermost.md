# Mattermost slash commands

Mattermost can create and edit Marbles work with `/marbles`, the same way a chat
integration talks to Jira. The integration is served by `marbles serve` on the
same listener as the normal API, so there is one writer, one audit trail, and no
second service to deploy.

**Status:** implemented as an adapter on the existing server. It is not a Go
Mattermost plugin and it is not a second Marbles API. Installing it is a custom
slash command in Mattermost plus one server config entry.

## Why it is not a second API

The canonical HTTP API remains `/v1/issues.*`, `/v1/claims.*`, `/v1/history.get`
and friends in [`src/api.rs`](../src/api.rs). The slash endpoint is
`POST /v1/mattermost/slash`:

- it accepts Mattermost's form encoding, not JSON;
- it authenticates Mattermost's shared command token, not a Marbles bearer;
- it maps the form's `user_name` to a human `Principal` and calls the same
  `Store` operations;
- it renders Mattermost's `{"response_type","text"}` envelope.

Anything else that needs Marbles should use the canonical API. A client that
imitates a chat provider has forked the contract.

This is also why the Spore web-bridge is not involved. `soil-web-bridge` serves a
Spore's `soil:records` capabilities to a browser; it is a different authority
from the native Marbles server, which owns the issue state machine. Do not point
Mattermost at the records bridge and expect `claim`/`close` semantics.

## Server configuration

Add a bearer-independent section to `server.toml`:

```toml
[mattermost]
token_file = "/run/secrets/mattermost-slash.token"
# Required when the server uses company stores or PostgreSQL.
company = "fpl"
# Optional: used when a command omits --project.
default_project = "shared-infra"
# Optional: empty means every user Mattermost forwards.
allowed_users = ["avery"]
```

The token file contains one string: the same token configured in Mattermost's
slash command. It is read once at startup. A missing or empty file fails server
startup instead of accepting every form. Rotate by replacing the file and
restarting Marbles.

The route is mounted next to the bearer-auth routes, but outside that
middleware. The shared token is the only credential on this route. Keep it
private: anyone who can post the token can act as any Mattermost user the
server forwards.

## Mattermost configuration

Create a custom slash command:

| Field | Value |
|---|---|
| Command | `/marbles` |
| Request URL | `https://marbles.fpl.dev/v1/mattermost/slash` |
| Request method | `POST` |
| Token | the value in `token_file` |
| Response username | `Marbles` (optional) |

A second command `/mb` can point at the same URL and token. The parser does not
inspect the command name Mattermost sends.

Mattermost posts these fields; only the token, `user_name`, and `text` are used.
The rest are ignored rather than trusted:

```text
token=<shared secret>
user_name=avery
text=create Fix the flux capacitor --project fpl
```

## Commands

```text
/marbles create <title> [--project P] [--description D] [--priority N] [--type T] [--label L] [--parent ID]
/marbles list [--project P] [--status S] [--all] [--query TEXT] [--limit N]
/marbles ready [--project P] [--limit N]
/marbles show <id>
/marbles update <id> [--title T] [--description D] [--status S] [--priority N] [--note TEXT] [--label-add L] [--label-remove L]
/marbles claim <id>
/marbles release <id>
/marbles close <id> [--reason TEXT] [--pr URL] [--commit SHA] [--ack TEXT]
```

Aliases: `new` for `create`, `ls` for `list`, `get` for `show`, `edit` for
`update`.

Examples:

```text
/marbles create Deploy the records bridge --project marbles --type task --priority 1 --label soil
/marbles list --project marbles --status in_progress
/marbles update mb-abcd --status blocked --note "waiting on the gateway route"
/marbles claim mb-abcd
/marbles close mb-abcd --pr https://github.com/FuturePresentLabs/marbles/pull/42
/marbles close mb-abcd --ack "no delivery expected: documentation-only change"
```

Quoted strings and backslash escapes work:

```text
/marbles create "Fix the \"anycast\" route" --description "needs a gateway review"
```

### Semantics that are not cosmetic

- **The person is the principal.** `/marbles claim` is a real human hold by the
  Mattermost user who typed it. `/marbles release` only releases that user's own
  claim; releasing another holder is refused with the holder's name.
- **Close still demands evidence.** A PR, a commit, or an explicit ack. A slash
  command cannot bypass the state machine.
- **Create needs a project.** `--project` falls back to `default_project`, but
  the project still has to exist in Marbles. Nothing creates projects on demand.
- **Reads are ephemeral, mutations are in-channel.** `list`, `ready`, `show` and
  `help` answer only to the caller. `create`, `update`, `claim`, `release` and
  `close` post in the channel so the work is visible.
- **Failures are visible.** Unknown flags, missing evidence, refused claims, and
  storage errors are returned as an ephemeral message with the reason. The HTTP
  status stays 200 because Mattermost renders the body; only a bad command token
  gets a 401.

## Hosted companies

`marbles.fpl.dev` uses the hosted server path. Set `[mattermost].company` to the
Mattermost deployment's company scope; the handler puts it on the principal, and
the existing `Api::db_for` routes every command to that company's store. A
hosted server with `company_store_root` or PostgreSQL and no configured company
fails startup rather than guessing a tenant.

For a workstation server, omit `company`; commands use the machine's normal
store.

## Exposure and anycast

`marbles.fpl.dev` is already the production route to the native server. The new
route is available there as `https://marbles.fpl.dev/v1/mattermost/slash` once
the server config and Mattermost command are deployed.

A `.myc` name such as `marbles.myc` is a Mycelium gateway route, not a Marbles
code change. Point the route at the same listener, keep the command token
private, and keep TLS/mTLS or an equivalent gateway check in front of it. A
`.myc` name is an address, not an authentication boundary.

## Tests

[`tests/mattermost.rs`](../tests/mattermost.rs) drives the real form-encoded
route through the real router: token refusal, help, create, update, claim,
release, close, missing evidence, filtered listing, and the user allowlist.
