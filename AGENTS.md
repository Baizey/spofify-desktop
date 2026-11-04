# Development rules

- Develop each feature/fix in its own branch and dedicated worktree. The assigned lead creates/manages it from the agreed clean base; do not develop product code directly on main.
- Keep a short durable brief with scope, acceptance and exclusions. One independent reviewer checks the frozen candidate and relevant tests; the same reviewer verifies substantive fixes. Reuse unchanged evidence, not repeated full reviews.
- UI acceptance requires actual rendered Slint screenshots and real interaction/inspection, not a no-pixel testing backend or source-only assertions. Clearly distinguish headless component tests from actual-window evidence.
- The server and mobile repositories are read-only context. Never mutate or launch a working server, use real accounts, or change server security/configuration. Use owned synthetic fixtures.
- Enforce exact-origin networking, normal TLS verification, bounded requests/results and redirect refusal. Credentials/sessions must not appear in logs, URLs, screenshots or saved settings. No remembered credentials in the initial MVP.
- Enable MCP/system-testing only in explicit development/test builds, bind inspection locally, and never expose it on a LAN or silently include it in a release build.
- Before heavy builds check actual scratch bytes/inodes and estimated peak. Quota headroom may remain unknown despite free bytes. Use recorded unique owned scratch roots; finite /dev/shm is acceptable when checked. Bound build jobs/output/runtime and await app/fixture shutdown in finally paths. Preserve first failures and separate cleanup errors. Verify exact owned absence; never sweep caches, /tmp, evidence or unrelated worktrees.
- Rust/Slint builds are serialized for this work. Do not run Android, server and desktop heavy builds concurrently. No hmem. No global tool/harness changes.
- No remote publication, deployment, force pushes, history rewriting or policy workarounds. Normal package dependency access is separate from the existing blocked server-repository remote operation; stop on an explicit denial.
- After approval, guard source/base/tree/clean state, squash this unit locally, verify actual main, preserve concise source/evidence, then normally remove only the completed owned worktree/ref. Do not delete useful screenshots.
