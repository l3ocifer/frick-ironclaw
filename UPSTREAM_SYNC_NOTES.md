# Upstream sync — manual resolution required

Generated: 2026-07-23T08:00:13Z
Upstream:   https://github.com/nearai/ironclaw.git @ main
Upstream commit: 68b4ded71deda6033a750bbc322dcca433243a60
Behind by:  163 commits

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
git fetch origin "chore/upstream-sync-2026-07-23-68b4ded" && git switch "chore/upstream-sync-2026-07-23-68b4ded"
git remote add upstream https://github.com/nearai/ironclaw.git 2>/dev/null || true
git fetch upstream main
git merge upstream/main
# resolve, then:
git rm UPSTREAM_SYNC_NOTES.md
git commit
git push --force origin "chore/upstream-sync-2026-07-23-68b4ded"
```

Then update the PR body / drop draft state and merge.
