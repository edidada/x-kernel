// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wait for TTY bytes on stdin so host QMP key injection can be verified.

#define _GNU_SOURCE
#include <poll.h>
#include <stdio.h>
#include <termios.h>
#include <unistd.h>

int main(void) {
    struct termios oldt, raw;
    if (tcgetattr(0, &oldt) != 0) {
        perror("tcgetattr");
        return 1;
    }
    raw = oldt;
    cfmakeraw(&raw);
    raw.c_lflag |= ISIG;
    if (tcsetattr(0, TCSANOW, &raw) != 0) {
        perror("tcsetattr");
        return 1;
    }

    printf("kbd-probe: waiting\n");
    fflush(stdout);

    struct pollfd pfd = {.fd = 0, .events = POLLIN};
    int pr = poll(&pfd, 1, 8000);
    unsigned char buf[64];
    ssize_t n = 0;
    if (pr > 0) {
        n = read(0, buf, sizeof(buf));
    }
    tcsetattr(0, TCSANOW, &oldt);

    if (n > 0) {
        printf("kbd-probe: got %zd bytes:", n);
        for (ssize_t i = 0; i < n; i++) {
            printf(" %02x", buf[i]);
        }
        printf("\n");
        return 0;
    }
    printf("kbd-probe: no bytes (poll=%d)\n", pr);
    return 1;
}
