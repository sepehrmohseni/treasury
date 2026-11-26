#!/bin/sh
# AUTHORIZED_KEY, when set, is the public key allowed to log in as `treasury`.
set -eu
if [ -n "${AUTHORIZED_KEY:-}" ]; then
  printf '%s\n' "$AUTHORIZED_KEY" > /home/treasury/.ssh/authorized_keys
  chown treasury:treasury /home/treasury/.ssh/authorized_keys
  chmod 600 /home/treasury/.ssh/authorized_keys
fi
exec /usr/sbin/sshd -D -e
