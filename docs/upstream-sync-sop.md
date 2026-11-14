# Upstream sync runbook

How to bring `PrimeIntellect-ai/prime-agent` `main` into the fork's `main`, keeping the fork's own changes.
Follow it in order. The steps come from the 2026-11 syncs (#34, #69, and the 4-commit sync in this PR).

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

Also check the upstream commits themselves:

```
gh api repos/PrimeIntellect-ai/prime-agent/commits/<sha>/check-runs
```

Cancelled jobs usually mean a newer push superseded the run. A failed job needs a look before the merge.

## 3. Conflict policy

Decisions made in past syncs. Use them unless there is a reason not to.

- **Duplicate fixes**: if upstream fixed the same bug in the same way, take upstream's code. Keep the fork's regression test only if it covers a different scenario (for example a platform upstream does not run on).
- **Test files**: take upstream's version, then port the fork's fields or helpers onto it.
- **Helpers moved upstream** (for example `tests/support`): take upstream's helper. Add any fork-only field to it.
- **Removed upstream features** (for example the private-auth path): follow upstream.
- **Fork features** (for example the settings-driven model catalog filter): keep them and reconnect them to the new code.
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

- If cargo reports `missing manifest in toolchain`, reinstall the pinned toolchain: `rustup toolchain uninstall <ver>-x86_64-unknown-linux-gnu`, then `rustup toolchain install <ver> --profile minimal -c clippy`.
- `cargo check --target x86_64-unknown-linux-gnu` from Windows fails in `ring`'s build script. Do not use it; use WSL.

Run the tests the merge touches, in WSL, with a timeout. The e2e tests (`*_e2e.rs`) are slow. Leave the full workspace test run to CI.

## 5. Commit and PR

- Create the integration branch from fork `main`: `integration/sync-upstream-<n>`.
- `git merge --no-ff FETCH_HEAD` with the message `merge(upstream): sync PrimeIntellect-ai/prime-agent main (<N> commits)`. Use a merge commit, not a squash.
- Do not set git identity. Do not add `Co-authored-by` trailers.
- Push and open the PR: `gh pr create --repo jinnnyang/prime-agent --base main --body-file <file>`.
- The PR body has these sections: Scope (commit list), Conflict resolutions (one line per file), Verification (commands and results), Changelog.
- Sync PRs carry no changelog fragment of their own. Upstream's `.changes/` fragments come with the merge.
- Watch CI with `gh pr view <n> --json statusCheckRollup`. PR CI (`ci.yml`) does not run macOS jobs.
- Merge only when every check is green, with `gh pr merge <n> --merge --match-head-commit <full-sha> --body-file <file>`. `--match-head-commit` needs the full 40-character SHA.

## 6. After the merge

1. Fast-forward local `main`: `git merge --ff-only origin/main`.
2. Move the mirror to upstream `main`: `git push origin <upstream-sha>:refs/heads/upstream`. This is a fast-forward.
3. Delete the integration branch on the fork and locally, once the PR is merged.
4. Expect `continuous` and `nightly` on `main` to behave as before. They depend on macOS runners, which may be unavailable (see the open issue for the nightly beta tag, #39).

## Known traps

- `git branch -d` refuses squash-merged branches. Use `-D` after confirming the PR is merged and the branch tip matches the PR head.
- A merge that is not green upstream can still be merged, but record the upstream check state in the PR.
- Scripts that read the mirror branch must use the current name (`upstream`), not the old `uptream`.
