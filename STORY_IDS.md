# Story ID legend

Internal tracker references (`DR-XXX`, `WEG-XXX`) appear in commit messages, PR titles, and some source comments. You do **not** need tracker access to contribute — they are provenance labels, not build inputs.

## Prefixes

| Prefix | Meaning | Example |
|---|---|---|
| `DR-XXX` | **Dreamd requirement** — user-facing story, bug, or NFR from the product backlog | `DR-402` = `POST /api/v1/learn` |
| `WEG-XXX` | **Work engineering group** — implementation slice, spike, or engineering task | `WEG-72` = SO_PEERCRED middleware |

## In commits and PRs

External contributors may omit story IDs. When present, reference the ID assigned by maintainers:

```
feat(api): document HTTP handlers (DR-404)
fix(wal): recovery deletes tmp on incomplete cycle (WEG-60)
```

## In source comments

Comments like `// WEG-72 / DR-407` link code to the decision record. They are safe to ignore when reading the code — behavior is defined by tests, SPEC, and ARCHITECTURE.

Stripping story IDs from source is optional cleanup; new code should prefer linking to ARCHITECTURE.md or SPEC.md sections when the decision matters to contributors.

## Related

- [CONTRIBUTING.md](../CONTRIBUTING.md) — commit message conventions
- [CHANGELOG.md](../CHANGELOG.md) — user-visible history (no story IDs required)
