#!/bin/sh
set -eu

python3 worker.py &
worker_pid=$!
python3 server.py &
server_pid=$!

shutdown() {
  kill "$server_pid" "$worker_pid" 2>/dev/null || true
  wait "$server_pid" "$worker_pid" 2>/dev/null || true
  exit 0
}

trap shutdown TERM INT
wait "$server_pid"
shutdown
