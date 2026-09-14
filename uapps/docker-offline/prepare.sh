#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# prepare.sh — stage prebuilt offline Docker assets for debugfs injection.
#
# Called by the uapp framework from this uapp's directory. REPO_ROOT is set by
# the framework. Fails fast (with guidance) whenever the prebuilt asset set is
# missing or fails sha256 verification, so `make dockerfs` can never inject a
# partial or corrupt set.

set -eu

PKGS_DIR="${REPO_ROOT}/tools/docker-pkgs"
ARCH="aarch64"

fail_missing_assets() {
    echo "docker-offline: prebuilt offline assets not found: ${PKGS_DIR}" >&2
    echo "docker-offline: prepare them once on a networked machine:" >&2
    echo "  make docker-assets" >&2
    echo "docker-offline: see tools/docker-pkgs/README.md for requirements" >&2
    exit 1
}

[ -d "${PKGS_DIR}/../.." ] || fail_missing_assets
[ -f "${PKGS_DIR}/manifest.json" ] || fail_missing_assets
[ -f "${PKGS_DIR}/SHA256SUMS" ] || fail_missing_assets
[ -d "${PKGS_DIR}/${ARCH}/debs" ] || fail_missing_assets
[ -d "${PKGS_DIR}/${ARCH}/images" ] || fail_missing_assets

# Verify every staged artifact against the recorded checksums before staging.
(cd "${PKGS_DIR}" && sha256sum -c SHA256SUMS --quiet) || {
    echo "docker-offline: sha256 verification of prebuilt assets failed" >&2
    echo "docker-offline: re-run 'make docker-assets' and retry" >&2
    exit 1
}

rm -rf out
mkdir -p out/pkgs out/images out/etc-docker

cp "${PKGS_DIR}/${ARCH}/debs/"*.deb out/pkgs/
cp "${PKGS_DIR}/${ARCH}/images/"*.tar out/images/
cp docker/daemon.json out/etc-docker/daemon.json
cp docker/xk-docker-setup out/xk-docker-setup

echo "docker-offline: staged $(find out/pkgs -name '*.deb' | wc -l) debs, \
$(find out/images -name '*.tar' | wc -l) images for injection"