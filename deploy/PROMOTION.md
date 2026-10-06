# Release promotion

Fab builds each `main` commit once as a digest-pinned OCI image. The release
controller promotes that same artifact through its configured stages.

Marbles uses Shroud's compatibility-attested stateful rollout API. A rollout
retains the existing `marbles-company-store` volume, waits for `/healthz`, and
restores the previous image against the same bytes if the candidate is not
ready. Do not replace this with a retire-then-create deployment or a mutable
image tag.

Compatibility evidence is versioned with the deployment target and must be
updated deliberately whenever the SQLite read/write contract changes.

The Fab release history and Shroud rollout receipt are the authoritative
deployment evidence; a mutable registry tag is not.

## Authentication preflight and postflight

For a human operator, supply the registered client secret through
`MARBLES_CLIENT_SECRET` from the secret manager, not a command-line argument:

```sh
mb --url https://marbles.fpl.dev login
mb --url https://marbles.fpl.dev doctor --project shared-infra --json
```

The login stores private `client.toml` under `MARBLES_HOME` (default
`~/.marbles`). Commands renew its short-lived token automatically. Concurrent
CLI calls serialize refresh on a kernel-owned sidecar lock, reload the latest
profile, and atomically persist rotation. Process exit releases the lock.
Credentials are created with mode 0600 and synced before replacement.
Expired/revoked sessions still require login; this does not disable expiration
or recover a consumed refresh token after a lost issuer response. Issuer calls
and JWKS discovery/fetch have timeouts; token errors never echo provider bodies.
Unknown signing-key IDs trigger a single-flight JWKS refresh with a five-second
cooldown instead of waiting for the fifteen-minute normal cache expiry.

For unattended callers, the operator provisions a company-scoped service bearer
and mounts it as a private secret file. Local `agent-token` does not enroll a
hosted tenant. Identity and company ownership come from server-owned credentials,
never a request body.

```sh
export MARBLES_URL=https://marbles.fpl.dev
export MARBLES_TOKEN_FILE=/run/secrets/marbles-agent
mb doctor --project shared-infra --json
```

The file must be private (0600 on Unix) and is read on every invocation. An
operator can rotate its contents without changing agent commands. This is a
credential-consumption path, **not** a new hosted token-issuance API. Revocation
and rotation remain operational requirements. `MARBLES_TOKEN` is also supported,
but a fixed JWT expires and overrides browser refresh. Explicit bearers do not
depend on a stale/malformed browser profile. Saved browser credentials are never
sent to a different `--url`.

`MARBLES_OIDC_REFRESH_TOKEN` configuration is process-local and never overwrites
the saved browser profile. It cannot retain rotation across separate processes:
use the saved login or an operator-managed service bearer for durable access.

`doctor` checks public health and authenticated read-only project access. A
health failure points to routing/storage/deployment; 401 to the credential;
invalid_client to issuer registration. Health alone proves no tenant access.
Doctor does not claim work, create tenants, deploy or collect billing.

## OpenTofu deployment boundary

The shared-infra Marbles root currently uses `terraform_data` shell provisioners
and a private controller tunnel. It is **not** the hosted customer happy path.
The immutable operator `fpl_shroud_deployment` resource is not a substitute for
in-place stateful rollout: request changes have replacement semantics. Never
apply an unexpected replacement to the Marbles data volume.

A replacement hosted path must preserve these gates:

- Separate project deployment credentials, application credentials and billing
  inventory/usage credentials. Marbles login grants no Shroud operator access.
- Keep bearer values, vendor secrets and database URLs out of declarations,
  plans and state. Use controller secret/volume references. `sensitive` masks
  display; it does not remove a value from state.
- Adopt the existing controller revision with GET-only refresh and no drift
  before rollout. Preserve the volume identity and exclusive writer.
- Build once, release a digest through Fab, health-gate promotion and retain the
  controller receipt. Reconcile the same operation after timeout, not a second
  writer. Do not retire-then-create a stateful workload.
- Run doctor before/after promotion, then an authorized persistence canary in
  a disposable company plus recovery/rollback tests. The canary is a separate
  write, not part of doctor.
- Require verified runtime retirement and complete billing inventory/receipts
  before cleanup or monthly close.

These CLI changes do not deploy, adopt, or cut over PG17 authority. The verified
backup/restore and exclusive-writer gates remain in the separate cutover runbook.
