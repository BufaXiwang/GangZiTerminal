#!/usr/bin/env bash
set -euo pipefail

SESSION_NAME="${SESSION_NAME:-gangzi-terminal}"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMMAND="${COMMAND:-npm run tauri -- dev}"

usage() {
  printf 'Usage: %s {start|stop|restart|attach|logs|status}\n' "$0"
}

has_session() {
  tmux has-session -t "$SESSION_NAME" 2>/dev/null
}

case "${1:-}" in
  start)
    if has_session; then
      printf 'tmux session already running: %s\n' "$SESSION_NAME"
    else
      tmux new-session -d -s "$SESSION_NAME" -c "$PROJECT_ROOT" "$COMMAND"
      printf 'started tmux session: %s\n' "$SESSION_NAME"
    fi
    ;;
  stop)
    if has_session; then
      tmux kill-session -t "$SESSION_NAME"
      printf 'stopped tmux session: %s\n' "$SESSION_NAME"
    else
      printf 'tmux session not running: %s\n' "$SESSION_NAME"
    fi
    ;;
  restart)
    if has_session; then
      tmux kill-session -t "$SESSION_NAME"
    fi
    tmux new-session -d -s "$SESSION_NAME" -c "$PROJECT_ROOT" "$COMMAND"
    printf 'restarted tmux session: %s\n' "$SESSION_NAME"
    ;;
  attach)
    exec tmux attach-session -t "$SESSION_NAME"
    ;;
  logs)
    if has_session; then
      tmux capture-pane -t "$SESSION_NAME" -p -S -200
    else
      printf 'tmux session not running: %s\n' "$SESSION_NAME"
      exit 1
    fi
    ;;
  status)
    if has_session; then
      tmux list-sessions | grep "^${SESSION_NAME}:"
    else
      printf 'tmux session not running: %s\n' "$SESSION_NAME"
      exit 1
    fi
    ;;
  *)
    usage
    exit 2
    ;;
esac
