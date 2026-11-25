# spofify-desktop

Do not use hmem. Use the current task brief and repository source, tests, and technical documentation. Never upload secrets or transcripts.

## Repository change policy

- Data: breakable (client-owned local data).
- Contracts: breakable.
- Behaviour: breakable.

## User-facing design and information

This applies to every user-facing surface in Spofify, including admin tools, setup, settings, lists, dialogs, empty and error states, and notifications.

- Build for direct understanding. Use hierarchy, grouping, familiar controls, clear labels, and visible state so users can tell what matters and what they can do without reading instructions. If prose is needed to explain the interface, improve the interface first.
- Show the information itself: a heading, value, status, or action. Trust familiar terms. Delete redundant copy rather than shortening it. No default subtitles, tooltips, or filler.
- Add short, specific context only when it changes a decision, makes a consequence clear, or helps someone recover. Put optional details on demand and implementation mechanics in docs. Preserve accessible names and states.
- Review the whole surface before shipping. Remove anything the design or another label already communicates.
- Keep lists continuously scrollable. Do not expose pagination, numbered windows, or manual “Load more” substitutes. Fetch in batches or virtualize rows internally when needed.
- Do not expose one-step Move up or Move down controls for reordering. Show the drop destination in the list during direct manipulation, and provide an accessible way to lift, place, or cancel without a series of one-step writes.
- Verify alignment in the rendered UI before approving a visual change. On one representative display for the platform, check shared edges, text baselines, icon and control centers, and spacing in the affected states. Use a running app, preview, or screenshots; code inspection alone is not enough. Note anything you could not verify.
- Show seek bar focus with the focus color on the control itself (thumb and active track or waveform), not with a surrounding border. This applies to all seek bars, including volume.
- Do not offer a “no playback device” choice or a Deselect action. Keep an eligible playback device selected whenever available, without taking over a different device’s active output. Real disconnected or offline states must not show a fake selection.
- Do not show retry buttons or equivalent retry actions. Automatically retry transient failures a reasonable, bounded number of times with backoff; retry writes only when safe to repeat. Stop on permanent failures or cancellation and show the resulting state without asking the user to retry.
