#!/bin/bash
# Builds the demo the site's recordings show, then `vhs tapes/<name>.tape` records each GIF into $D/out.
# Needs: vhs, tmux, herdr and omp on PATH, podman with the test Postgres container `tsv-ses-pg`
# (AGENTS.md, live team tests), and these built binaries:
#   SESYNC_BIN_DIR   a folder holding sesync, sesync-conv and sesync-lock
#   FAKE_MODEL_BIN   the fake-model binary (the one tests/bridge.rs uses)
#   TEAM_SERVER_BIN  sesync-team's team-server
# Nothing outside $D, /tmp/desktop and /tmp/laptop is written; no real session store is read.
#
# Two fictional machines, on loopback only:
#   desktop: HOME=/tmp/desktop, seven omp sessions, watch daemon, herdr (started by the tapes).
#   laptop:  HOME=/tmp/laptop, one omp session made against a fake model, its own herdr server and bridge (:6791).
#   team:    team-server :30599 on the throwaway test Postgres (database sesync_demo), team "demo", both approved.
set -e
D=/var/tmp/sesync-demo; A=/tmp/desktop; B=/tmp/laptop
FAKE=${FAKE_MODEL_BIN:?set FAKE_MODEL_BIN}
TS=${TEAM_SERVER_BIN:?set TEAM_SERVER_BIN}
BIN=${SESYNC_BIN_DIR:?set SESYNC_BIN_DIR}
HERE=$(cd "$(dirname "$0")" && pwd)
U=http://127.0.0.1:30599

# Stop what an earlier run left (by PID, found through the ports and the demo HOME paths).
for port in 6791 18080 30599; do
  for pid in $(ss -ltnpH "sport = :$port" | grep -oE 'pid=[0-9]+' | cut -d= -f2); do kill $pid; done
done
for pid in $(pgrep -f 'sesync --watch|herdr server|sesync --tui'); do
  grep -qzE 'HOME=/tmp/(desktop|laptop)' /proc/$pid/environ 2>/dev/null && kill $pid
done
tmux -L sdemo kill-server 2>/dev/null || true
sleep 1
rm -rf $A $B $D/tmp $D/bin; mkdir -p $D/tmp $D/bin $D/team $D/out
for b in sesync sesync-conv sesync-lock; do cp "$BIN/$b" $D/bin/; done
ln -s "$(command -v omp)" "$(command -v herdr)" $D/bin/
cp "$HERE/env.sh" "$HERE/reset-herdr.sh" $D/; cp "$HERE/totp.py" $D/team/
[ -f $D/team/master.key ] || head -c 32 /dev/urandom > $D/team/master.key
[ -f $D/team/admin.token ] || head -c 48 /dev/urandom | base64 | tr -d '\n/+=' > $D/team/admin.token
chmod 700 $D/team; chmod 600 $D/team/master.key $D/team/admin.token

