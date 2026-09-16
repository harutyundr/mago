---
name: upstream-sync
description: Rebase the patch branch onto upstream carthage-software/mago and publish release binaries + container images via scripts/update-upstream.sh. Use when upstream has new commits or a new image build is needed.
---

# Upstream Sync + Publish Workflow

The patches live on `feature/type-inference-enhancement` — the branch is the
single source of truth; there is no patch file anymore. `scripts/update-upstream.sh`
automates the sync, and `PUBLISHING.md` documents the release artifacts in full.

## The flow

### 1. Dry run

```bash
scripts/update-upstream.sh --dry-run
```

Reports how many commits the branch is ahead of and behind `upstream/main`
without changing anything.

### 2. Rebase + test

```bash
scripts/update-upstream.sh
```

Fetches upstream, rebases the branch **directly onto `upstream/main`**, then
runs `cargo test --workspace --locked --all-targets`. Local/origin `main` is not
part of the flow; the GitHub fork page showing `main` behind upstream is
cosmetic and expected.

On conflicts the script stops. Resolve each conflicted file, `git add` it,
`git rebase --continue`, then re-run the script.

### 3. Regression check (manual)

The script runs unit tests but not the real-world type-inference check:

Verify against an actual XF add-on which has `.mago-analyzer-baseline.toml`.

New warnings from XF framework code go into that project's `.mago-analyzer-baseline.toml`. New warnings from our own addon code get fixed,
not baselined.

### 4. Publish

```bash
scripts/update-upstream.sh --tag
```

Pushes the branch and creates a `v<mago-version>-<short-sha>` tag. The tag
triggers the Publish workflow, which builds release binaries (attached to a
GitHub release) and pushes the multi-arch (`amd64` + `arm64`) image to every
registry configured in repository secrets, tagged `latest`, `<version>`, and
`<version>-<sha>`. Registry destinations are secrets — never named in the
workflow file or docs.

Monitor: `gh run watch -R harutyundr/mago $(gh run list -R harutyundr/mago -L 1 --json databaseId -q '.[0].databaseId')`

### 5. Consumers

Projects pick an image slot via `mago.version` in `.devbox.yml` (or the
`CI_MAGO_VERSION` CI variable / `--version` on `php:mago:format`); empty means
`latest`. Devbox's `php:mago:init` validates the existing `mago.toml` against
the resolved image and regenerates it when the schema drifted, so bumping a
pinned version usually needs no per-project fixes. Hosts that install a binary
rather than run the image use `~/devbox/bin/install-mago.sh` (release assets or
image extraction) or compile via `~/devbox/bin/build-mago.sh`.

## Conflict knowledge (accumulated)

- `crates/codex/src/scanner/function_like.rs` — return type inference vs upstream flags/checks
- `crates/analyzer/src/statement/class_like/initialization.rs` — constructor fix vs upstream API changes
- API drift to watch: `get_class_name()` return type, `declaring_property_ids` key type
- New `unknown-ref` errors after a rebase → check `crates/codex/src/populator/return_hints.rs`

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Rebase conflicts | Resolve by hand per the script's instructions, `git rebase --continue`, re-run |
| Compile errors after rebase | Upstream changed method signatures — see API drift above |
| Tag already exists | Same HEAD was published before; the exact-build image tag is already up. Advance HEAD or delete the local tag first |
| One registry fails at publish | Registries publish independently; fix that registry's secret triple and re-tag |
| `insufficient_scope` on push | Registry token needs both `read_registry` and `write_registry` scopes (see PUBLISHING.md) |
