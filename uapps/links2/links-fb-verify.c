// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Start Links 2.30 framebuffer mode twice, sampling `/dev/fb0` while it runs.
//!
//! A painted Example Domain page is mostly light pixels, not the magenta
//! mmap-refresh bar and not an all-black shadow.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
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

struct Sample {
    unsigned long pixels;
    unsigned long black;
    unsigned long magenta;
    unsigned long light;
    unsigned long dark;
    unsigned long other;
};

static int fail(const char *msg) {
    perror(msg);
    return 1;
}

static int sample_fb(const char *tag, struct Sample *out) {
    int fd = open("/dev/fb0", O_RDONLY);
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
    if (vinfo.bits_per_pixel != 32 || finfo.smem_len == 0) {
        fprintf(stderr, "unexpected fb smem_len=%u bpp=%u\n",
                finfo.smem_len, vinfo.bits_per_pixel);
        close(fd);
        return 1;
    }

    unsigned char *buf = malloc(finfo.smem_len);
    if (!buf) {
        close(fd);
        return fail("malloc fb sample");
    }
    ssize_t n = read(fd, buf, finfo.smem_len);
    close(fd);
    if (n != (ssize_t)finfo.smem_len) {
        fprintf(stderr, "short fb read: %zd errno=%d\n", n, errno);
        free(buf);
        return 1;
    }

    memset(out, 0, sizeof(*out));
    unsigned int line = finfo.line_length;
    for (unsigned int y = 0; y < vinfo.yres; y++) {
        for (unsigned int x = 0; x < vinfo.xres; x++) {
            const unsigned char *p = buf + (size_t)y * line + (size_t)x * 4;
            unsigned char b = p[0], g = p[1], r = p[2];
            out->pixels++;
            if (b == 0 && g == 0 && r == 0) {
                out->black++;
            } else if (b == 0xff && g == 0x00 && r == 0xff) {
                out->magenta++;
            } else if (b >= 180 && g >= 180 && r >= 180) {
                out->light++;
            } else if (b <= 60 && g <= 60 && r <= 60) {
                out->dark++;
            } else {
                out->other++;
            }
        }
    }
    free(buf);

    unsigned long painted = out->light + out->dark + out->other;
    printf("links-fb-verify %s: %lux%u pixels=%lu black=%lu magenta=%lu "
           "light=%lu dark=%lu other=%lu painted=%lu\n",
           tag, (unsigned long)vinfo.xres, vinfo.yres, out->pixels, out->black,
           out->magenta, out->light, out->dark, out->other, painted);
    return 0;
}

static int looks_painted(const struct Sample *s) {
    /* Example.com in Links is a light page plus chrome; the mmap-refresh
     * magenta bar is 40 rows. Require a real page, not leftover test pixels. */
    unsigned long painted = s->light + s->dark + s->other;
    if (s->light < 20000) {
        return 0;
    }
    if (painted < (s->pixels / 8)) {
        return 0;
    }
    return 1;
}

static pid_t start_links(void) {
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return -1;
    }
    if (pid == 0) {
        execl("/usr/local/bin/links", "links", "-g", "http://example.com",
              (char *)NULL);
        perror("execl links");
        _exit(127);
    }
    return pid;
}

static void stop_links(pid_t pid) {
    if (pid <= 0) {
        return;
    }
    kill(pid, SIGTERM);
    for (int i = 0; i < 20; i++) {
        int status = 0;
        pid_t r = waitpid(pid, &status, WNOHANG);
        if (r == pid) {
            return;
        }
        usleep(100 * 1000);
    }
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
}

static int one_pass(const char *tag, unsigned wait_ms) {
    pid_t pid = start_links();
    if (pid < 0) {
        return 1;
    }
    usleep((useconds_t)wait_ms * 1000);
    struct Sample sample;
    int rc = sample_fb(tag, &sample);
    stop_links(pid);
    usleep(500 * 1000);
    if (rc != 0) {
        return rc;
    }
    if (!looks_painted(&sample)) {
        fprintf(stderr, "links-fb-verify: %s framebuffer does not look painted\n",
                tag);
        return 1;
    }
    return 0;
}

int main(void) {
    if (one_pass("first", 18000) != 0) {
        return 1;
    }
    if (one_pass("second", 12000) != 0) {
        return 1;
    }
    printf("links-fb-verify: two framebuffer starts painted ok\n");
    return 0;
}
