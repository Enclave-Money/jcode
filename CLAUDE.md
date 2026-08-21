<!-- gitnexus:start -->
# Repo graph — GitNexus (CLI mode)

`blaude-agent` is indexed as a knowledge graph of 54204 symbols and 137029 relationships across 300 execution flows. Query it with the commands
below **instead of grepping** when you need structure: callers, callees, blast
radius, execution flows.

No MCP server is installed by design — these commands cost nothing until you run
one, whereas MCP tool schemas would cost tokens on every turn.

## Commands

Run from the repo root. All output is JSON.

| Need | Command |
|------|---------|
| What calls this? What does it call? | `node .gitnexus/run.cjs context <symbol> --repo .` |
| What breaks if I change this? | `node .gitnexus/run.cjs impact <symbol> --repo .` |
| Find code by concept, not by string | `node .gitnexus/run.cjs query "<concept>" --repo .` |
| How does A reach B? | `node .gitnexus/run.cjs trace <from> <to> --repo .` |
| What did my diff actually touch? | `node .gitnexus/run.cjs detect-changes --repo .` |
| Is the index current? | `node .gitnexus/run.cjs status` |

`--repo .` is not optional: GitNexus keeps one global registry of indexed
checkouts, and two checkouts of the same repository — a `council` run creates a
git worktree per backend — make an unqualified command fail with *"Multiple
repositories indexed"*, as an unhandled stack trace rather than as JSON.

## Use it for

- **Before editing a shared symbol**, run `impact` and check the risk level.
  It reports the true blast radius from the call graph, not a guess.
- **Before committing**, `detect-changes` maps your diff to affected symbols and
  execution flows — it catches edits that reach further than intended.
- **When exploring unfamiliar code**, `query` returns flows ranked by relevance.
  It finds things a grep for the wrong noun would miss.

## Refresh

The index is per-commit. If `status` reports stale, run `blaude brief` again.
<!-- gitnexus:end -->
