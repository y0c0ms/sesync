#!/bin/bash
# Stop the desktop's herdr server and forget its workspaces (settings stay).
for pid in $(pgrep -f 'herdr'); do grep -qz 'HOME=/tmp/desktop' /proc/$pid/environ 2>/dev/null && kill $pid; done
sleep 1
rm -rf /tmp/desktop/.config/herdr/session.json /tmp/desktop/.config/herdr/session-snapshots /tmp/desktop/.config/herdr/*.sock
