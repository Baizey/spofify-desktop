# PAR-007 — Negotiated priority actions on selected capable outputs

**Status:** specified, unassigned. **Prerequisite:** 002. Not dependent on local full queue/receiver or playlists; existing search rows supply a complete native→browser feature. Follow [delivery guide](README.md).

## Wire and UX contract

Reference G `static/{player,playback-sync}.js`, `internal/web/{queue_intent,queue_intent_dto}.go`, `testdata/queue-intents-v1.json` and `testdata/queue-intents-pause-receipt-v1.json`. D extends typed API/controller/selection state, not a second queue engine.

1. Probe **GET `/api/me/player` with `X-Spofify-Playback-Extensions: queue-intents-v1`**, recognize strict optional `queueIntents` v1. No speculative write probe/general version API. Controller opt-in is not receiver advertisement: registration stays legacy until 008 fully passes.
2. Menus expose named Play next / Queue high priority / Queue low priority / Play now, using `command.type:"queueIntent"`, intent `next|high|low|now`. Freeze `expected={outputDeviceId,receiverId,revision,currentKey,currentEventId}` once at gesture/menu activation lifetime, not after awaits or a newer snapshot. Session/route/selection/semantic ABA cancels; progress/duration alone does not stale an action. Selected output must be available/capable with matching binding. No output selected: tell user to select an output, **no self-admission** (including High/Now). Mobile High exception is separately authorized optional work.
3. Selection union: track with positive track metadata/context (no new key/order), full collection context, or exact existing upcoming occurrence key. Manual selection preserves entry ID; derived selection preserves target/occurrence/signed selection authority when 017 is available. Wire newly supported actions to already-landed surfaces; later surfaces reuse this route. Never substitute legacy `enqueue`/`enqueueContext` or use track ID to disambiguate duplicates.
4. Low appends; High inserts after last pending High; Next directly after current and joins protected High; Now replaces context/selects requested start or jumps to exact existing upcoming occurrence. Insertion onto an existing paused/ended/blocked/media current **does not start/resume/retry**. Only exact current-occurrence change (including empty→selected) attempts playback. This is receiver-authoritative, not optimistic local playback.
5. Parse complete extension `{version,receivers,binding,state,directives,results}` and truthful result `{outcome,acceptedCount,commitRevision,placement,playback,playbackKey,notice}`. Handle playing/queued/limited/no_op/full/blocked/stale/unavailable/unsupported/media and superseded playback without inventing success from HTTP or a later Playing snapshot. Pending/unknown completion is inspect-and-act-again, never replay/rebind. Keep bounded safe notice mapping.

## Required capability matrix

| Deployed observation / output | Controller behavior |
| --- | --- |
| Old server/no recognized extension | Ordinary supported legacy transport remains; priority unavailable/update guidance; no extension mutation probe |
| Current extension + legacy output (including desktop before 008) | No priority send/fallback; identify unsupported receiver |
| Current extension + selected capable output + valid binding | Send exact frozen intent once; show detailed authoritative result |
| Missing/malformed binding, unavailable output, no selected output | Refuse safely with truthful guidance, no takeover |
| Capable→legacy→capable or receiver replacement | Old held binding is stale; observe fresh state for a **new** gesture, never refresh held intent |

## Tests / native acceptance

Use byte-exact shared golden fixtures (verify provenance/hash), strict missing/duplicate/case-variant/unknown fields, aligned directive metadata and detailed results. Test each matrix row; pending/final/lost result, semantic A→B→A, output/session replacement, benign progress, old-menu action and 401/409/oversize. Include paused A + High X remains paused, empty addition starts only at receiver, same-current no-op, exact duplicate occurrence, full/partial/publication limit, blocked/media and Pause during held start → `superseded`.

Native menus against actual owned current-browser receiver execute all four intents with correct ordering/outcomes and **no local media/output**. Capture keyboard/menu/focus, stale/unsupported/limited feedback and bottom continuity; no popup/Close player. Play/queue feedback is authoritative acceptance vs started, not an unconditional toast. Legacy receiving remains unadvertised after this complete controller unit.
