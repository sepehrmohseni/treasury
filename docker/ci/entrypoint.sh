#!/bin/sh
# Entry point of the treasury-ci image: copies the read-only checkout from /src into the container
# and runs `cargo xtask` there. See scripts/docker-check.sh.
set -eu

usage() {
  cat <<'EOF'
usage: treasury-ci [xtask arguments...|shell|-- command...]
  ci [STEP...]   cargo xtask ci (the default): every check CI runs, or only the named steps
  shell          an interactive shell in the prepared copy
  -- cmd         run any command in the prepared copy
Anything else is passed to `cargo xtask`.
EOF
}

# Some tools (OpenSSH among them) refuse to run for a user id without an account.
if ! getent passwd "$(id -u)" >/dev/null 2>&1; then
  echo "ci:x:$(id -u):$(id -g):treasury ci:${HOME}:/bin/sh" >>/etc/passwd
fi
USER="$(id -un)"
export USER
mkdir -p "$HOME"

# Work on a private copy, so the checkout stays untouched and host build outputs (target/,
# node_modules/, dist/) never mix with these. tar keeps modification times, so cargo's cache in the
# target volume stays valid between runs.
W=/work/treasury
mkdir -p "$W"
tar -C /src --exclude=./.git --exclude=./target --exclude=./ui/node_modules --exclude=./ui/dist \
  --exclude=./site/node_modules --exclude=./site/book --exclude=./.work -cf - . | tar -C "$W" -xf -
cd "$W"

[ "$#" -gt 0 ] || set -- ci
case "$1" in
  shell) exec bash ;;
  --)
    shift
    exec "$@"
    ;;
  -h | --help | help) usage ;;
  *) exec cargo xtask "$@" ;;
esac
