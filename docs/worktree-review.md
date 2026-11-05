# Dedicated worktrees, review and local integration

Read [AGENTS](../AGENTS.md), [role contract](../AGENT_BEHAVIOUR.md), the current assignment and [testing](testing.md). Finished assigned units land in main, not an indefinite integration branch. Assignment grants only its stated scope; reviewers/helpers have no integration authority.

## Provenance

Workflow adapted from read-only `spofify-mobile` **`5b2422f645bc461af3e8419cd6af3399b33fc940`**: `AGENTS.md`, `AGENT_BEHAVIOUR.md`, `docs/{worktree-review,storage-hygiene,testing,ui-validation,mobile-ux}.md`, verified at synchronization. Desktop baseline: `9899a1a824f5633230291916ca8243a3eae9fa5c`. Mobile supplied delivery/resource/review lessons, not desktop product or security policy. No Android tooling, remembered-login behavior, permissive HTTP policy or phone navigation conventions are imported.

The current desktop/browser parity request authorizes device playback, playlists (creation in Playlists), search and Home with unified native header/profile. Old search-only MVP descriptions record shipped coverage, not permanent exclusions. Scope still comes from the current assignment; do not expand into backend/admin parity or copy the browser's separate topbar/now-playing popup. Keep player capabilities in the bottom player and preserve desktop security rules.

## Start and freeze a delivery unit