envA() { env -i bash --noprofile --norc -c ". $D/env.sh; $*"; }
envB() { env -i HOME=$B XDG_CONFIG_HOME=$B/.config XDG_DATA_HOME=$B/.local/share XDG_STATE_HOME=$B/.local/state \
  XDG_CACHE_HOME=$B/.cache PATH=$D/bin:/usr/bin:/bin SHELL=/bin/bash TERM=xterm-256color LANG=C.UTF-8 TMPDIR=$D/tmp \
  HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost OLLAMA_HOST=http://127.0.0.1:9 "$@"; }

# Fake model shared by both machines.
mkdir -p $B/logs $B/projects/api-gateway $B/.config/herdr
cat > $D/scenario.yaml <<'S'
rules:
  - name: title
    match: 'You name coding-agent sessions'
    reply: {text: "Health check endpoint"}
  - name: health
    match: 'health check endpoint'
    reply: {text: "Added GET /healthz in internal/http/health.go. It pings Redis and returns 200 with {\"status\":\"ok\"}, or 503 when Redis is down. Two tests cover both cases."}
  - name: tests
    match: 'tests'
    reply: {text: "All 58 tests pass in 3.2 s, including the two new /healthz cases."}
  - name: other
    reply: {text: "Done."}
S
setsid $FAKE --listen 127.0.0.1:18080 --scenario $D/scenario.yaml --log $D/fake.log > $D/fake.out 2>&1 < /dev/null &
models='providers:\n  fake:\n    baseUrl: http://127.0.0.1:18080/v1\n    api: openai-completions\n    apiKey: fake-key-not-a-secret\n    authHeader: true\n    discovery:\n      type: openai-models-list\n      injectV1: false\n    modelOverrides:\n      fake-32k:\n        input: [text, image]\n'

# Desktop.
python3 "$HERE/make-home.py" $A > /dev/null
mkdir -p $A/.omp/agent $A/.config/herdr; printf "$models" > $A/.omp/agent/models.yml
cat > $A/.config/herdr/config.toml <<'T'
onboarding = false
[[keys.command]]
key = "ctrl+alt+s"
type = "popup"
command = "sesync --tui"
description = "sesync: open a session"
width = "90%"
height = "85%"
T

envA "herdr integration install omp" > /dev/null 2>&1 || true

# Laptop: one real omp turn against the fake model, then its herdr and bridge.
mkdir -p $B/.omp/agent; printf "$models" > $B/.omp/agent/models.yml
sleep 1
(cd $B/projects/api-gateway && envB TERM=dumb omp -p --model fake-32k --no-lsp "Add a health check endpoint for the load balancer" > $B/logs/omp-first.out 2>&1)
setsid env -i HOME=$B XDG_CONFIG_HOME=$B/.config PATH=$D/bin:/usr/bin:/bin SHELL=/bin/bash TERM=xterm-256color LANG=C.UTF-8 \
  TMPDIR=$D/tmp HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost herdr server > $B/logs/herdr.log 2>&1 < /dev/null &
for i in $(seq 75); do [ -S $B/.config/herdr/herdr.sock ] && break; sleep 0.2; done
envB herdr integration install omp > $B/logs/integ.out 2>&1 || true
setsid env -i HOME=$B XDG_CONFIG_HOME=$B/.config PATH=$D/bin:/usr/bin:/bin TMPDIR=$D/tmp LANG=C.UTF-8 \
  HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost \
  sesync bridge serve --listen 127.0.0.1:6791 --herdr-socket $B/.config/herdr/herdr.sock --no-ntfy --no-usage > $B/logs/bridge.log 2>&1 < /dev/null &
sleep 2
url=$(envB sesync bridge pair --endpoint ws://127.0.0.1:6791 --label desktop 2>&1 | grep -oE 'agenthub://pair\?code=[A-Za-z0-9=_-]+')
envA "sesync machine add laptop '$url'" > /dev/null

# Team.
podman start tsv-ses-pg > /dev/null
podman exec tsv-ses-pg psql -U postgres -qc "DROP DATABASE IF EXISTS sesync_demo" -c "CREATE DATABASE sesync_demo" 2>/dev/null
setsid env -i TEAM_SERVER_BIND=127.0.0.1:30599 TEAM_SERVER_DATABASE=postgres://postgres:test@127.0.0.1:55437/sesync_demo \
  TEAM_MASTER_KEY_FILE=$D/team/master.key TEAM_ADMIN_TOKEN_FILE=$D/team/admin.token $TS > $D/team/server.log 2>&1 < /dev/null &
sleep 2
envA "SESYNC_TEAM_URL=$U SESYNC_ADMIN_TOKEN_FILE=$D/team/admin.token sesync team create demo --member alice" 2>&1 \
  | grep -oE 'secret=[A-Z2-7]+' | cut -d= -f2 > $D/team/totp.secret
python3 $D/team/totp.py | envA "SESYNC_TEAM_URL=$U SESYNC_MACHINE=desktop sesync team join demo --member alice" > /dev/null 2>&1
# One code per join: wait for the next 30 s window.
sleep $(( 31 - $(date +%s) % 30 ))
python3 $D/team/totp.py | envB SESYNC_TEAM_URL=$U SESYNC_MACHINE=laptop sesync team join demo --member alice > /dev/null 2>&1
envA "export SESYNC_TEAM_URL=$U SESYNC_ADMIN_TOKEN_FILE=$D/team/admin.token; sesync team approve demo alice desktop; sesync team approve demo alice laptop" > /dev/null

# Shares: desktop marks two sessions, laptop its one (the TUI's own `m`).
tmux -L sdemo -f /dev/null new-session -d -x 150 -y 36 -n lap "env -i HOME=$B XDG_CONFIG_HOME=$B/.config PATH=$D/bin:/usr/bin:/bin TERM=xterm-256color LANG=C.UTF-8 TMPDIR=$D/tmp sesync --tui"
tmux -L sdemo new-window -d -n desk "env -i bash --noprofile --norc -c '. $D/env.sh; sesync --tui'"
sleep 3
tmux -L sdemo send-keys -t lap m; tmux -L sdemo send-keys -t desk Down Down m Down Down Down m
sleep 1
tmux -L sdemo send-keys -t lap q; tmux -L sdemo send-keys -t desk q
sleep 1; tmux -L sdemo kill-server 2>/dev/null || true
rm -f $A/.config/sesync/tui.json

# Watch daemons on both machines.
for h in $A $B; do
  setsid env -i HOME=$h XDG_CONFIG_HOME=$h/.config PATH=$D/bin:/usr/bin:/bin TMPDIR=$D/tmp LANG=C.UTF-8 SESYNC_QUIET=2 SESYNC_TEAM_QUIET=5 \
    sesync --watch > $h/daemon.log 2>&1 < /dev/null &
done
sleep 30
tail -n 2 $A/daemon.log $B/daemon.log
envA "sesync machine list"
