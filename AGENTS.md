# Repository Guidelines

## Repository Scope

- Jcode Desktop is in a separate repository.

## Development Workflow

- **Use the user's Git identity** - Create commits with the configured
  `user.name` and `user.email`. Do not override them with `Jcode`, `Jcode agent`,
  or a fabricated agent email. Preserve existing contributor attribution when
  integrating work. If no identity is configured, ask rather than inventing one.
- **Welcome pull requests from everyone** - Review contributions on their merits,
  regardless of whether the author is a maintainer, an existing contributor, a
  first-time contributor, or an agent. Good PRs can be merged directly after review
  and validation. Do not require a maintainer-authored rewrite merely because of
  who submitted the change. See `CONTRIBUTING.md` for the contribution policy.
- **Keep work scoped** - Work on your own branch and preserve unrelated work. When
  the user asks you to review or integrate a PR or branch, you may inspect, test,
  and integrate that contribution regardless of author status. Do not pull in
  unrelated branches or merge a PR without user authorization.

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

`blaude-agent` is indexed as a knowledge graph of ~54k symbols and ~140k relationships across ~300 execution flows. Query it with the commands
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
| Find dead / unused code | `blaude prune` |

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
- **When asked to remove dead/unused code**, run `blaude prune`: it lists
  functions the call graph shows no callers for (candidates — confirm with
  `cargo check` before deleting).

## Refresh

blaude re-indexes in the background whenever an agent writes code, so this graph
stays current on its own. `blaude brief` re-briefs or repairs by hand if
`status` ever reports stale.
<!-- gitnexus:end -->