1. Record absolute main/worktree paths, branch, clean tree/index, full base SHA, assigned paths, acceptance and exclusions. Coordinate known writers. Create a dedicated branch/worktree from that exact base; do not develop product code in main. Inventory/preserve existing work before migration or reassignment; never reset/stash/restore another lane or overwrite unknown files.
2. Keep outputs worktree-local or in recorded unique owned scratch. No shared active target assumption. Follow [storage hygiene](storage-hygiene.md) before allocation and [resource ownership](#build-and-native-resource-ownership) before protected activity.
3. Run relevant checks, `git diff --check`, and commit only assigned paths. Record full candidate/base SHAs, commands/results, binary hashes where relevant, evidence and blockers. A checkpoint or build is not native acceptance.
4. Obtain **one independent read-only review of the frozen exact candidate against the validated base**. Supply requirements, full diff and relevant source/tests/renders. Fix substantive findings in new scoped commits; the same peer verifies the new SHA and affected checks. Reuse unchanged evidence, not repeated whole historical campaigns. Record approval outside the frozen candidate to avoid self-reference. Changed source/base requires renewed applicable review, not inherited approval.

## BUILD and NATIVE resource ownership

Use normal Git internal locking for all worktree/index/ref/integration operations. **Never add a custom MAIN/Git guard or flock.** Brief root-write/build communication is not a Git lock protocol.

pi.lot's `docs/policy.md` (Filesystem mediation) and `docs/security.md` (Known limitations), checked in `/var/home/lsm/repos/pilot` at `b1a4d289f04bf2ad633a01f2816fef7e223f9ec6`, state that backing-file POSIX/`flock` locks do not coordinate with host processes or separate Bash mounts. Therefore flock alone is insufficient. Resolve the absolute shared common directory with `git rev-parse --path-format=absolute --git-common-dir`, not a worktree-local gitdir.

For participating desktop writers sharing build/cache or native test resources, use this resource-specific protocol (not a supplied executable runner):

| Resource | Host-atomic ownership directory under absolute common dir | Persistent advisory flock |
| --- | --- | --- |
| BUILD: Rust/Slint builds, tests and shared cache restore/save | `spofify-build.guard` | `/var/home/lsm/repos/spofify-go/.tmp/feature-orchestration/measurements.lock` |
| NATIVE: owned native test window/fixture, shared test ports/capture state | `spofify-native.guard` | `<common-dir>/spofify-native.lock` |

- Acquire early, before restore/build or native launch: successful atomic `mkdir` of the guard (**not** `mkdir -p` or check-then-create), then record unique owner/run token, worktree, candidate and command. Existing guard is busy; use a bounded wait or stop. Never steal a guard.
- Acquire the corresponding exclusive flock with finite wait, e.g. `flock -x -w 60 <fd>`, and keep the descriptor open throughout the operation and descendant writers. No protected work before both succeed. Never unlink/replace persistent lock files.
- Retain crossproject heavy-work coordination at the exact Go `measurements.lock` above; no concurrent desktop/Android/server heavy builds. The desktop directory guard coordinates participating desktop writers only. **Neither that guard nor measurements.lock alone excludes host or other-project nonparticipants.** Coordinate with the parent/other project owners to exclude uncoordinated heavy activity; stop on uncertainty. Do not mutate Go/mobile instructions or invent a global guard/configuration to solve this.
- Release only after actual children/writers are stopped and awaited. Release flock, verify the guard's owner token matches the retained token, then remove only its own record and empty directory. Failed acquisition permits only owner-checked release after confirming no protected writers started/remain. Interrupted/uncertain shutdown or token mismatch: retain guard/evidence and request explicit coordinated recovery; age or PID absence in one namespace is not stale-lock authority.
- **BUILD and NATIVE are distinct, never nested.** Finish compilation/tests/cache writers and release BUILD before acquiring NATIVE for exact-binary reuse. They may occur in one Bash invocation to retain ephemeral scratch, with no concurrent build or implicit native-runner build. If cache saving after native needs BUILD, first stop/await native children and release NATIVE, then reacquire BUILD. Keep the complete ephemeral lifecycle in that invocation.
- Coordinate source immutability for any root/main build, verify intended SHA and clean source/index before/after; changed source invalidates attribution. No custom MAIN lock through compilation. Documentation-only work needs neither BUILD nor NATIVE.

## Local squash integration

With assigned-scope standing authority, briefly coordinate root writes with the parent/other leads (including root builds). Use ordinary Git locking and recheck state on every attempt.

1. Destination `/var/home/lsm/repos/spofify-desktop` must be on `main` at the exact reviewed base, with clean tracked tree/index and no unexpected untracked changes/writers. Normally require a fully clean destination. Only an explicit narrow user/parent exception may leave named unrelated untracked data; record it without inspecting/staging/deleting/ignoring/archiving that data. An exception never permits tracked/index dirt or unknown writers.
2. If main moved, stop. Normally merge new main into the **owned topic worktree** (no rebase/history rewriting), resolve only assigned conflicts, repeat relevant checks and independent exact-SHA review against the new base. Never catch up by editing main.
3. Verify topic tip equals the approved full candidate and the complete base-to-candidate path list/diff matches scope. Recheck dependent contracts and already-landed deltas. Unresolved findings stop integration.
4. `git merge --squash <full-reviewed-candidate>` in main. Inspect the entire staged diff and path list; run `git diff --cached --check` and compare it to the approved delta. Unexpected/conflicted staging: stop/report and preserve state, no reset/stash/revert/restore in main.
5. Commit a focused local squash identifying full base/candidate and peer approval. Verify complete intended tree/delta accounting, clean tracked tree/index and no new unexpected untracked changes; record full main SHA. Run proportionate actual-main checks from [testing](testing.md). Follow-up fixes return to an owned topic/review cycle, never direct main edits. No push/deployment/force operations.

## Evidence and non-forced cleanup

Inventory topic history, staged/dirty/untracked/needed ignored files and jobs. Preserve bounded non-sensitive source/history (candidate bundle plus patch), review/check receipts and relevant screenshots **outside** the worktree; verify hashes and bundle validity. Compare scoped source/tree or stable patch accounting with main; squash ancestry/`branch --merged` alone does not prove preservation. Unaccounted work keeps its ref/worktree.

After actual-main checks and confirmed child shutdown, verify the exact worktree is clean and all useful ignored artifacts preserved. From outside it, `git worktree remove <owned-path>` **without force**. A refusal stops cleanup; do not filesystem-delete the worktree. Remove only the accounted branch with expected-tip checking, e.g. `git update-ref -d refs/heads/<owned-topic> <reviewed-tip>`. Report main SHA, review, retained evidence, removed paths/ref and unresolved cleanup. Do not delete useful screenshots or old `.tmp/mvp` / `.tmp/web-vibe` binaries/evidence.
