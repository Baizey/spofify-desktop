# Roles and delivery contract

Read [AGENTS.md](AGENTS.md) and the sections for your role. This is the desktop adaptation of the pinned mobile workflow; see [provenance](docs/worktree-review.md#provenance).

## Everybody

- Deliver small, independently usable features: each has a scoped brief/spec, dedicated branch/worktree, implementation, appropriate tests, one independent reviewer and its own integration into main.
- Land the smallest genuine prerequisite first. Shared-file overlap needs coordination, not a waterfall mega-branch, unfinished handoff or central integration agent. A combined smoke pass supplements individual delivery, not an all-or-nothing batch gate.
- Use normal Git internal locks only: **no custom MAIN/Git lock, guard, flock, semaphore or persistent reservation**. Coordinate short root writes and root builds with the parent/other leads. BUILD and NATIVE safeguards are separate; see [worktree/review](docs/worktree-review.md).
- Clean up your own work and account for descendants. Preserve bounded source/review/evidence before removing only completed owned worktrees/refs and disposable scratch. Never sweep another lane's data or the user's existing windows/binaries. Follow [storage hygiene](docs/storage-hygiene.md).

## Orchestrator

Give the user request to an analyzer for scoped documentation/specs and dependency analysis, then assign each ready deliverable to a lead. Coordinate ownership/prerequisites and check liveness/completion without repeatedly redistributing unfinished implementation. The assigned lead owns implementation, tests, independent review and integration. Account for combined validation and abandoned scratch; do not become a central batch integrator.

## Analyzer

Inspect actual desktop and read-only reference behavior before splitting the request. Return concise scoped briefs/specs (under `docs/specs/` when useful) with acceptance, exclusions, dependencies, affected interfaces and validation. Prefer independently deliverable features; document genuine prerequisites. Do not recreate deleted historical task boards or treat old MVP exclusions as current authorization limits.

## Assigned lead

Create/manage the dedicated branch/worktree from the agreed pinned base. Implement and test the complete assigned feature, obtain one independent read-only exact-source review, fix substantive issues and use the same peer to close them. With assigned-scope integration authority, land the reviewed feature locally in main under [the guarded workflow](docs/worktree-review.md#local-squash-integration); approval is not self-review, remote publication or permission to overwrite others. Escalate conflicts outside your scope; a conflict lead resolves that conflict, not the whole backlog. Report actual-main SHA, evidence/limitations, preservation and cleanup.

## Independent reviewer

Read the assignment and relevant desktop guidance; review the exact candidate/base and evidence without editing, staging or committing. Check correctness, security, scope and proportionate tests. For UI, actually inspect affected native after-screenshots and interaction evidence using [UI validation](docs/ui-validation.md); source/tree/mock results are not rendered acceptance. Identify substantive findings versus nits; recheck fixes and affected evidence with the same peer rather than restarting unchanged campaigns.
