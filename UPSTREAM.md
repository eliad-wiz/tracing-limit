# Upstream relationship

This repository is a Wiz fork of the `tracing-limit` crate from
[vectordotdev/vector](https://github.com/vectordotdev/vector), extracted from the
`lib/tracing-limit` subdirectory into a standalone repository.

The crate originated in Vector itself (`Initial rate limit subscriber`, vector#494,
July 2019) as `lib/trace-limit`, and was renamed to `lib/tracing-limit` in vector#608.
It was never vendored from a third project, so Vector is the true upstream. The code is
MPL-2.0, same as Vector.

## Branches

| Branch | Contents |
| --- | --- |
| `upstream` | Verbatim extraction of `lib/tracing-limit` from Vector. Never hand-edit. |
| `wiz` | Default branch. Wiz's fork, which shares real history with `upstream`. |

`wiz` was grafted onto `upstream` at `5d6da996d4` (Vector `5e34065f5e`, 2022-10-07),
the upstream revision the crate was copied from when it landed in the Sensor tree on
2022-10-31. `git merge-base wiz upstream` resolves there, so upstream merges behave
like an ordinary merge rather than an unrelated-histories dance.

Note that the initial Sensor commit already carried Wiz modifications, so it is not a
pristine copy of that upstream revision.

## Pulling in the latest upstream

```sh
./scripts/sync-upstream.sh
```

The script re-extracts `lib/tracing-limit` from a fresh Vector clone and fast-forwards
the `upstream` branch, then leaves you to merge into `wiz`.

The extraction is reproducible: `git filter-repo` rewrites commits deterministically, so
re-running it over a newer Vector clone reproduces the same commit IDs for history that
hasn't changed and simply appends the new commits. That is what makes `upstream` a
fast-forward every time. Two consequences:

- Do not change the `--path` or `--path-rename` arguments. Different arguments produce
  different commit IDs, which turns the next sync into a force-push.
- Both `lib/tracing-limit` and the pre-rename `lib/trace-limit` must stay in the path
  list, or the crate's first commit is silently dropped.

## Expect conflicts in two places

`src/lib.rs` has diverged substantially in both directions — Wiz added a
`RateLimitConfiguration` builder, a per-callsite count field and continuous-time mute
window aging, while upstream has since reworked the file and enabled
`clippy::pedantic`. Merges touching it need real review, not a mechanical resolution.

`Cargo.toml` conflicts on essentially every sync because upstream inherits from Vector's
workspace (`[lints] workspace = true`, `tracing-subscriber = { workspace = true }`),
which cannot resolve in a standalone crate. If this becomes tiresome, adding a
`[workspace]` table here with matching `[workspace.dependencies]` and `[workspace.lints]`
would let upstream's manifest apply almost verbatim.
