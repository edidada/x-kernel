// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Probe Linux mousedev `/dev/input/mice` the way Links 2.30 does.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int fail(const char *msg) {
    perror(msg);
    return 1;
}

static int read_exact(int fd, unsigned char *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(fd, buf + got, n - got);
        if (r < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (r == 0) {
            errno = EPIPE;
            return -1;
        }
        got += (size_t)r;
    }
    return 0;
}

static int expect_ack_id(int fd, unsigned char want_id) {
    unsigned char c = 0;
    do {
        if (read_exact(fd, &c, 1) < 0) {
            return fail("read ACK");
        }
    } while (c != 0xfa);
    if (read_exact(fd, &c, 1) < 0) {
        return fail("read ID");
    }
    if (c != want_id) {
        fprintf(stderr, "unexpected mouse id %u want %u\n", c, want_id);
        return 1;
    }
    return 0;
}

int main(void) {
    int fd = open("/dev/input/mice", O_RDWR);
    if (fd < 0) {
        return fail("open /dev/input/mice");
    }

    static const unsigned char imps[] = {0xf3, 200, 0xf3, 100, 0xf3, 80, 0xf2};
    static const unsigned char imex[] = {0xf3, 200, 0xf3, 200, 0xf3, 80, 0xf2};
    if (write(fd, imps, sizeof(imps)) != (ssize_t)sizeof(imps)) {
        return fail("write IMPS probe");
    }
    if (expect_ack_id(fd, 3) != 0) {
        return 1;
    }
    if (write(fd, imex, sizeof(imex)) != (ssize_t)sizeof(imex)) {
        return fail("write EXPS probe");
    }
    if (expect_ack_id(fd, 4) != 0) {
        return 1;
    }

    /* Short reads must be allowed; Links2 reads one byte while hunting ACK. */
    unsigned char reset = 0xff;
    if (write(fd, &reset, 1) != 1) {
        return fail("write reset");
    }
    unsigned char one;
    if (read(fd, &one, 1) != 1) {
        return fail("short read after reset");
    }
    if (one != 0xfa) {
        fprintf(stderr, "reset ACK was %u\n", one);
        return 1;
    }

    printf("mousedev-ps2: IMPS id=3 EXPS id=4 short-read ok\n");
    close(fd);
    return 0;
}
