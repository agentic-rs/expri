#!/bin/sh
set -eu

fixture_dir=/opt/expri-fixtures
cuda_available=1
case "${EXPRI_TEST_CASE:-matched-native}" in
  matched-native|matched-python|matched-auto)
    ;;
  torch-mismatch)
    uv pip install --system --no-index --find-links "$fixture_dir/wheels" --reinstall \
      'torch==2.10.0+cu126' 'nvidia-cuda-runtime-cu12==12.6.77' 'nvidia-cublas-cu12==12.6.4.1'
    ;;
  dependency-mismatch)
    uv pip install --system --no-index --find-links "$fixture_dir/wheels" --reinstall \
      'torch==2.10.0+cu128' 'nvidia-cuda-runtime-cu12==12.8.91' 'nvidia-cublas-cu12==12.8.4.1'
    ;;
  cuda-unavailable)
    cuda_available=0
    ;;
  *)
    echo "Unknown EXPRI_TEST_CASE: $EXPRI_TEST_CASE" >&2
    exit 2
    ;;
esac

test -s /run/expri-ssh/id_ed25519.pub
install -d -m 700 -o tester -g tester /home/tester/.ssh
install -m 600 -o tester -g tester /run/expri-ssh/id_ed25519.pub /home/tester/.ssh/authorized_keys
mkdir -p /run/sshd
ssh-keygen -A
cat > /etc/profile.d/expri-fixtures.sh <<EOF
export PATH=/usr/local/bin:/usr/bin:/bin
export UV_INDEX_URL=http://worker:8000/simple
export UV_PYTHON_DOWNLOADS=never
export UV_NO_PROGRESS=1
export EXPRI_FAKE_CUDA_AVAILABLE=$cuda_available
EOF
# expri sources this profile for SSH commands as well as ordinary login shells.
printf '. /etc/profile.d/expri-fixtures.sh\n' > /home/tester/.profile
chown tester:tester /home/tester/.profile
chmod 644 /home/tester/.profile
cat > /etc/ssh/expri_sshd_config <<EOF
Port 22
ListenAddress 0.0.0.0
HostKey /etc/ssh/ssh_host_ed25519_key
PidFile /run/sshd/expri.pid
AuthorizedKeysFile .ssh/authorized_keys
PermitRootLogin no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
AllowUsers tester
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
PermitUserEnvironment no
SetEnv UV_INDEX_URL=http://worker:8000/simple UV_PYTHON_DOWNLOADS=never UV_NO_PROGRESS=1 EXPRI_FAKE_CUDA_AVAILABLE=$cuda_available
Subsystem sftp internal-sftp
EOF
/usr/sbin/sshd -t -f /etc/ssh/expri_sshd_config

index_pid=
sshd_pid=
cleanup() {
  rm -f /tmp/expri-worker.ready
  if [ -n "$index_pid" ]; then kill "$index_pid" 2>/dev/null || true; fi
  if [ -n "$sshd_pid" ]; then kill "$sshd_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
python -m http.server 8000 --bind 0.0.0.0 --directory "$fixture_dir" &
index_pid=$!
/usr/sbin/sshd -D -e -f /etc/ssh/expri_sshd_config &
sshd_pid=$!
python - <<'PY'
import socket
import time
from urllib.error import URLError
from urllib.request import urlopen

for attempt in range(50):
  try:
    with socket.create_connection(("127.0.0.1", 22), timeout=1):
      pass
    with urlopen("http://127.0.0.1:8000/simple/", timeout=1) as response:
      if response.status != 200:
        raise RuntimeError("fixture index did not respond successfully")
    break
  except (OSError, URLError):
    if attempt == 49:
      raise
    time.sleep(0.1)
PY
kill -0 "$index_pid"
kill -0 "$sshd_pid"
touch /tmp/expri-worker.ready
wait "$sshd_pid"
