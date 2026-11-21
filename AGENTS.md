# spofify-desktop

hmem is authoritative for agent practices, planning, specifications, tasks, reviews and handoffs.

1. Initialize hmem with `set_workspace(workspace_id: null, identity: {role, "model-id": "harness:model:reasoning"})` using your actual identity.
2. List workspaces, then select **Desktop application** (`41c2649a-9891-4b95-b600-ff69cfd13228`).
3. Read **Desktop working practices** (`b626449f-38c8-4586-b4d3-44352156068b`) and technical information (`210adbe4-9608-4778-b0bf-def61b769fea`), then the relevant hmem topics and existing assigned tasks. Follow shared-practice/planning links there.

Do not maintain local policy copies, plans, task boards or handoffs. If hmem is unavailable, report the blocker; do not revive legacy Markdown guidance. Never upload secrets or transcripts. Ordinary README/API/technical documentation remains source material, not task state.

## User-facing design and information

This applies to every user-facing surface in Spofify, including admin tools, setup, settings, lists, dialogs, empty and error states, and notifications.

- Build for direct understanding. Use hierarchy, grouping, familiar controls, clear labels, and visible state so users can tell what matters and what they can do without reading instructions. If prose is needed to explain the interface, improve the interface first.
- Show the information itself: a heading, value, status, or action. Trust familiar terms. Delete redundant copy rather than shortening it. No default subtitles, tooltips, or filler.
- Add short, specific context only when it changes a decision, makes a consequence clear, or helps someone recover. Put optional details on demand and implementation mechanics in docs. Preserve accessible names and states.
- Review the whole surface before shipping. Remove anything the design or another label already communicates.
- Verify alignment in the rendered UI before approving a visual change. On one representative display for the platform, check shared edges, text baselines, icon and control centers, and spacing in the affected states. Use a running app, preview, or screenshots; code inspection alone is not enough. Note anything you could not verify.
- Do not offer a “no playback device” choice or a Deselect action. Keep an eligible playback device selected whenever available, without taking over a different device’s active output. Real disconnected or offline states must not show a fake selection.
- Do not show retry buttons or equivalent retry actions. Automatically retry transient failures a reasonable, bounded number of times with backoff; retry writes only when safe to repeat. Stop on permanent failures or cancellation and show the resulting state without asking the user to retry.
