# Publishing

This fork publishes two kinds of artifacts on every release tag:

- **Release assets** — prebuilt binaries for Linux (static musl), macOS, and
  Windows, attached to a GitHub release. Hosts install these directly; no Rust
  toolchain needed.
- **Container image** — a multi-arch (`linux/amd64` + `linux/arm64`) image at
  `registry.gitlab.com/xenreact/devbox/devbox/mago`, carrying the static musl
  binaries. CI jobs use the image.

The `Publish` workflow (`.github/workflows/publish.yml`) builds both from the
`feature/type-inference-enhancement` branch, which is the single source of
truth for the patches.

## When to rebuild

The patches track upstream `carthage-software/mago`. To rebuild on top of the
latest upstream:

```sh
scripts/update-upstream.sh          # fetch upstream, rebase, run tests
scripts/update-upstream.sh --tag    # push branch + tag → publishes everything
```

Rebase conflicts are resolved by hand — the script stops with instructions.
Optionally pass `--regen-diff` to refresh `~/devbox/patches/type-inferrence.diff`
from the branch. The loose diff file is a derived artifact; the branch is the
source of truth.

Tag names follow `v<mago-version>-<short-sha>` (for example `v1.24.0-a0a4570`),
so a rebuild after a rebase never collides with a previous tag.

## Image tags

Each publish pushes three tags to `registry.gitlab.com/xenreact/devbox/devbox/mago`:

| Tag | Meaning |
| --- | --- |
| `latest` | most recent publish |
| `<version>` | mago version from `Cargo.toml`, e.g. `1.24.0` |
| `<version>-<sha>` | exact build, pinned |

## Using the image in CI

```yaml
mago:
  image: registry.gitlab.com/xenreact/devbox/devbox/mago:latest
  script:
    - mago analyze
```

The image is based on Alpine and contains only `mago` and `git`. The binary is
statically linked (musl), so the image works on both amd64 and arm64 runners
with no emulation.

## Extracting the binary from the image (Linux hosts)

The binary in the image is static, so it runs on any Linux once extracted —
no glibc dependency:

```sh
docker pull --platform linux/amd64 registry.gitlab.com/xenreact/devbox/devbox/mago:latest
cid=$(docker create --platform linux/amd64 registry.gitlab.com/xenreact/devbox/devbox/mago:latest)
docker cp "$cid":/usr/local/bin/mago ./mago && docker rm "$cid"
```

macOS and Windows cannot run the Linux binaries from the image; install the
matching release asset instead (`~/devbox/bin/install-mago.sh` does this
automatically).

## Required secrets

The `docker` job pushes to GitLab using two repository secrets on the fork:

- `GITLAB_REGISTRY_USER` — username for the token below
- `GITLAB_REGISTRY_TOKEN` — a project access token on `xenreact/devbox/devbox`
  with the `write_registry` scope
