# Git Rules

## Branch naming

`main` is the only long-lived branch. Never commit directly to `main` — cut a new branch from it for every change.

Branch names follow `<type>/<short-description>`:

- `<type>` is one of `feature`, `fix`, `docs`, `refactor`, `chore`, `test`
- `<short-description>` is lowercase English words separated by hyphens — no Chinese, underscores, or camelCase
- When the branch addresses an issue, append the issue number at the end, separated by a hyphen (e.g. `fix/subscribe-500-error-42`)

Examples: `feature/bark-key-mask`, `fix/subscription-duplicate-push`, `docs/branch-naming`, `refactor/event-runtime-split`, `chore/upgrade-tokio`
