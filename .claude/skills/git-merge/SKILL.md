---
name: git-merge
description: Merge one or more PRs in the frantufro/skulk repository. Handles the rebase-only policy, conflict resolution, and force-push. Pass PR numbers or branch names as arguments.
allowed-tools: Bash
argument-hint: <PR number(s) or branch names>
---

# Git Merge — skulk repository

This repository enforces **rebase-only merges**. Squash merges and merge commits are both disabled. Every PR must be merged with `gh pr merge --rebase`.

## Merging a PR (no conflicts)

```bash
gh pr merge <PR> --repo frantufro/skulk --rebase
```

## Merging a PR with conflicts

When `gh pr merge --rebase` fails with "Pull Request has merge conflicts", rebase the branch on `main` first:

```bash
# 1. Rebase the agent's branch onto current main
skulk send <agent-name> "Please rebase your branch on main and push: run 'git fetch origin && git rebase origin/main', fix any conflicts, run cargo test to confirm everything passes, then push with 'git push --force-with-lease'."

# 2. Once the agent has pushed, retry the merge
gh pr merge <PR> --repo frantufro/skulk --rebase
```

## Merging multiple PRs

Merge in order — later PRs may need rebasing after earlier ones land:

```bash
for pr in <pr1> <pr2> <pr3>; do
  result=$(gh pr merge $pr --repo frantufro/skulk --rebase 2>&1)
  if echo "$result" | grep -q "merge conflicts"; then
    echo "PR #$pr has conflicts — rebase needed"
  else
    echo "PR #$pr merged"
  fi
done
```

Then rebase any conflicting branches and retry.

## Why rebase-only?

The skulk repo uses a linear history policy. Rebase merge preserves individual commits and their messages (`feat:`, `fix:`, `test:` prefixes) without introducing a merge bubble. This keeps `git log --oneline` readable and makes `git bisect` reliable.

## Key rules

- Always `--rebase`, never `--squash` or `--merge`
- After rebasing a branch, `git push --force-with-lease` is required (rebase rewrites commit SHAs)
- Run `cargo test` after resolving conflicts before pushing
- Merge PRs in dependency order when multiple land at once
