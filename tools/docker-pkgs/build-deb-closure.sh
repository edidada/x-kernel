#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# build-deb-closure.sh — prepare prebuilt offline Docker assets once.
#
# Generates:
#   <out>/{manifest.json,SHA256SUMS}
#   <out>/aarch64/debs/*.deb          docker engine + CLI + containerd deb closure
#   <out>/aarch64/images/*.tar        docker save (hello-world / busybox / alpine)
#
# Requires network and a Linux host with `docker`, plus either:
#   - an arm64 host, or
#   - an amd64 host with qemu-user-static registered for arm64 emulation:
#       docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
# The deb closure is built inside a clean `debian:bookworm` arm64 container so
# dependency resolution never depends on the host's own package set.
#
# Usage:
#   tools/docker-pkgs/build-deb-closure.sh [OUT_DIR]   # default: repo tools/docker-pkgs
#
# Running this script is a one-time action on a networked machine. Everything
# downstream (make dockerfs, guest first boot) is offline.

set -eu

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
OUT_DIR=${1:-"${REPO_ROOT}/tools/docker-pkgs"}
ARCH_DIR="${OUT_DIR}/aarch64"
DEB_DIR="${ARCH_DIR}/debs"
IMG_DIR="${ARCH_DIR}/images"

DOCKER_PLATFORM=${DOCKER_PLATFORM:-linux/arm64}
DEBIAN_SUITE=${DEBIAN_SUITE:-bookworm}
DOCKER_REPO_SUITE="debian-${DEBIAN_SUITE}"

# Image pins: fixed tags, never `latest`.
BUSYBOX_TAG=${BUSYBOX_TAG:-1.36.1}
ALPINE_TAG=${ALPINE_TAG:-3.19}
HELLO_WORLD_TAG=${HELLO_WORLD_TAG:-latest}

# Recorded in manifest.json so regeneration is auditable.
MANIFEST_JSON="${OUT_DIR}/manifest.json"
SHA256SUMS="${OUT_DIR}/SHA256SUMS"

log() { echo "[docker-assets] $*"; }

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required to prepare docker assets" >&2
    exit 1
}
command -v sha256sum >/dev/null 2>&1 || {
    echo "error: sha256sum is required" >&2
    exit 1
}

mkdir -p "${DEB_DIR}" "${IMG_DIR}"
: > /tmp/xk-deb-manifest.txt   # filename:version lines produced by the container

log "building docker deb closure for ${DOCKER_PLATFORM} (debian ${DEBIAN_SUITE})"
# A single container run performs the whole download-only install and prints a
# version manifest; outputs land in a bind-mounted staging directory so host and
# container agree on exact bytes without `docker cp` round-trips.
docker run --rm --platform "${DOCKER_PLATFORM}" \
    -v "${DEB_DIR}:/artifacts" \
    -e "DOCKER_REPO_SUITE=${DOCKER_REPO_SUITE}" \
    "${DOCKER_IMAGE:-debian:${DEBIAN_SUITE}}" \
    sh -euxc '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends apt-transport-https ca-certificates curl gnupg
        install -m 0755 -d /etc/apt/keyrings
        curl -fsSL https://download.docker.com/linux/debian/gpg \
            -o /etc/apt/keyrings/docker.asc
        chmod a+r /etc/apt/keyrings/docker.asc
        printf "deb [arch=arm64 signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/debian ${DOCKER_REPO_SUITE} stable\n" \
            > /etc/apt/sources.list.d/docker.list
        apt-get update
        apt-get install -y --download-only \
            docker-ce docker-ce-cli containerd.io \
            docker-buildx-plugin docker-compose-plugin
        find /var/cache/apt/archives -maxdepth 1 -name "*.deb" \
            -exec chmod 0644 {} \; -exec cp {} /artifacts/ \;
        # Record resolved versions for the manifest (readable dpkg versions).
        dpkg-query -W -f="${Package} ${Version}\n" \
            docker-ce docker-ce-cli containerd.io \
            docker-buildx-plugin docker-compose-plugin \
            > /artifacts/versions.txt
    ' || {
        echo "error: deb closure build failed; on amd64 hosts you likely need" >&2
        echo "  docker run --rm --privileged multiarch/qemu-user-static --reset -p yes" >&2
        exit 1
    }

# Move the version manifest out of the artifacts dir before copying debs.
VERSIONS_FILE="${DEB_DIR}/versions.txt"
cp "${VERSIONS_FILE}" /tmp/xk-versions.txt 2>/dev/null || true
rm -f "${VERSIONS_FILE}"

log "pulling arm64 container images"
docker pull --platform linux/arm64 "busybox:${BUSYBOX_TAG}"
docker pull --platform linux/arm64 "alpine:${ALPINE_TAG}"
docker pull --platform linux/arm64 "hello-world:${HELLO_WORLD_TAG}"

log "saving container images"
docker save "busybox:${BUSYBOX_TAG}" -o "${IMG_DIR}/busybox-${BUSYBOX_TAG}.tar"
docker save "alpine:${ALPINE_TAG}" -o "${IMG_DIR}/alpine-${ALPINE_TAG}.tar"
docker save "hello-world:${HELLO_WORLD_TAG}" -o "${IMG_DIR}/hello-world-${HELLO_WORLD_TAG}.tar"

log "writing ${SHA256SUMS}"
(cd "${OUT_DIR}" && find aarch64 -type f -print0 | sort -z | xargs -0 sha256sum) > "${SHA256SUMS}"

log "writing ${MANIFEST_JSON}"
{
    printf '{\n'
    printf '  "generated_at": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '  "arch": "aarch64",\n'
    printf '  "platform": "%s",\n' "${DOCKER_PLATFORM}"
    printf '  "debian_suite": "%s",\n' "${DEBIAN_SUITE}"
    printf '  "versions": {\n'
    # versions.txt lines are "Package Version" pairs; emit JSON entries.
    awk -F' ' 'NF >= 2 {
        printf "    \"%s\": \"%s\",\n", $1, $2
    }' /tmp/xk-versions.txt | sed '$ s/,$//'
    printf '  },\n'
    printf '  "images": [\n'
    printf '    { "repository": "busybox", "tag": "%s", "file": "busybox-%s.tar" },\n' \
        "${BUSYBOX_TAG}" "${BUSYBOX_TAG}"
    printf '    { "repository": "alpine", "tag": "%s", "file": "alpine-%s.tar" },\n' \
        "${ALPINE_TAG}" "${ALPINE_TAG}"
    printf '    { "repository": "hello-world", "tag": "%s", "file": "hello-world-%s.tar" }\n' \
        "${HELLO_WORLD_TAG}" "${HELLO_WORLD_TAG}"
    printf '  ],\n'
    printf '  "commands": [\n'
    printf '    "tools/docker-pkgs/build-deb-closure.sh %s"\n' "${OUT_DIR}"
    printf '  ]\n'
    printf '}\n'
} > "${MANIFEST_JSON}"

rm -f /tmp/xk-deb-manifest.txt /tmp/xk-versions.txt
log "done: $(find "${DEB_DIR}" -name '*.deb' | wc -l) debs + $(find "${IMG_DIR}" -name '*.tar' | wc -l) images (manifest: ${MANIFEST_JSON})"