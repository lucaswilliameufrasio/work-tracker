# ADR 0009: Secrets from the environment, and deployment through Uirapuru

- **Status:** Accepted
- **Date:** 2026-11-12
- **Context:** [ADR 0003](0003-totp-secret-encryption.md), [ADR 0008](0008-replication-to-s3-compatible-storage.md)

## Context

The master key lived only in `master.key` in the data directory. The TOTP secrets, the Muira client secret, and the email provider keys are encrypted with keys derived from it. A database restored from S3 after the server is lost cannot decrypt them without that file.

The operator asked whether Puxiri could use Ciphera, Infisical, or a self-hosted Vault, configured from the UI, and will deploy through Uirapuru. Uirapuru already fetches its `secure` variables from Ciphera and hands them to the container as environment variables, without passing them through its hub or writing them to disk.

## Decision

- Puxiri reads secrets from its environment and has no client of its own for Ciphera, Infisical, or Vault. Ciphera reaches it through Uirapuru or `ciphera run`, Infisical through `infisical run`, and Vault through Vault Agent or an equivalent that sets the environment.
- `PUXIRI_MASTER_KEY_BASE64` supplies the master key as base64 for exactly 32 bytes. Nothing is written to disk. If a `master.key` file already exists at the configured path, it must hold the same key or startup fails, because a different key would make every encrypted secret unreadable. An invalid value also fails startup. Without the variable, the key file is read or created as before.
- Email senders are still configured from `/gestao` and stored encrypted under that master key.
- `deploy/uirapuru/compose.yaml` runs a published image with no build, no published ports, and no networks, which Uirapuru adds. `deploy/uirapuru/.env.example` marks each variable `@plain`, `@secure`, or `@required` for Uirapuru's variable discovery. The master key and the S3 credentials are `@secure`.
- `.github/workflows/publish-image.yml` publishes `ghcr.io/{owner}/puxiri` for `linux/amd64` on `v*` tags. The job checks `sqlite-replication` out next to this repository at a commit pinned in the workflow. `Cargo.toml` keeps its path dependency.
- Puxiri runs as one instance. On Uirapuru it uses the `recreate` strategy with one replica. The compose has no healthcheck, so Uirapuru does not default it to blue-green, which would start a second container on the locked volume.

## Alternatives considered

- Native clients for Ciphera, Infisical, and Vault, with their settings in the UI. Each needs a bootstrap credential. If it is stored from the UI, it sits in the database beside the data it is meant to protect, and three clients would need their own maintenance and tests. The environment already reaches all three.
- A git dependency on `sqlite-replication` pinned in `Cargo.lock`. It makes the pin part of the build, but it changes how both repositories are developed side by side. The workflow pin is enough for published images.
- One compose for both cases. The self-hosted compose builds from source and publishes a port; Uirapuru expects an image and manages ports and networks.

## Consequences and limits

- Whoever sets the environment can read the master key from the process environment. Puxiri does not remove it from its own environment.
- The workflow pin and the local `../sqlite-replication` checkout can drift. Bump `SQLITE_REPLICATION_REV` when the library changes.
- The image is published only for `linux/amd64`, the platform where replication capture is validated.
- A GHCR package starts private. The Uirapuru server needs registry credentials, or the package has to be made public.

## Evidence

- On 2026-11-12 the operator agreed to read secrets from the environment and to take the master key from a variable instead of adding native integrations.
- Uirapuru's `docs/environment-variables.md` and `docs/decisions.md` (2026-10-26) describe `secure` delivery from Ciphera, `.env.example` annotations, and blue-green as the default with a route and a healthcheck.
- Locally the Uirapuru compose ran with `PUXIRI_MASTER_KEY_BASE64` and wrote no `master.key`, and an invalid value stopped startup.
