# Show HN draft - dreamd v0.1

Paste-ready for the marketing spike (~2026-08-11). Allen drafts; Austin posts (RED).

Link the GitHub repo: https://github.com/botzrDev/dreamd  
Do not link a landing page. Do not put "MCP" in the title.

---

## Title (candidate)

```
Show HN: dreamd - make Claude Code, Cursor, and Cline remember the same things
```

Char count: 78 (under 80). Alternate if needed:

```
Show HN: dreamd - shared memory files for Claude Code, Cursor, and Cline
```

---

## Maker comment (paste after the link post)

I got tired of teaching the same project facts to every coding agent.

Claude Code remembers one way. Cursor remembers another. Cline has its own store. I bounce between them in a week, and each one starts cold on things the others already learned.

dreamd is a local-first answer to that. It puts a `.agent/` folder in your repo: plain JSONL and Markdown. Agents read and write those files through a small MCP server (`search_nodes` / `append_node`). The filesystem is the source of truth. You can cat, grep, git diff, and hand-edit everything. What one harness learns, the next already has.

The architectural bet is the storage model, not "we invented cross-harness memory." Other projects exist. We own the files-you-already-version-control wedge.

Honest tradeoff at v0.1: recall is lexical BM25 plus a simple salience formula, not embeddings. That is deliberate scope so the store stays plain text and portable across models. Semantic recall is later, not claimed as shipped.

Open core, stated plainly: Apache-2.0 core, self-hosted only. Premium may come later. Do not assume free-forever for everything.

I want feedback on: does the "files are the memory" story land for people who already live in git, or does it still read like another agent accessory?

Repo: https://github.com/botzrDev/dreamd  
Install: `npx -y dreamd-mcp init` then point your harness at `npx -y dreamd-mcp`.

---

## Checklist before posting

- [ ] Title has no "MCP"
- [ ] Title under 80 chars
- [ ] Link is GitHub, not dreamd.dev
- [ ] Open-core disclosed in the maker comment (not discovered later)
- [ ] One honest tradeoff named (BM25 at v0.1)
- [ ] One concrete ask for feedback
- [ ] Austin posts personally (RED)
