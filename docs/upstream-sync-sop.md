# Upstream sync runbook

How to bring `PrimeIntellect-ai/prime-agent` `main` into the fork's `main`, keeping the fork's own changes.
Follow it in order. The steps come from the 2026-11 syncs (#34, #69, #94).

## Terms

- **Upstream**: `PrimeIntellect-ai/prime-agent`, branch `main`.
- **Fork**: `jinnnyang/prime-agent`, branch `main`. The fork's own changes live here.
- **Mirror**: fork branch `upstream`. It must equal upstream `main` after each sync (fast-forward only).
- **Base**: the upstream commit that the last sync merged. Find it as the second parent of the last `merge(upstream): ...` commit on fork `main`.

## 1. Fetch and measure

```
git fetch https://github.com/PrimeIntellect-ai/prime-agent.git main   # FETCH_HEAD = upstream main
git log --format='%h %an %s' <base>..FETCH_HEAD                         # what is new
git diff --name-only <base> FETCH_HEAD                                   # upstream-changed files
git diff --name-only <base> HEAD                                         # fork-changed files
```

The two file lists overlap in a few places. Those are the only files that can conflict.

## 2. Dry-run the merge

```
git merge-tree --write-tree --name-only --messages HEAD FETCH_HEAD
```

No output lines starting with `CONFLICT` means a clean text merge. Text-clean is not enough: read the upstream diff of each overlapping file and check the fork's behavior still holds.

Check upstream's CI on `main` (commit-scoped check-run listings proved unreliable):

```
gh run list --repo PrimeIntellect-ai/prime-agent --workflow ci.yml --branch main --limit 5
```

Cancelled jobs usually mean a newer push superseded the run. A failed job needs a look before the merge.

## 3. Conflict policy

Decisions made in past syncs. Use them unless there is a reason not to.

- **Duplicate fixes**: if upstream fixed the same bug in the same way, take upstream's code. Keep the fork's regression test only if it covers a different scenario (for example a platform upstream does not run on).
- **Test files**: take upstream's version, then port the fork's fields or helpers onto it.
- **Helpers moved upstream** (for example `tests/support`): take upstream's helper. Add any fork-only field to it.
- **Removed upstream features** (for example the private-auth path): follow upstream.
- **Fork features** (for example the settings-driven model catalog filter): keep them and reconnect them to the new code.
- **Security-hardened files** (for example `python_env.rs`): never take upstream's file wholesale. Merge upstream's additions into the fork's version, keep the fork's trust boundaries (the `!path_starts_with` HOME rule), and rerun that module's fork tests.
- **Runner labels or CI comments**: take upstream's unless the fork has a reason.

Never add a `let _ = ...` around a persist, write, or flush call that the fork had made fallible. Keep the fork's logging.

## 4. Verify

Windows (fast):

```
cargo fmt --all --check
cargo test -p <crate> --lib <filter>      # crates the merge touched
```

Linux (required before the PR, because Windows skips `#![cfg(unix)]` tests):

Run inside WSL Ubuntu-26.04. Keep the target dir and logs in the WSL home:

```
wsl -d Ubuntu-26.04 bash -lc 'cd /mnt/c/Users/jinnn/Documents/prime-agent && CARGO_TARGET_DIR=$HOME/pa-target cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings'
```

Anything longer than a one-liner goes through a script file: inline `$PATH`, parentheses, and nested quotes break `wsl bash -lc`.

1. Write the script to `C:/Users/jinnn/AppData/Local/Temp/<name>.sh` with `newline="\n"`.
2. Start with `set -u` and `export PATH="$HOME/.cargo/bin:$PATH"`; end every step with an explicit marker (`echo CLIP_EXIT=$?`) — a wrapper's exit code says nothing about the inner command.
3. Run with `MSYS_NO_PATHCONV=1 wsl -d Ubuntu-26.04 bash -lc 'bash /mnt/c/Users/jinnn/AppData/Local/Temp/<name>.sh' | tr -d '\000'`.

- If cargo reports `missing manifest in toolchain`, reinstall the pinned toolchain: `rustup toolchain uninstall <ver>-x86_64-unknown-linux-gnu`, then `rustup toolchain install <ver> --profile minimal -c clippy`.
- `cargo check --target x86_64-unknown-linux-gnu` from Windows fails in `ring`'s build script. Do not use it; use WSL.
- Windows cross-clippy (`--target x86_64-pc-windows-gnu` from WSL) needs MinGW (`gcc-mingw-w64-x86-64`, installed once by hand). Run it per-crate (`-p <crate> --lib --tests`), not workspace-wide.
- A `-D warnings` failure is not automatically merge-introduced: `vendor/crossterm` carries pre-existing warnings that exist on fork `main` too. When clippy fails, run the same command on a worktree of fork `main` and compare; only merge-introduced findings block.
- A bug-fix regression test counts only after a reverse check: remove the fix, watch the test fail, restore, watch it pass.
- A local e2e failure that also reproduces on a clean upstream checkout in the same environment is not a merge blocker: record it in the PR, file an issue, let PR CI decide (example: #95).

Run the tests the merge touches, in WSL, with a timeout. The e2e tests (`*_e2e.rs`) are slow. Leave the full workspace test run to CI.

## 5. Commit and PR

- Create the integration branch from fork `main`: `integration/sync-upstream-<n>`.
- `git merge --no-ff FETCH_HEAD` with the message `merge(upstream): sync PrimeIntellect-ai/prime-agent main (<N> commits)`. Use a merge commit, not a squash.
- Do not set git identity. Do not add `Co-authored-by` trailers.
- Push and open the PR: `gh pr create --repo jinnnyang/prime-agent --base main --body-file <file>`.
- The PR body has these sections: Scope (commit list), Conflict resolutions (one line per file), Verification (commands and results), Changelog.
- Changelog: the `Check changelog fragment` job fails when a PR's own diff touches shipped paths (`crates/`, `vendor/`, `install-rust.sh`, `prime-agent-runtime/`, `skills/`). A pure merge sync relies on upstream's `.changes/` fragments riding along; a PR that adds fork fixes on top adds its own `.changes/<branch-slug>.md` (one user-facing bullet) before the PR is opened.
- Watch CI with `gh pr view <n> --json statusCheckRollup`. PR CI (`ci.yml`) does not run macOS jobs.
- CI runs only with an open PR targeting `main`. Pushes to a branch whose PR was already merged trigger nothing: open a new PR from the same branch — the diff against `main` is exactly the new commits.
- Do not burn Actions triggers: the fork's Actions were disabled once for excessive runs. The only acceptable empty commit is a single retrigger after Actions were re-enabled.
- Watch long runs with a heartbeat, not by hand (issue #96): create it at launch (five-segment cron or `@hourly` only), adjust the interval by completion ratio, delete it when the run ends, list failed jobs with log summaries before reporting.
- Merge only when every check is green, with `gh pr merge <n> --merge --match-head-commit <full-sha> --body-file <file>`. `--match-head-commit` needs the full 40-character SHA.

## 6. After the merge

1. Fast-forward local `main`: `git merge --ff-only origin/main`.
2. Move the mirror to upstream `main`: `git push origin <upstream-sha>:refs/heads/upstream`. This is a fast-forward.
3. Delete the integration branch on the fork and locally once the PR is merged, and remove the temp worktrees, scripts, and review files the run created.
4. Expect `continuous` and `nightly` on `main` to behave as before. macOS runner capacity can cancel runs; the #39 pattern did not recur after the 2026-11 sync and #39 is closed — reopen it if cancellations return.

## Known traps

- `git branch -d` refuses squash-merged branches. Use `-D` after confirming the PR is merged and the branch tip matches the PR head.
- A merge that is not green upstream can still be merged, but record the upstream check state in the PR.
- Scripts that read the mirror branch must use the current name (`upstream`), not the old `uptream`.
- The `deny` gate reads the RustSec advisory DB live: yesterday's green run is not today's. A new advisory is fixed by a lock-only bump (`cargo update -p <crate> --precise <fixed>`), verified locally with `cargo-deny check advisories licenses`, and rides in the same PR with a dependency note (example: RUSTSEC-2026-0592 in `url` <= 2.5.8, hit right after the 2026-11 sync).
- Until #98 lands `.gitattributes`: the repo's blobs are LF, the system `core.autocrlf=true` masks whole-file CRLF drift in `git diff`, and Python text reads/writes must use `newline=""`. Verify with `git ls-files --eol <file>` (`i/lf w/lf`).
