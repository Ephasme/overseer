#!/usr/bin/env bash
# Sets up the tools Overseer drives. Safe to re-run: every step checks before acting.
set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/overseer"
OMP_DIR="${PI_CODING_AGENT_DIR:-$HOME/.omp/agent}"
OMP_MCP="$OMP_DIR/mcp.json"
LOCAL_AGENT_MAIL_URL="http://127.0.0.1:8765/mcp/"
export PATH="$HOME/.local/bin:$PATH"

log()  { printf '  \033[32m>\033[0m %s\n' "$1"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }
die()  { printf '  \033[31mx\033[0m %s\n' "$1" >&2; exit 1; }
has()  { command -v "$1" >/dev/null 2>&1; }
ask()  { local reply; read -r -p "  ? $1 " reply </dev/tty; printf '%s' "$reply"; }
confirm() { [[ "$(ask "$1 [y/N]")" =~ ^[Yy]$ ]]; }

check_prerequisites() {
  local missing=()
  for bin in git curl jq gh; do has "$bin" || missing+=("$bin"); done
  ((${#missing[@]} == 0)) || die "missing: ${missing[*]}. Install them, then re-run."
  gh auth status >/dev/null 2>&1 || die "gh is not authenticated: run 'gh auth login'."
}

install_herdr() {
  if has herdr; then
    log "herdr $(herdr --version | awk '{print $2}') present"
  else
    log "installing herdr"
    curl -fsSL https://herdr.dev/install.sh | sh
  fi
}

install_omp() {
  if has omp; then
    log "$(omp --version) present"
  else
    log "installing Oh My Pi"
    curl -fsSL https://omp.sh/install | sh
  fi
  # Lets herdr report each omp pane as working / blocked / done.
  if herdr integration status 2>/dev/null | grep -q '^omp: current'; then
    log "herdr omp integration current"
  else
    # herdr refuses to install into a missing directory, which a never-launched omp leaves.
    mkdir -p "$OMP_DIR/extensions"
    herdr integration install omp
  fi
}

# Strips the MCP path so the health route can be reached on the same origin.
agent_mail_base() { printf '%s' "${1%/}" | sed -E 's#/mcp$##'; }

wait_healthy() {
  local base; base="$(agent_mail_base "$1")"
  for _ in $(seq 1 30); do
    curl -fsS -o /dev/null --max-time 3 "$base/health/liveness" && return 0
    sleep 1
  done
  return 1
}

install_local_agent_mail() {
  if ! has minisign; then
    # The agent-mail installer refuses to run without minisign to verify the release.
    if has brew; then brew install minisign
    else die "minisign is required by the agent-mail installer (apt install minisign xz-utils)."
    fi
  fi
  if has am; then
    log "agent-mail present"
  else
    log "installing agent-mail (local server on 127.0.0.1:8765)"
    curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh" | bash -s -- --yes
  fi
  wait_healthy "$LOCAL_AGENT_MAIL_URL" || die "local agent-mail did not come up: check 'am service status'."
}

# The agent-mail installer may have written a bearer token; clients must send it.
local_agent_mail_token() {
  local env_file="${XDG_CONFIG_HOME:-$HOME/.config}/mcp-agent-mail/config.env"
  [[ -f "$env_file" ]] || return 0
  sed -nE 's/^(export )?HTTP_BEARER_TOKEN=["'\'']?([^"'\'']*)["'\'']?$/\2/p' "$env_file" | tail -1
}

configure_agent_mail() {
  local url token="" token_file="$CONFIG_DIR/agent-mail.token"
  url="$(ask "Agent-mail MCP URL (empty = install a local server):")"
  if [[ -z "$url" ]]; then
    install_local_agent_mail
    url="$LOCAL_AGENT_MAIL_URL"
    token="$(local_agent_mail_token)"
  else
    [[ "$url" == */ ]] || url="$url/"
    wait_healthy "$url" || die "$url is not answering on /health/liveness."
    read -r -s -p "  ? Bearer token (empty = none): " token </dev/tty; echo
  fi
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST ${token:+-H "Authorization: Bearer $token"} "$url")"
  [[ "$code" != 401 && "$code" != 403 ]] || die "$url refused the credentials (HTTP $code)."
  [[ -z "$token" ]] || (umask 077; printf '%s' "$token" >"$token_file")
  printf 'OVERSEER_AGENT_MAIL_URL=%s\n' "$url" >"$CONFIG_DIR/agent-mail.env"
  register_mcp "$url" "$( [[ -n "$token" ]] && printf '%s' "$token_file" )"
}

# Workers run omp inside other repositories, so the server goes in the user-level omp config.
register_mcp() {
  local url="$1" token_file="$2" existing
  mkdir -p "$(dirname "$OMP_MCP")"
  [[ -f "$OMP_MCP" ]] || echo '{"mcpServers":{}}' >"$OMP_MCP"
  existing="$(jq -r --arg u "$url" '.mcpServers // {} | to_entries[] | select((.value.url // "") | rtrimstr("/") == ($u | rtrimstr("/"))) | .key' "$OMP_MCP" | head -1)"
  if [[ -n "$existing" ]]; then
    log "omp already reaches $url as '$existing'"
    return
  fi
  confirm "Add agent-mail ($url) to $OMP_MCP?" || { warn "skipped: omp agents will not reach agent-mail"; return; }
  cp "$OMP_MCP" "$OMP_MCP.bak"
  # The token stays in its own 0600 file; omp runs the '!' command at connect time.
  jq --arg u "$url" --arg f "$token_file" '
    .mcpServers["agent-mail"] = {type: "http", url: $u}
    + (if $f == "" then {} else {headers: {Authorization: ("!printf \"Bearer %s\" \"$(cat " + $f + ")\"")}} end)
  ' "$OMP_MCP.bak" >"$OMP_MCP"
  log "registered agent-mail in $OMP_MCP (backup: $OMP_MCP.bak)"
}

configure_roots() {
  local roots_file="$CONFIG_DIR/roots" input dir
  if [[ -s "$roots_file" ]]; then
    log "repository roots: $(paste -sd ' ' "$roots_file")"
    confirm "Replace them?" || return 0
  fi
  input="$(ask "Directories holding your repositories (space-separated):")"
  : >"$roots_file.tmp"
  for dir in $input; do
    dir="${dir/#\~/$HOME}"
    [[ -d "$dir" ]] || die "not a directory: $dir"
    (cd "$dir" && pwd) >>"$roots_file.tmp"
  done
  [[ -s "$roots_file.tmp" ]] || die "at least one directory is required."
  mv "$roots_file.tmp" "$roots_file"
  log "Overseer will only work on repositories under: $(paste -sd ' ' "$roots_file")"
}

main() {
  mkdir -p "$CONFIG_DIR"
  check_prerequisites
  install_herdr
  install_omp
  configure_agent_mail
  configure_roots
  log "done. Config in $CONFIG_DIR"
}

main "$@"
