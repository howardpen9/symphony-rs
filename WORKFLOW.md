---
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: demo-project
polling:
  interval_ms: 30000
workspace:
  root: ./workspaces
agent:
  max_concurrent_agents: 4
  max_turns: 20
codex:
  command: codex app-server
---

# Rust Symphony Rewrite

Work on issue {{ issue.identifier }}: {{ issue.title }}

Current state: {{ issue.state }}
Attempt: {{ attempt }}

Review the repository, implement the requested change, run relevant checks, and leave clear notes for the next handoff.
