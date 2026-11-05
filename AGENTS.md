# hmem bootstrap — spofify-desktop

Canonical repository and local checkout: `spofify-desktop`. The backend is **spofify-server**; `spofify-go` is its local checkout alias, not another project.

## Start in hmem

1. Initialize identity with `set_workspace` using `workspace_id: null`, your assigned role and `model-id` formatted `harness:model:reasoning` (use the actual harness, model and reasoning setting).
2. List workspaces, then select **Desktop application** (canonical repository `spofify-desktop`), workspace `41c2649a-9891-4b95-b600-ff69cfd13228`; group its Git worktrees with this repository.
3. Read project details for **Spofify Desktop** (`210adbe4-9608-4778-b0bf-def61b769fea`), the practices root below and the topics relevant to the assignment. Browse existing projects/tasks before creating records; use full IDs and explicit workspace boundaries.

## Desktop destinations

**Desktop working practices** — `b626449f-38c8-4586-b4d3-44352156068b`:

| Focused topic | Project ID |
| --- | --- |
| Build/native resource ownership and storage | `e147a114-57c7-4b19-a8bb-5ba7d9c8d5c1` |
| Roles, review, integration and evidence | `3a71f849-7e73-4122-9424-c59e6d80d663` |
| Product, security, playback and UX contracts | `aa15abef-6688-44cc-b289-8fccda731835` |
| Validation commands and native acceptance | `d3fa042f-a9ba-4ab6-994e-613384717591` |

## Shared destinations

- **General practice**, workspace `8c030c78-4418-4a51-9981-2b00b5da39ec`: **Spofify engineering practices** (`3fc89965-22a1-406f-9523-ee53c50a1439`) and **hmem knowledge and coordination model** (`575dbbbd-c662-402a-81a7-305171f94b72`).
- **Planning**, workspace `3f54babf-484d-492f-bf3b-ce136daa9b1c`: cross-repository information capture (`9379df08-a5ee-4b44-a687-d62053c5365e`) and architecture/product coordination (`84c6c8ef-b178-425b-977e-e49a795deaa2`). Browse project/task entity types in these nonrepository workspaces.

## Authority and access

hmem owns practices, planning, specifications, tasks, status, reviews and handoffs; it is not a mirror of local Markdown. Repository-manager agents own feature/bug/task migration and lifecycle. Task/spec migration and knowledge/practice migration are separately owned; neither belongs in an unrelated feature. Reuse existing PAR tasks; do not upload transcripts or secrets.

Do not maintain local shadow boards, plans, status checklists or handoffs. Legacy task-spec links and compatibility pointers are provenance, not instructions to resume local planning maintenance.

If hmem is inaccessible, report the access blocker; do not resurrect old local guidance as a fallback. This bootstrap grants no additional execution or integration authority. Normal README, technical/API/protocol/fixture documentation, historical evidence, assets and licenses remain source documents; source and measured evidence still ground technical claims.
