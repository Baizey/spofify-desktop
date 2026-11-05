# Desktop checks: scope, commands and evidence

Use the [dedicated-worktree/review workflow](worktree-review.md), [storage preflight](storage-hygiene.md) and [native UI gate](ui-validation.md). This is Rust/Slint on Linux, not Android. There is no checked-in all-in-one acceptance runner.

## Choose proportionate checks

| Change | Required evidence |
| --- | --- |
| Workflow/docs only, no app/build behavior | Inspect full diff, local Markdown links/anchors and command claims; `git diff --check`; independent exact-SHA docs review; repeat relevant docs checks on actual main. **No Cargo tests/build or native launch just for documentation.** |
| Rust/protocol/security behavior | Formatting, relevant unit/mock tests and default build; focused owned synthetic integration for affected contracts; honest remaining coverage. |
| UI or user-visible interaction | Applicable automated checks plus real native screenshots **opened and inspected**, actual interaction and proportionate same-peer rendered review. |
| Unchanged code/artifacts | Reuse exact-source evidence with hashes/source attribution and explain why it remains applicable. No automatic replay of the full historical MVP campaign. |

Automated, mock, native output and physical audibility are separate claims. Historical totals are not a test result for a new candidate. New device/queue/playlist/Home/profile features need their assigned acceptance, not a decorative control or source-only assertion.

## Rust commands inside an owned BUILD lifecycle

These are command fragments, **not** a supervisor/lock/cleanup runner. From the intended worktree, after early BUILD acquisition, capacity preflight and setting owned `TMPDIR` / `CARGO_TARGET_DIR`, run with bounded logs/runtime and at most two Rust jobs:

```sh
unset SLINT_EMIT_DEBUG_INFO SLINT_MCP_PORT SLINT_TEST_SERVER SPOFIFY_DEV_SIZE
export CARGO_BUILD_JOBS=2
cargo fmt --check
cargo test --offline --locked -j 2 -- --test-threads=2
cargo build --offline --locked -j 2
```

Rust >=1.92 and pinned `Cargo.lock`; native fontconfig/ALSA development libraries must already be available. If necessary, point `PKG_CONFIG_PATH` at an already provisioned installation as described in [README](../README.md#build-and-run), not new global packages. Missing offline dependencies are a concrete provisioning blocker; do not silently fetch, alter pins or bypass denials.

For affected native inspection only, explicitly build a separate debug artifact:

```sh
SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked -j 2 --features slint/mcp
```

Record/copy the intended artifact to its own path and hash **before** another feature build replaces `target/debug/spofify-desktop`. Keep default and MCP binaries separately attributed. Default builds exclude MCP; release entry rejects inspection environment opt-ins. If build/inspection configuration changes, verify the ordinary feature graph with `cargo tree --offline --locked -e features -i slint` and absence of MCP, plus appropriate owned ordinary-runtime checks. Do not launch a compile implicitly through `cargo run` while holding NATIVE.

Use supported scoped test filters when sufficient, with full applicable tests for affected shared contracts. No synthetic clock or mocked server proves native playback/interoperability. Fixtures must be owned, isolated and synthetic; Go/mobile source remain read-only. Record exact Go source/binary hashes before relying on a real-Go fixture, never use a live server/account or put data into its checkout.

## Native and actual-main accounting

Release BUILD after all writers stop, then acquire NATIVE and reuse the exact binary as described in [UI validation](ui-validation.md). Keep `/dev/shm` lifecycle in the same invocation; no build/native overlap. Bounded native children must be awaited before cleanup. The only tracked helper, `scripts/inspect-native.py`, captures an already-running opt-in window; it does not launch, authenticate, exercise a journey or shut it down.

Historical `.tmp/web-vibe/build-check.sh`, `check-actual-main.sh` and campaigns are local evidence, **not supported fresh-run commands**: fixed old output paths, implicit build/native coupling and old guard behavior must not be reused as current workflow. Never overwrite their artifacts. No mobile runner, invented gate or active target cache is assumed.

After reviewed local squash, check actual main's SHA, intended paths/delta and clean tracked tree/index; repeat only checks relevant to this unit. A root build needs BUILD and source-mutation coordination, not a MAIN guard. If source changes mid-check, report invalid attribution. Combined smoke checks supplement individual features; the parent accounts for interaction coverage without withholding unrelated finished deliveries.

## Known baseline, not a current test claim

At desktop `9899a1a824f5633230291916ca8243a3eae9fa5c`, retained `.tmp/web-vibe/ROOT-FINAL.md` records default-feature build/tests and healthy ordinary native startup without MCP, and focused real-Go rendered interaction at 900×640 and 1100×760. [Validation history](validation.md) preserves prior successes/failures and audibility limits. Historical failures are not current blockers; neither does past success validate changed source. This workflow synchronization ran no Cargo or native campaign because product bytes were unchanged.
