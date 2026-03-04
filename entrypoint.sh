#!/bin/bash
set -e

PUID=${PUID:-1000}
PGID=${PGID:-1000}

groupadd -f -g "$PGID" appgroup 2>/dev/null || true
useradd -u "$PUID" -g "$PGID" -o -s /bin/bash appuser 2>/dev/null || true

chown -R "$PUID":"$PGID" /data

exec gosu "$PUID":"$PGID" "$@"
