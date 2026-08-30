// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! mmap `/dev/fb0`, write pixels without write()/FBIOPAN_DISPLAY, then read
//! them back through the file offset path.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define FBIOGET_VSCREENINFO 0x4600
#define FBIOGET_FSCREENINFO 0x4602

struct fb_bitfield {
    uint32_t offset;
    uint32_t length;
    uint32_t msb_right;
};

struct fb_var_screeninfo {
    uint32_t xres;
    uint32_t yres;
    uint32_t xres_virtual;
    uint32_t yres_virtual;
    uint32_t xoffset;
    uint32_t yoffset;
    uint32_t bits_per_pixel;
    uint32_t grayscale;
    struct fb_bitfield red;
    struct fb_bitfield green;
    struct fb_bitfield blue;
    struct fb_bitfield transp;
    uint32_t nonstd;
    uint32_t activate;
    uint32_t height;
    uint32_t width;
    uint32_t accel_flags;
    uint32_t pixclock;
    uint32_t left_margin;
    uint32_t right_margin;
    uint32_t upper_margin;
    uint32_t lower_margin;
    uint32_t hsync_len;
    uint32_t vsync_len;
    uint32_t sync;
    uint32_t vmode;
    uint32_t rotate;
    uint32_t colorspace;
    uint32_t reserved[4];
};

struct fb_fix_screeninfo {
    char id[16];
    unsigned long smem_start;
    uint32_t smem_len;
    uint32_t type;
    uint32_t type_aux;
    uint32_t visual;
    uint16_t xpanstep;
    uint16_t ypanstep;
    uint16_t ywrapstep;
    uint32_t line_length;
    unsigned long mmio_start;
    uint32_t mmio_len;
    uint32_t accel;
    uint16_t capabilities;
    uint16_t reserved[2];
};

static int fail(const char *msg) {
    perror(msg);
    return 1;
}

int main(void) {
    int fd = open("/dev/fb0", O_RDWR);
    if (fd < 0) {
        return fail("open /dev/fb0");
    }

    struct fb_var_screeninfo vinfo;
    struct fb_fix_screeninfo finfo;
    if (ioctl(fd, FBIOGET_VSCREENINFO, &vinfo) < 0) {
        return fail("FBIOGET_VSCREENINFO");
    }
    if (ioctl(fd, FBIOGET_FSCREENINFO, &finfo) < 0) {
        return fail("FBIOGET_FSCREENINFO");
    }
    if (finfo.smem_len == 0 || vinfo.bits_per_pixel != 32) {
        fprintf(stderr, "unexpected fb geometry smem_len=%u bpp=%u\n",
                finfo.smem_len, vinfo.bits_per_pixel);
        return 1;
    }

    size_t size = finfo.smem_len;
    unsigned char *map = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        return fail("mmap");
    }

    /* Draw a visible magenta bar at the top of the screen for graphic demo. */
    unsigned int line = finfo.line_length;
    for (unsigned int y = 0; y < 40 && y < vinfo.yres; y++) {
        for (unsigned int x = 0; x < vinfo.xres; x++) {
            unsigned char *p = map + y * line + x * 4;
            p[0] = 0xff; /* B */
            p[1] = 0x00; /* G */
            p[2] = 0xff; /* R */
            p[3] = 0xff;
        }
    }

    /* Place the offset pattern below the magenta bar so the two checks
     * do not overwrite each other. */
    const size_t off = (size_t)40 * line + 16;
    if (size <= off + 16) {
        fprintf(stderr, "framebuffer too small: %zu\n", size);
        return 1;
    }
    unsigned char pattern[16];
    for (int i = 0; i < 16; i++) {
        pattern[i] = (unsigned char)(0xa0 + i);
    }
    memcpy(map + off, pattern, sizeof(pattern));

    /* Do not call write() or FBIOPAN_DISPLAY. Give the refresh task time. */
    usleep(200 * 1000);

    unsigned char got[16];
    if (lseek(fd, (off_t)off, SEEK_SET) < 0) {
        return fail("lseek");
    }
    ssize_t n = read(fd, got, sizeof(got));
    if (n != (ssize_t)sizeof(got)) {
        fprintf(stderr, "short read: %zd errno=%d\n", n, errno);
        return 1;
    }
    if (memcmp(got, pattern, sizeof(pattern)) != 0) {
        fprintf(stderr, "offset readback mismatch\n");
        return 1;
    }

    printf("fb-mmap-refresh: mmap write visible in offset readback (%ux%u)\n",
           vinfo.xres, vinfo.yres);
    munmap(map, size);
    close(fd);
    return 0;
}
