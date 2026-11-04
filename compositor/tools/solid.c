// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Solid-color DRM test: raw ioctls only, no libdrm at runtime or link time.
// The headers supply the wire structs; the ioctl numbers come from drm.h.
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm.h>
#include <drm_mode.h>

static void die(const char *what) {
    fprintf(stderr, "%s: %s\n", what, strerror(errno));
    exit(1);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/dev/dri/card0";
    uint32_t color = argc > 2 ? (uint32_t)strtoul(argv[2], NULL, 0) : 0x003060c0;

    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) die("open card");

    // Two-pass GETRESOURCES: counts first, then the id arrays.
    struct drm_mode_card_res res = {0};
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0) die("getresources");
    uint32_t crtcs[16] = {0}, connectors[16] = {0}, encoders[16] = {0};
    res.crtc_id_ptr = (uint64_t)(uintptr_t)crtcs;
    res.connector_id_ptr = (uint64_t)(uintptr_t)connectors;
    res.encoder_id_ptr = (uint64_t)(uintptr_t)encoders;
    res.count_crtcs = res.count_crtcs < 16 ? res.count_crtcs : 16;
    res.count_connectors = res.count_connectors < 16 ? res.count_connectors : 16;
    res.count_encoders = res.count_encoders < 16 ? res.count_encoders : 16;
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0) die("getresources ids");

    uint32_t connector = 0, crtc = 0;
    struct drm_mode_modeinfo mode = {0};
    for (uint32_t i = 0; i < res.count_connectors && !connector; i++) {
        struct drm_mode_get_connector conn = {0};
        conn.connector_id = connectors[i];
        if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) < 0) continue;
        if (conn.connection != 1 /* DRM_MODE_CONNECTED */ || conn.count_modes == 0) continue;

        struct drm_mode_modeinfo modes[8] = {0};
        uint32_t encs[8] = {0};
        conn.modes_ptr = (uint64_t)(uintptr_t)modes;
        conn.encoders_ptr = (uint64_t)(uintptr_t)encs;
        conn.count_modes = conn.count_modes < 8 ? conn.count_modes : 8;
        conn.count_encoders = conn.count_encoders < 8 ? conn.count_encoders : 8;
        if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) < 0) continue;

        uint32_t enc_id = conn.encoder_id ? conn.encoder_id : encs[0];
        if (enc_id) {
            struct drm_mode_get_encoder enc = {0};
            enc.encoder_id = enc_id;
            if (ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &enc) == 0) crtc = enc.crtc_id;
        }
        if (!crtc && res.count_crtcs > 0) crtc = crtcs[0];
        if (!crtc) continue;
        connector = connectors[i];
        mode = modes[0];
    }
    if (!connector) { fprintf(stderr, "no connected connector\n"); return 1; }
    printf("output: connector %u crtc %u mode %ux%u@%u\n",
           connector, crtc, mode.hdisplay, mode.vdisplay, mode.vrefresh);
    fflush(stdout);

    struct drm_mode_create_dumb dumb = {0};
    dumb.width = mode.hdisplay;
    dumb.height = mode.vdisplay;
    dumb.bpp = 32;
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) < 0) die("create dumb");
    printf("dumb handle=%u pitch=%u size=%llu\n", dumb.handle, dumb.pitch,
           (unsigned long long)dumb.size);
    fflush(stdout);

    struct drm_mode_fb_cmd fb = {0};
    fb.width = mode.hdisplay;
    fb.height = mode.vdisplay;
    fb.pitch = dumb.pitch;
    fb.bpp = 32;
    fb.depth = 24;
    fb.handle = dumb.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) < 0) die("addfb");
    printf("fb %u\n", fb.fb_id);
    fflush(stdout);

    struct drm_mode_map_dumb map_req = {0};
    map_req.handle = dumb.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map_req) < 0) die("map dumb");
    void *map = mmap(NULL, dumb.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, map_req.offset);
    if (map == MAP_FAILED) die("mmap");
    for (uint64_t i = 0; i + 4 <= dumb.size; i += 4) *(uint32_t *)((char *)map + i) = color;
    munmap(map, dumb.size);

    struct drm_mode_crtc set = {0};
    set.crtc_id = crtc;
    set.fb_id = fb.fb_id;
    set.set_connectors_ptr = (uint64_t)(uintptr_t)&connector;
    set.count_connectors = 1;
    set.mode_valid = 1;
    set.mode = mode;
    if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &set) < 0) die("setcrtc");
    printf("setcrtc ok, color %#010x, holding 60s\n", color);
    fflush(stdout);
    sleep(60);

    set.fb_id = 0;
    set.count_connectors = 0;
    set.mode_valid = 0;
    ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &set);
    ioctl(fd, DRM_IOCTL_MODE_RMFB, &fb.fb_id);
    close(fd);
    return 0;
}
