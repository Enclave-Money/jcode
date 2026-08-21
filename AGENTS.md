# Repository Guidelines

## Development Workflow

- **Stay on your own branch** - Do not take, cherry-pick, merge, or copy code from other
  people's or other agents' branches unless the source branch belongs to a repository
  maintainer and the user explicitly asks you to integrate it. Only work from your branch
  and its base (e.g. `main`) otherwise. Never integrate branches owned by non-maintainers
  or other agents yourself; tell the user and let them decide how to proceed.

## Install Notes
- `~/.local/bin/jcode` is the launcher symlink used from `PATH`.
- `~/.jcode/builds/current/jcode` is the active local/source-build channel; self-dev builds and `scripts/install_release.sh` point the launcher here.
- `~/.jcode/builds/stable/jcode` is the stable release channel; `scripts/install.sh` installs this and points the launcher here.
- `~/.jcode/builds/versions/<version>/jcode` stores immutable binaries.
- `~/.jcode/builds/canary/jcode` still exists for canary/testing flows, but it is not the primary self-dev install path.
- On Windows, the equivalents are `%LOCALAPPDATA%\\jcode\\bin\\jcode.exe` for the launcher, `%LOCALAPPDATA%\\jcode\\builds\\stable\\jcode.exe` for stable, and `%LOCALAPPDATA%\\jcode\\builds\\versions\\<version>\\jcode.exe` for immutable installs; `scripts/install.ps1` currently installs the stable channel.
- Ensure `~/.local/bin` is **before** `~/.cargo/bin` in `PATH`.

## Verifying a change at runtime

`cargo build` alone proves nothing about behavior. `jcode run` and interactive
sessions are served by the long-lived daemon at
`~/.jcode/builds/shared-server/jcode`, which is a symlink into
`~/.jcode/builds/versions/<version>/`. Until that symlink is repointed and the
daemon restarted (`jcode self-dev --build`), a freshly built binary is inert and
every runtime check silently measures the old code.

To test a change without disturbing the shared daemon or the caller's session,
run your build against its own socket:

```bash
cargo build --profile selfdev
./target/selfdev/jcode run --no-update --socket /run/user/1000/jcode-mytest.sock '<prompt>'
```

Two things that waste time otherwise:

- `crate::logging::info` writes to a log file, not stderr, so instrumenting a
  code path with it produces no visible output under `--trace`. Use `eprintln!`
  for throwaway diagnostics and delete it before committing.
- Confirm which binary you are actually inspecting. `strings` on
  `builds/shared-server/jcode` reads a 70-byte symlink, not a program; resolve it
  with `readlink -f` first.

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
