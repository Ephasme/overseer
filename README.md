# overseer

A single supervising agent that runs other coding agents. Workers are Oh My Pi (`omp`) sessions in herdr workspaces. They coordinate through an MCP Agent Mail server.

Overseer never clones a repository into its own directory. It works only on repositories that already exist under the directories you list at install time, and it opens each task in a herdr worktree of that repository.

## Install

```sh
./install.sh
```

The script is safe to re-run. It:

1. Checks for `git`, `curl`, `jq`, and an authenticated `gh`.
2. Installs herdr and `omp` if they are missing, then the herdr `omp` integration that reports each pane's state.
3. Asks for an Agent Mail server URL and bearer token. If you leave the URL empty, it installs a local server on `127.0.0.1:8765`.
4. Registers that server in the user-level `omp` config (`~/.omp/agent/mcp.json`), after asking and after backing the file up.
5. Asks which directories hold your repositories.

Settings are written to `~/.config/overseer/`:

| File | Content |
|------|---------|
| `roots` | Absolute repository directories, one per line |
| `agent-mail.env` | `OVERSEER_AGENT_MAIL_URL` |
| `agent-mail.token` | Bearer token, mode `0600`, read by `omp` when it connects |
