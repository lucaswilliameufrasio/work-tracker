#!/usr/bin/env bash
# Runs the replication S3 test against a disposable, loopback-only RustFS.
# The credentials are synthetic and exist only inside this container.
set -euo pipefail

image='rustfs/rustfs:1.0.4@sha256:ed9210d6411d77ffceb70d94b964a3cb7d225983368bc25bebe01e995293dcc2'
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
endpoint="http://127.0.0.1:${port}"
name="puxiri-replication-rustfs-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"

cleanup() {
    docker rm --force --volumes "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run --detach --rm --name "$name" \
    --publish "127.0.0.1:${port}:9000" \
    --env RUSTFS_ACCESS_KEY=test \
    --env RUSTFS_SECRET_KEY=test \
    --env RUSTFS_ADDRESS=:9000 \
    --env RUSTFS_CONSOLE_ENABLE=false \
    --volume /data \
    "$image" /data >/dev/null

for _ in $(seq 1 90); do
    # Any HTTP status means the S3 listener answers; anonymous requests are refused.
    if curl --silent --output /dev/null --max-time 2 "$endpoint/"; then
        break
    fi
    sleep 1
done
curl --silent --output /dev/null --max-time 2 "$endpoint/" || {
    echo "RustFS did not become ready at $endpoint" >&2
    exit 1
}

PUXIRI_TEST_REPLICATION_S3_ENDPOINT="$endpoint" \
    cargo test --locked --test replication -- --nocapture \
    capture_uploads_to_s3_and_a_new_installation_restores_from_it \
    remote_retention_keeps_the_newest_generations_and_only_deletes_its_own
