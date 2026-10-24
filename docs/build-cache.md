# Build cache

The container build works with **no remote cache and no AWS credentials**:

```bash
docker build -t marbles:test .
```

`Dockerfile` still copies the `sccache` binary from the Bob image so that an
opt-in build uses the byte-identical wrapper, but it only enables the wrapper
when a cache is fully configured. With no cache, the build stage unsets
`RUSTC_WRAPPER`, sets `SCCACHE_DISABLE=1`, and runs
`cargo test --locked --release --all-targets --all-features` normally.

The project cache lives on DigitalOcean (reached through mycelium). **AWS and
Cloudflare R2 are not used**, and no R2 endpoint is baked into the Dockerfile.

## Opting into the remote cache

The cache is an S3-compatible bucket. You need:

| setting | kind | meaning |
| --- | --- | --- |
| `SCCACHE_ENDPOINT` | build arg | DigitalOcean/mycelium S3 endpoint, e.g. `https://<region>.digitaloceanspaces.com` |
| `SCCACHE_BUCKET` | build arg | bucket name |
| `sccache_access_key_id` | BuildKit secret | DigitalOcean Spaces access key id |
| `sccache_secret_access_key` | BuildKit secret | matching DigitalOcean Spaces secret key |

Optional build args: `SCCACHE_REGION` (default `auto`),
`SCCACHE_S3_KEY_PREFIX` (default `rust/`). TLS is always used for the remote
cache.

```bash
# Never commit these files; this is a throwaway directory.
install -d -m 0700 "${TMPDIR:-/tmp}/marbles-cache-secrets"
printf '%s' "$DO_SPACES_KEY_ID"     > "${TMPDIR:-/tmp}/marbles-cache-secrets/key-id"
printf '%s' "$DO_SPACES_SECRET_KEY" > "${TMPDIR:-/tmp}/marbles-cache-secrets/secret-key"

docker build \
  --build-arg SCCACHE_BUCKET=fpl-sccache-cache \
  --build-arg SCCACHE_ENDPOINT=https://<region>.digitaloceanspaces.com \
  --secret "id=sccache_access_key_id,src=${TMPDIR:-/tmp}/marbles-cache-secrets/key-id" \
  --secret "id=sccache_secret_access_key,src=${TMPDIR:-/tmp}/marbles-cache-secrets/secret-key" \
  -t marbles:test .
rm -rf "${TMPDIR:-/tmp}/marbles-cache-secrets"
```

All four values must be present. If the endpoint, bucket or either secret is
missing, the build logs a notice and proceeds without sccache — it does not fail.

The key pair is passed to the Docker build as BuildKit secrets, never as build
args, so the values do not land in image metadata. Inside the build stage the
Dockerfile maps them to `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` because
those are the credential variable names `sccache`'s S3 backend reads. That is an
sccache implementation detail, not an AWS dependency.

## CI

Both workflows build without a cache by default. To enable the remote cache on
same-repo CI and image publishes, configure these **repository** settings
(Settings → Secrets and variables → Actions):

- variables: `SCCACHE_ENDPOINT`, `SCCACHE_BUCKET`
- secrets: `SCCACHE_ACCESS_KEY_ID`, `SCCACHE_SECRET_ACCESS_KEY` (the DO Spaces
  key pair)

The workflows pass the key pair as the `sccache_access_key_id` /
`sccache_secret_access_key` BuildKit secrets and add the two `--build-arg`
values only when all four are set. No `AWS_*` secret is read or required. When
they are absent, the image build runs uncached and emits a
`::notice title=sccache disabled::` annotation.

Fork PRs do not receive repository secrets. They still run fmt/clippy/test; the
private Bob registry image means the container build is skipped with an explicit
notice rather than a 401. This decision depends only on the FPL registry client
certificate, not on cache credentials.

## Rotating or disabling

- Rotate the DO Spaces key pair in the repository secrets; the next build uses it.
- Remove either the variables or the secrets to fall back to uncached builds.
- Scope the Spaces key to the cache bucket only. Do not reuse it for anything
  else.
