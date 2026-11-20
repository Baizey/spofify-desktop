# spofify-desktop

hmem is authoritative for agent practices, planning, specifications, tasks, reviews and handoffs.

1. Initialize hmem with `set_workspace(workspace_id: null, identity: {role, "model-id": "harness:model:reasoning"})` using your actual identity.
2. List workspaces, then select **Desktop application** (`41c2649a-9891-4b95-b600-ff69cfd13228`).
3. Read **Desktop working practices** (`b626449f-38c8-4586-b4d3-44352156068b`) and technical information (`210adbe4-9608-4778-b0bf-def61b769fea`), then the relevant hmem topics and existing assigned tasks. Follow shared-practice/planning links there.

Do not maintain local policy copies, plans, task boards or handoffs. If hmem is unavailable, report the blocker; do not revive legacy Markdown guidance. Never upload secrets or transcripts. Ordinary README/API/technical documentation remains source material, not task state.

## Product copy

- Use headings, labels, values, and status first. Show the data, not a sentence about the data: `Offline playlists` and `12 GB · 3 playlists`.
- Omit copy that does not help the user act or understand the current state. Trust familiar terms: `Crossfade` needs no definition, duration, subtitle, or tooltip on a simple switch. Delete redundant copy rather than shortening it.
- Add one short sentence only when it helps someone decide, avoid data loss, or recover from a problem. Say what happened and give the next action if there is one. Put mechanics and edge cases in docs.
- Keep accessible names and states clear without repeating visible copy. Before shipping, read the screen as a whole and cut anything the layout, labels, or values already say.
