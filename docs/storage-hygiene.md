# Owned storage and bounded lifecycles

Applies to every lead and descendant, including tests, captures, fixture runtimes and recovery. Follow [worktree/resource ownership](worktree-review.md); serialization alone does not prevent accumulated scratch exhausting quota.

## Before allocating

- Record run/owner, worktree/candidate, exact unique scratch/output/evidence paths, estimated peak, finite budget and cleanup responsibility **before mkdir/restore**. Check bytes/inodes on scratch, output and `/tmp`, plus visible quotas. `df` availability is not proof of user/project quota headroom; report unknown quota honestly.
- For Rust/Slint artifact work, use a monitored **6 GiB maximum owned scratch budget only when current capacity plus safety headroom allows the estimated peak**. Otherwise stop/replan; do not allocate blindly or grow the limit to hide a failure. Max two Rust jobs; bound test concurrency, runtime, logs and captures. No hmem, package/global configuration changes or shared-cache sweeps.
- Set `TMPDIR` and `CARGO_TARGET_DIR` to the owned run paths for processes that produce artifacts. Record any fixture/database/cache location separately; never put synthetic runtime data in a Go checkout or assume a FUSE-backed path supports every native database operation.
- Reuse only verified, explicitly owned compatible caches/artifacts. Record source, feature set, toolchain, hash and cache origin. A target archive is an optimization, not evidence or a source backup; none is assumed active. Never overwrite retained MVP/web-vibe binaries/evidence.

## One invocation owns ephemeral work

With pi.lot's per-Bash mounts/namespaces, keep **`/dev/shm` setup → cache restore → build/tests → optional exact-binary native run → cache/evidence save → cleanup in one Bash invocation**. Do not expect an ephemeral path, process or namespace-local endpoint to survive between calls. Acquire BUILD before protected restore/build activity; release it before standalone NATIVE; see [separation and optional reacquisition](worktree-review.md#build-and-native-resource-ownership). Cache saves with shared writers need BUILD too, never nested with NATIVE.

Use a supervising shell/process with bounded timeouts and cleanup on success/failure/cancellation. Monitor actual growth; EDQUOT/ENOSPC or unexpected growth stops artifact production pending investigation, not blind retries. Keep tool output bounded because tool capture itself may spill to OS temporary storage. Preserve first failure separately from cleanup errors.

Await actual app/fixture/build/monitor descendants before releasing their resource ownership or deleting files; launcher exit or timeout alone is insufficient. Preserve required small logs, hashes, reports/screenshots and source recovery outside scratch and verify them. Remove only the exact owned disposable paths after confirming ownership, no escaping symlinks and no active writers; verify absence. Unknown liveness/ownership requires coordinated recovery, not an unconditional cleanup trap. No blanket `/tmp`/cache deletion, wildcard sweeps or touching user windows/binaries.

## Delegation and receipts

Assignments explicitly name [AGENTS](../AGENTS.md), this policy, ownership/budget, allowed retention and cleanup duties; require the child to read them. Each handoff records generated paths, checks/quota uncertainty, awaited children, cleanup performed (size when measurable), retained evidence and unresolved artifacts. The parent accounts for idle/crashed descendants; an idle agent or moved directory does not prove cleanup.

Documentation-only edits use small owned evidence, no build caches or native scratch. Preserve source/review receipts and remove the dedicated worktree/ref through ordinary non-forced Git after integration; see [cleanup](worktree-review.md#evidence-and-non-forced-cleanup).
