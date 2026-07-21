# Upstream sync — manual resolution required

Generated: 2026-07-21T08:14:52Z
Upstream:   https://github.com/nearai/ironclaw.git @ main
Upstream commit: 400001e3b5f2e6f0fc62a9f4a3f49cbad5007186
Behind by:  116 commits

The automated 3-way merge on top of `origin/main` produced conflicts.
The merge was aborted before any conflict markers were committed, so
this branch currently contains only this notes file on top of
`origin/main` — that is by design.

## Conflicting paths

```
CLAUDE.md
src/agent/agent_loop.rs
src/agent/commands.rs
src/agent/heartbeat.rs
src/main.rs
src/safety/mod.rs
```

## How to resolve

```bash
git fetch origin "chore/upstream-sync-2026-07-21-400001e" && git switch "chore/upstream-sync-2026-07-21-400001e"
git remote add upstream https://github.com/nearai/ironclaw.git 2>/dev/null || true
git fetch upstream main
git merge upstream/main
# resolve, then:
git rm UPSTREAM_SYNC_NOTES.md
git commit
git push --force origin "chore/upstream-sync-2026-07-21-400001e"
```

Then update the PR body / drop draft state and merge.
