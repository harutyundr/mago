# Publishing

This fork publishes two kinds of artifacts on every release tag:

- **Release assets** — prebuilt binaries for Linux (static musl), macOS, and
  Windows, attached to a GitHub release. Hosts install these directly; no Rust
  toolchain needed.
- **Container image** — a multi-arch (`linux/amd64` + `linux/arm64`) image
  carrying the static musl binaries, pushed to every registry configured in
  the repository secrets. CI jobs use the image.

The `Publish` workflow (`.github/workflows/publish.yml`) builds both from the
`feature/type-inference-enhancement` branch, which is the single source of
truth for the patches. This repository is public, so the workflow file and
this document deliberately do not name the destination registries.

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

Each publish pushes three tags to every configured registry:

| Tag | Meaning |
| --- | --- |
| `latest` | most recent publish |
| `<version>` | mago version from `Cargo.toml`, e.g. `1.24.0` |
| `<version>-<sha>` | exact build, pinned |

## Registry configuration (secrets)

The workflow supports up to two registries; each is a repository secret
triple on the fork. Empty slots are skipped, and one registry failing does
not block the other.

| Secret | Value |
| --- | --- |
| `REGISTRY_1_IMAGE` / `REGISTRY_2_IMAGE` | full image reference including the registry host, e.g. `<host>/<group>/<project>/<image>` |
| `REGISTRY_1_USER` / `REGISTRY_2_USER` | registry login name |
| `REGISTRY_1_TOKEN` / `REGISTRY_2_TOKEN` | registry password; a token with `write_registry` (or equivalent) scope |

Pushed references are masked in CI logs (GitHub secret masking), and the
workflow additionally rewrites the reference and host out of any captured
tool output before printing it.

## Using the image in CI

```yaml
mago:
  image: <image reference from REGISTRY_1_IMAGE>:latest
  script:
    - mago analyze
```

The image is based on Alpine and contains only `mago` and `git`. The binary
is statically linked (musl), so the image works on both amd64 and arm64
runners with no emulation.

## Extracting the binary from the image (Linux hosts)

The binary in the image is static, so it runs on any Linux once extracted —
no glibc dependency:

```sh
IMAGE=<image reference from REGISTRY_1_IMAGE>
docker pull --platform linux/amd64 "$IMAGE:latest"
cid=$(docker create --platform linux/amd64 "$IMAGE:latest")
docker cp "$cid":/usr/local/bin/mago ./mago && docker rm "$cid"
```

macOS and Windows cannot run the Linux binaries from the image; install the
matching release asset instead (`~/devbox/bin/install-mago.sh` does this
automatically, and also offers `--source=image` on Linux hosts).
