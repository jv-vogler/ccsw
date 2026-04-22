# ccsw — Claude Code Switcher

Run multiple Claude Code accounts **concurrently** from different terminals, with plugins, settings, hooks, skills, and `CLAUDE.md` **shared across accounts**. No `CLAUDE_CONFIG_DIR` juggling, no `~/.claude/` copies.

```
Terminal A                 Terminal B
┌─────────────┐            ┌─────────────┐
│ $ claude    │            │ $ ccsw work │
│ (personal)  │            │ (team seat) │
└─────────────┘            └─────────────┘
       │                          │
       ▼                          ▼
~/.claude (base)          ~/.claude-profiles/work
  real creds                 real creds
  + real .claude.json        + real .claude.json
  plugins/ ◄────symlink────  plugins/
  settings.json ◄──symlink── settings.json
  CLAUDE.md ◄─────symlink──  CLAUDE.md
  hooks/   ◄──────symlink──  hooks/
  ... (allowlist)
```

Each terminal launches `claude` pinned to one account via `CLAUDE_CONFIG_DIR`. Because Claude Code derives every path — credentials, lockfiles, and (on macOS) even the Keychain service name — from that one env var, pinning it at launch gives full, concurrent isolation without touching Claude Code internals.

## Install

```bash
cargo install ccsw
```

Or build from source:

```bash
git clone https://github.com/jv-vogler/claude-acc-switcher
cd claude-acc-switcher
cargo install --path .
```

Requires Rust 1.80+.

## Quickstart

```bash
# You're already logged in as your personal account in ~/.claude.
# Create a second profile for a team seat:
ccsw add work

# Launch claude with the work profile:
ccsw work
# → inside claude: /login and authenticate the work account

# Meanwhile, another terminal stays on your personal account:
claude
```

That's the whole loop. You can open as many terminals as you like — each one runs the profile it was launched with, for its whole lifetime.

## Commands

| Command | What it does |
| --- | --- |
| `ccsw` | Open the TUI picker, then launch `claude`. |
| `ccsw <name>` | Shortcut: launch `claude` pinned to that profile. |
| `ccsw run <name> -- <cmd>` | Run any command with `CLAUDE_CONFIG_DIR` set. |
| `ccsw shell <name>` | Spawn your `$SHELL` with the env exported. |
| `ccsw ls` | List profiles with email + last-used. |
| `ccsw add <name>` | Create a profile. Copies `.claude.json` forward, builds symlinks. |
| `ccsw rm <name>` | Delete a profile. Snapshots first. |
| `ccsw rename <old> <new>` | Rename a profile. Snapshots first. |
| `ccsw sync` | Re-evaluate symlinks for every profile against the current allowlist. |
| `ccsw doctor` | Read-only health check: symlinks, credentials, orphans. |
| `ccsw restore <ts>` | Restore a profile from a snapshot (unique prefix accepted). |
| `ccsw backups` | List snapshots. |
| `ccsw current` | Print the profile name of the current shell. |
| `ccsw completions <shell>` | Emit shell completions (bash/zsh/fish/powershell/elvish). |

### Shell completions

```bash
# bash
ccsw completions bash > /etc/bash_completion.d/ccsw

# zsh
ccsw completions zsh > "${fpath[1]}/_ccsw"

# fish
ccsw completions fish > ~/.config/fish/completions/ccsw.fish
```

## Layout

```
~/.claude/                      ← base profile (personal / default)
  .credentials.json
  .claude.json
  settings.json
  CLAUDE.md
  plugins/  agents/  hooks/ ... (shared sources)

~/.claude-profiles/
  .ccsw.json                    ← profile registry
  .backups/                     ← timestamped snapshots
  work/
    .credentials.json           ← real file, per-profile
    .claude.json                ← real file, per-profile
    settings.json  → symlink → ~/.claude/settings.json
    CLAUDE.md      → symlink → ~/.claude/CLAUDE.md
    plugins/       → symlink → ~/.claude/plugins/
    ...
```

### Shared (symlinks into `~/.claude`)

`settings.json`, `CLAUDE.md`, `RTK.md`, `keybindings.json`, `hooks/`, `plugins/`, `agents/`, `skills/`, `rules/`, `ide/`, `history.jsonl`, `shell-snapshots/`, `paste-cache/`, `cache/`, `downloads/`, `file-history/`, `plans/`, `tasks/`, `session-env/`, `projects/`.

Sharing `projects/` means conversation transcripts are visible across every profile, so `/resume` works after switching accounts — useful when one account hits a rate limit mid-task.

### Per-profile (never symlinked)

`.credentials.json`, `.claude.json`, `sessions/`, `backups/`, `debug/`, `telemetry/`, `usage-data/`, `mcp-needs-auth-cache.json`.

### Override an entry per-profile

If you want a profile to have its own `settings.json` (breaking the share), just drop a real file into `~/.claude-profiles/<name>/settings.json`. Heal-on-launch detects the real file and **never clobbers it** — it logs a skip and moves on. Remove the real file to rejoin the shared copy on next launch.

## Design notes

- **Heal-on-launch (~1ms)**: every `ccsw <name>` walks the allowlist and repairs broken/missing symlinks before `exec`. New entries added to the base (e.g. installing a new plugin) appear across every profile on the next launch.
- **Directories are linked as directories**, not their contents recursively. Siblings added later to `plugins/` just work across profiles.
- **Absolute paths** in symlink targets, so profiles can be moved without breaking shares.
- **Backups before every destructive op** (`add`, `rm`, `rename`, `restore`). `restore` is itself reversible — it snapshots current state first.
- **ccsw does not stay resident**: on Unix it `execvp`s into `claude`. No parent process overhead.

## Windows caveat

Windows symlinks require **Developer Mode** or an elevated (admin) shell. Everything still works once symlinks are permitted; the symlink creation will otherwise fail with a clear error message on the first `ccsw add`.

macOS and Linux don't need anything special.

## Verification

```bash
# Create a second profile.
ccsw add work
ccsw ls

# Concurrent use:
# Terminal A
claude
# Terminal B
ccsw work        # launches claude with CLAUDE_CONFIG_DIR=~/.claude-profiles/work
# inside claude: /login → authenticate the other account
# confirm `/status` shows the right account in each terminal

# Share confirmation
ls -l ~/.claude-profiles/work/plugins
ls -l ~/.claude-profiles/work/CLAUDE.md

# Heal-on-launch
rm ~/.claude-profiles/work/CLAUDE.md
ccsw work                                    # re-heals then launches
ls -l ~/.claude-profiles/work/CLAUDE.md      # symlink restored

# Backup + restore
ccsw rm work
ccsw backups                                 # shows <ts>-rm-work
ccsw restore <ts>                            # profile returns
```

## License

MIT OR Apache-2.0.
