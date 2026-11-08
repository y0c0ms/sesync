export HOME=/tmp/desktop XDG_CONFIG_HOME=/tmp/desktop/.config XDG_DATA_HOME=/tmp/desktop/.local/share XDG_STATE_HOME=/tmp/desktop/.local/state XDG_CACHE_HOME=/tmp/desktop/.cache
export PATH=/var/tmp/sesync-demo/bin:/usr/bin:/bin TMPDIR=/var/tmp/sesync-demo/tmp SHELL=/bin/bash
export TERM=xterm-256color LANG=C.UTF-8 HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost OLLAMA_HOST=http://127.0.0.1:9
export PS1='\[\e[1;32m\]alice@desktop\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]$ '
unset OMP_AGENT_DIR PI_AGENT_DIR CLAUDE_CONFIG_DIR SESYNC_CONFIG_DIR HERDR_SOCKET_PATH HERDR_SESSION HERDR_ACTIVE_WORKSPACE_ID
for v in $(compgen -e); do case $v in HERDR*|*TOKEN*|*_KEY|*SECRET*|SSH_AUTH_SOCK|DBUS_SESSION_BUS_ADDRESS|OMP_*|ANTHROPIC*|OPENAI*) unset $v;; esac; done
