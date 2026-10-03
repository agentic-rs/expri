#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

for tool in docker ssh-keygen; do
  if ! command -v "$tool" >/dev/null; then
    echo "Container integration requires $tool." >&2
    exit 1
  fi
done

build=true
if [[ "${1:-}" == --no-build ]]; then
  build=false
  shift
fi
if [[ "$#" == 0 ]]; then
  set -- matched-native matched-python matched-auto torch-mismatch dependency-mismatch cuda-unavailable
fi
for test_case in "$@"; do
  case "$test_case" in
    matched-native|matched-python|matched-auto|torch-mismatch|dependency-mismatch|cuda-unavailable) ;;
    *) echo "Unknown container case: $test_case" >&2; exit 1 ;;
  esac
done

state_dir="$(mktemp -d "${TMPDIR:-/tmp}/expri-containers.XXXXXX")"
logs_dir="${EXPRI_CONTAINER_ARTIFACTS:-}"
if [[ -z "$logs_dir" ]]; then
  logs_dir="$(mktemp -d "${TMPDIR:-/tmp}/expri-container-logs.XXXXXX")"
fi
mkdir -p "$logs_dir" "$state_dir/ssh"
logs_dir="$(cd "$logs_dir" && pwd)"
echo "Container workflow logs: $logs_dir"
network="expri-ci-$$"
worker=""
host=""
network_created=false

cleanup_containers() {
  if [[ -n "$host" ]]; then docker rm --force "$host" >/dev/null 2>&1 || true; fi
  if [[ -n "$worker" ]]; then docker rm --force "$worker" >/dev/null 2>&1 || true; fi
  host=""
  worker=""
}
cleanup() {
  cleanup_containers
  if [[ "$network_created" == true ]]; then docker network rm "$network" >/dev/null 2>&1 || true; fi
  rm -rf "$state_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

logged() {
  local log_file="$1"
  shift
  if "$@" >"$log_file" 2>&1; then
    tail -n 4 "$log_file"
  else
    tail -n 60 "$log_file" >&2
    return 1
  fi
}

if [[ "$build" == true ]]; then
  echo "Building worker and host CI images. Logs: $logs_dir"
  logged "$logs_dir/build-worker.log" docker build --target worker --tag expri-ci-worker --file tests/containers/Dockerfile .
  logged "$logs_dir/build-host.log" docker build --target host --tag expri-ci-host --file tests/containers/Dockerfile .
fi

# Neither image publishes ports. Runtime dependencies are served by the worker
# on this internal network; tests cannot reach external package indexes.
docker network create --internal "$network" >/dev/null
network_created=true
ssh-keygen -q -t ed25519 -N '' -C expri-container-ci -f "$state_dir/ssh/id_ed25519"

for test_case in "$@"; do
  echo "Testing $test_case"
  worker="$network-worker"
  host="$network-host"
  docker create --name "$worker" --network "$network" --network-alias worker \
    --env "EXPRI_TEST_CASE=$test_case" expri-ci-worker >/dev/null
  # Copy only fixture keys; project/run directories never share a volume.
  docker cp "$state_dir/ssh/id_ed25519.pub" "$worker:/run/expri-ssh/id_ed25519.pub"
  docker start "$worker" >/dev/null
  ready=false
  for ((attempt=0; attempt<60; attempt++)); do
    if docker exec "$worker" test -f /tmp/expri-worker.ready >/dev/null 2>&1; then
      ready=true
      break
    fi
    if [[ "$(docker inspect --format '{{.State.Running}}' "$worker")" != true ]]; then break; fi
    sleep 1
  done
  if [[ "$ready" != true ]]; then
    docker logs "$worker" >"$logs_dir/$test_case.worker.log" 2>&1 || true
    tail -n 60 "$logs_dir/$test_case.worker.log" >&2
    echo "Worker did not become ready." >&2
    exit 1
  fi
  host_key="$(docker exec "$worker" cat /etc/ssh/ssh_host_ed25519_key.pub)"
  printf 'worker %s\n' "$host_key" >"$state_dir/ssh/known_hosts"
  docker create --name "$host" --network "$network" \
    --env "EXPRI_TEST_CASE=$test_case" expri-ci-host >/dev/null
  docker cp "$state_dir/ssh/." "$host:/run/expri-ssh/"
  if ! logged "$logs_dir/$test_case.host.log" docker start --attach "$host"; then
    docker logs "$worker" >"$logs_dir/$test_case.worker.log" 2>&1 || true
    exit 1
  fi
  exit_code="$(docker inspect --format '{{.State.ExitCode}}' "$host")"
  docker logs "$worker" >"$logs_dir/$test_case.worker.log" 2>&1 || true
  if [[ "$exit_code" != 0 ]]; then
    tail -n 60 "$logs_dir/$test_case.host.log" >&2
    exit "$exit_code"
  fi
  cleanup_containers
done

echo "All container scenarios passed. Logs: $logs_dir"
