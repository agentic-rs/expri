#!/bin/sh
set -eu

test -s /run/expri-ssh/id_ed25519
test -s /run/expri-ssh/known_hosts
install -d -m 700 -o tester -g tester /home/tester/.ssh
install -m 600 -o tester -g tester /run/expri-ssh/id_ed25519 /home/tester/.ssh/id_ed25519
install -m 600 -o tester -g tester /run/expri-ssh/known_hosts /home/tester/.ssh/known_hosts
cat > /home/tester/.ssh/config <<'EOF'
Host worker
  HostName worker
  User tester
  IdentityFile /home/tester/.ssh/id_ed25519
  IdentitiesOnly yes
  UserKnownHostsFile /home/tester/.ssh/known_hosts
  StrictHostKeyChecking yes
  BatchMode yes
  ConnectTimeout 5
EOF
chown tester:tester /home/tester/.ssh/config
chmod 600 /home/tester/.ssh/config
cd /home/tester
exec timeout --verbose --kill-after=10s 180s runuser -u tester -- env \
  HOME=/home/tester \
  UV_INDEX_URL=http://worker:8000/simple \
  UV_PYTHON_DOWNLOADS=never \
  EXPRI_TEST_BIN=/usr/local/bin/expri \
  /usr/local/bin/expri-container-tests --ignored --nocapture --test-threads=1 "$@"
