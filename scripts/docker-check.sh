#!/usr/bin/env bash
# Runs `cargo xtask` in Docker, so the checks work on any Linux or macOS machine with Docker,
# without installing Rust, Node, pnpm or the WebKitGTK headers.
#
#   scripts/docker-check.sh                 every check CI runs (cargo xtask ci)
#   scripts/docker-check.sh ci fmt clippy   only the named steps; `cargo xtask ci --list` lists them
#   scripts/docker-check.sh fmt             cargo fmt --all on the real checkout
#   scripts/docker-check.sh lock            add new dependencies to Cargo.lock (upgrades nothing)
#   scripts/docker-check.sh release 0.2.0   cargo xtask release on the real checkout (commits, tags)
#   scripts/docker-check.sh bundle DIR      the desktop app's Linux packages (.deb, .rpm) in DIR
#   scripts/docker-check.sh shell           a shell in the prepared container
#   scripts/docker-check.sh -- CMD          any command, for example: -- cargo test -p treasury-core
#
# The checkout is mounted read-only and copied inside the container, so nothing is written to it
# (except by `fmt`, `lock` and `release`, which exist to edit it). Caches live in the Docker volumes
# treasury-ci-cargo, treasury-ci-target and treasury-ci-pnpm; remove them with:
#   docker volume rm treasury-ci-cargo treasury-ci-target treasury-ci-pnpm
#
# Environment:
#   TREASURY_CI_CPUS        CPU cores to use (default: half of them, so the machine stays usable)
#   TREASURY_CI_DOCKER=off  do not pass the Docker socket in; the integration steps then report
#                           themselves as skipped
#   TREASURY_CI_GPU=off     do not pass /dev/dri in (the desktop smoke test renders in software)
#   TREASURY_CI_RUST_IMAGE  build the image from another Rust release, e.g. rust:1.101.0-trixie
#   TREASURY_CI_TARGET_GB   start the build cache afresh once it grows past this (default 30)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${TREASURY_CI_IMAGE:-treasury-ci}"
cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)"
cpus="${TREASURY_CI_CPUS:-$(( cores > 1 ? cores / 2 : 1 ))}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is not installed or not on PATH" >&2
  exit 1
fi

build=(docker build --tag "$image")
if [ -n "${TREASURY_CI_RUST_IMAGE:-}" ]; then
  build+=(--build-arg "RUST_IMAGE=$TREASURY_CI_RUST_IMAGE")
fi
if docker image inspect "$image" >/dev/null 2>&1; then
  # Already built: a quick, quiet rebuild that only redoes layers whose inputs changed.
  "${build[@]}" --quiet "$root/docker/ci" >/dev/null
else
  echo "Building the $image image once (downloads Rust, Node, the cargo tools and WebKitGTK)..." >&2
  "${build[@]}" "$root/docker/ci" >&2
fi

tty=()
if [ -t 0 ] && [ -t 1 ]; then
  tty=(--interactive --tty)
fi

# Integration tests start database containers next to this one through the host's Docker daemon,
# and reach their published ports through the host gateway.
dockersock=()
if [ "${TREASURY_CI_DOCKER:-auto}" != off ] && [ -S /var/run/docker.sock ]; then
  if [ "$(uname -s)" = Darwin ]; then
    sockgid=0
  else
    sockgid="$(stat -c %g /var/run/docker.sock)"
  fi
  dockersock=(--volume /var/run/docker.sock:/var/run/docker.sock
    --group-add "$sockgid"
    --add-host host.docker.internal:host-gateway
    --env TESTCONTAINERS_HOST_OVERRIDE=host.docker.internal
    --env TREASURY_DOCKER=1)
fi

gpu=()
if [ "${TREASURY_CI_GPU:-auto}" != off ] && [ -d /dev/dri ]; then
  gpu=(--device /dev/dri)
  # The render nodes belong to these groups; the container user needs them to open the GPU.
  for group in render video; do
    gid="$(getent group "$group" 2>/dev/null | cut -d: -f3 || true)"
    if [ -n "$gid" ]; then
      gpu+=(--group-add "$gid")
    fi
  done
fi

common=(--rm
  --cpus "$cpus"
  --env "CARGO_BUILD_JOBS=$cpus"
  --env "TREASURY_CI_CPUS=$cpus"
  --env "TREASURY_CI_TARGET_GB=${TREASURY_CI_TARGET_GB:-30}"
  --user "$(id -u):$(id -g)"
  --volume treasury-ci-cargo:/cache/cargo
  --volume treasury-ci-target:/cache/target
  --volume treasury-ci-pnpm:/cache/pnpm)

if [ "${1:-}" = fmt ]; then
  exec docker run "${common[@]}" --volume "$root:/repo:z" --workdir /repo \
    --entrypoint cargo "$image" fmt --all
fi

if [ "${1:-}" = lock ]; then
  # `cargo metadata` records new dependencies in Cargo.lock without upgrading existing ones.
  exec docker run "${common[@]}" --volume "$root:/repo:z" --workdir /repo \
    --entrypoint cargo "$image" metadata --format-version 1 --quiet >/dev/null
fi

if [ "${1:-}" = release ]; then
  # The release task edits manifests, commits and tags, so it runs on the real checkout with the
  # caller's git identity. Build outputs still go to the target volume.
  name="$(git -C "$root" config user.name)"
  email="$(git -C "$root" config user.email)"
  exec docker run "${common[@]}" ${tty[@]+"${tty[@]}"} \
    --volume "$root:/repo:z" --workdir /repo \
    --env "GIT_AUTHOR_NAME=$name" --env "GIT_AUTHOR_EMAIL=$email" \
    --env "GIT_COMMITTER_NAME=$name" --env "GIT_COMMITTER_EMAIL=$email" \
    --entrypoint cargo "$image" xtask "$@"
fi

if [ "${1:-}" = bundle ]; then
  # The packages are for this machine's architecture. They are not signed for the updater;
  # releases are built and signed by the release workflow.
  out="${2:?usage: scripts/docker-check.sh bundle DIR}"
  mkdir -p "$out"
  out="$(cd "$out" && pwd)"
  exec docker run "${common[@]}" --volume "$root:/src:ro,z" --volume "$out:/out:z" \
    "$image" bundle
fi

# Running as your own user id keeps the read-only checkout readable even with a private umask.
# The ",z" label lets SELinux hosts (Fedora, RHEL) share the mount; elsewhere it is ignored.
exec docker run "${common[@]}" ${tty[@]+"${tty[@]}"} ${gpu[@]+"${gpu[@]}"} \
  ${dockersock[@]+"${dockersock[@]}"} \
  --volume "$root:/src:ro,z" \
  "$image" "$@"
