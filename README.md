# cs — claude-switch

Sub-second switching between Claude Code accounts on macOS, with optional shared
config (skills / commands / agents / `CLAUDE.md`) and a per-profile usage
dashboard powered by the same `/api/oauth/usage` endpoint that backs Claude
Code's `/usage` slash command.

## Why

If you run two Claude Code accounts (e.g. a personal Max plan and a work Pro
plan), the built-in flow is `claude /logout`, `/login`, wait. `cs` swaps the
macOS Keychain entry in place, replaces `~/.claude/settings.json` from a
per-profile copy, and exits in well under a second — preserving token state
and per-account configuration.

## Install

macOS only. Requires [Claude Code](https://docs.claude.com/en/docs/claude-code)
already installed and logged in, and `~/.local/bin` (or your chosen
`CS_INSTALL_DIR`) on your `PATH`.

```bash
curl -fsSL https://raw.githubusercontent.com/mchen04/claude-swtich-application/main/install.sh | sh
cs setup            # install the shell wrapper into ~/.zshrc
cs doctor           # verify install
```

The installer downloads the latest prebuilt binary for your arch
(`aarch64`/`x86_64`) into `~/.local/bin/cs`. Set `CS_INSTALL_DIR=/somewhere`
to install elsewhere.

### From source

```bash
git clone https://github.com/mchen04/claude-swtich-application
cd claude-swtich-application
cargo build --release
cp target/release/cs ~/.local/bin/
cs setup
```

## Quick start

```bash
claude /login                # log into your first account
cs save personal             # snapshot it as a named profile

claude /login                # log into your second account
cs save work

cs personal                  # switch (sub-second)
cs work
cs -                         # toggle to the previous profile

cs                           # per-profile usage dashboard
cs list                      # all saved profiles
cs status --json | jq        # active profile + token expiry
```

## Usage dashboard

Running `cs` with no arguments (or `cs usage`) renders one row per saved
profile, populated from `/api/oauth/usage` per profile. Responses are cached
on disk for 300 s — the endpoint is aggressively rate-limited, has no
`Retry-After` header, and a triggered 429 can lock you out for 30+ minutes,
so the cache TTL is conservative.

```
   PROFILE          5H LEFT   5H RESETS    7D LEFT   7D RESETS    PLAN
*  work             63%       2h14m        36%       3d04h        max
   personal         91%       4h51m        88%       6d22h        pro
   research         —         —            —         —            max     ↳ token expired — `cs refresh research`
```

- `5H LEFT` / `7D LEFT` — `100 − utilization` for the rolling 5-hour and 7-day buckets.
- `5H RESETS` / `7D RESETS` — countdown to each bucket's reset.
- `PLAN` — subscription tier from the OAuth blob.
- `*` marks the active profile; rows sort active first, then alphabetically.
- A row that can't be fetched shows `—` plus a hint
  (`token expired — cs refresh <name>`, or `rate-limited` when 429s persist
  past the on-disk cache).

`cs usage --watch` repaints every second; reset countdowns recompute every
tick from cached `resets_at` timestamps so the display feels live, while live
fetches stay on the 300 s cache boundary. `cs usage --json` emits the report
struct for scripting.

## Master profile (shared config)

Anything in `~/.claude/{skills,commands,agents,CLAUDE.md}` is duplicated
across accounts by default. Designate one profile as **master** and those
four candidates move into that profile's directory; every other profile picks
them up via symlinks back into `~/.claude/`.

```bash
cs master personal           # designate `personal` as master
cs master                    # show current designation + per-item state
cs master work               # change master (refuses if `work` already has any of the four)
cs master --unset            # clear designation; restore content to ~/.claude
```

`cs uninstall` rolls everything back to a plain `~/.claude/` — verified
byte-clean by an integration test that diffs the directory snapshot
before/after.

## Commands

```
cs                            usage dashboard
cs <profile>                  switch
cs <profile> -- claude …      switch then exec claude with passthrough args
cs -                          switch to previous profile

cs save <name>                snapshot the active Claude Code creds as a profile
cs list                       list saved profiles
cs status [<profile>]         active profile details (token expiry, plan)
cs rm <name>                  remove a saved profile
cs rename <from> <to>         rename a saved profile
cs default <name>             set the default profile
cs default-go                 switch to the default profile
cs refresh [<profile>]        force-refresh OAuth via `claude /status`

cs usage [--watch] [--json]   per-profile % of 5h block + weekly cap remaining

cs master [<name>|--unset]    manage the master profile

cs setup [--shell zsh|bash]   install/repair the shell wrapper
cs doctor                     read-only health check
cs uninstall [--keep-master]  remove cs (symlinks, wrapper)

Global flags: --json --no-color --profile <name> -v / -vv
```

## Layout

```
~/.claude/                          # canonical Claude Code home
├── settings.json                   # rewritten on switch from per-profile copy
├── .active-profile                 # tracking marker
├── skills/  → ~/.claude-cs/profiles/<master>/skills/    (after `cs master <name>`)
├── commands/ → ~/.claude-cs/profiles/<master>/commands/
├── agents/   → ~/.claude-cs/profiles/<master>/agents/
└── CLAUDE.md → ~/.claude-cs/profiles/<master>/CLAUDE.md

~/.claude-cs/
├── profiles/<name>/
│   ├── settings.json               # replaces canonical on switch
│   ├── env                         # KEY=VAL pairs sourced post-switch (optional)
│   └── skills/, commands/, …       # only on the profile designated as master
├── state.json                      # {active, previous, default, master}
└── cache/usage-limits/<name>.json  # 300 s cache of /api/oauth/usage responses
```

Keychain entries:

- `Claude Code-credentials` / `acct=$USER` — Claude Code's canonical entry (read/written by `cs` on switch).
- `Claude Code-credentials` / `acct=Claude Code-credentials-<name>` — saved profile.

## Safety

- Every destructive operation acquires `~/.claude-cs/.lock` (advisory `flock`)
  so two concurrent `cs` invocations can't clobber each other.
- Every Keychain write is read back and verified byte-equal; on mismatch the
  switch rolls back to the previous credential.
- **No surprise logouts.** Claude Code silently rotates the OAuth
  access **and refresh** token in its live Keychain entry as it runs, and a
  rotated refresh token invalidates its predecessor server-side. Before
  overwriting that entry, `cs` copies the *current* credential back into the
  profile you're leaving — so switching away and back never reinstalls a
  dead refresh token. `cs refresh` updates the active profile's live
  credential in place (it only stages non-active profiles), so a refresh can't
  leave your running session on an invalid token.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Test seam: every filesystem and Keychain access is routed through `Paths` and
the `Keychain` trait, so tests can inject a tmpdir and a JSON-backed mock
Keychain via `CS_TEST_KEYCHAIN=1` and
`CS_TEST_KEYCHAIN_FIXTURE=/path/to/fixture.json`. For per-profile usage
fixtures, `CS_TEST_LIMITS_FIXTURE=/dir` reads `<profile>.json` (matching the
`/api/oauth/usage` response shape); `CS_TEST_LIMITS_FAIL=/dir` (file
`<profile>.txt` containing `expired`, `rate_limited`, or `http`) exercises
error paths without networking.

## Acknowledgements

Usage data comes from Anthropic's `/api/oauth/usage` endpoint — the same one
that backs Claude Code's `/usage` slash command.
