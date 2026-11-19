// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { WARMUP_ROUNDS = 4, SAMPLE_ROUNDS = 24, PAGE_BYTES = 4096 };

static uint64_t monotonic_ns(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) {
        perror("clock_gettime");
        exit(2);
    }
    return (uint64_t)now.tv_sec * 1000000000 + now.tv_nsec;
}

static void measure_exit(const char *name, int mapping_count,
                         size_t mapping_bytes, int resident_pages)
{
    uint64_t samples_ns[SAMPLE_ROUNDS];
    for (int round = 0; round < WARMUP_ROUNDS + SAMPLE_ROUNDS; round++) {
        int ready[2], release[2];
        if (pipe(ready) || pipe(release)) {
            perror("pipe");
            exit(2);
        }
        pid_t child = fork();
        if (child < 0) {
            perror("fork");
            exit(2);
        }
        if (child == 0) {
            close(ready[0]);
            close(release[1]);
            for (int mapping = 0; mapping < mapping_count; mapping++) {
                volatile unsigned char *base = mmap(
                    NULL, mapping_bytes, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
                if (base == MAP_FAILED) {
                    perror("mmap");
                    _exit(3);
                }
                for (int page = 0; page < resident_pages; page++) {
                    size_t offset = (size_t)page * (mapping_bytes / PAGE_BYTES)
                                    / (size_t)resident_pages * PAGE_BYTES;
                    base[offset] = (unsigned char)(page + 1);
                }
            }
            char token = 'r';
            if (write(ready[1], &token, 1) != 1)
                _exit(4);
            if (read(release[0], &token, 1) != 1)
                _exit(5);
            _exit(0);
        }
        close(ready[1]);
        close(release[0]);
        char token;
        if (read(ready[0], &token, 1) != 1) {
            fprintf(stderr, "child mapping setup failed\n");
            exit(6);
        }
        // Exclude fork, mapping creation and page faults from the interval.
        // Pipe wakeup and wait/reaping remain part of this observable latency.
        uint64_t start_ns = monotonic_ns();
        if (write(release[1], "x", 1) != 1)
            exit(7);
        int status;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status)
            || WEXITSTATUS(status) != 0)
            exit(8);
        uint64_t elapsed_ns = monotonic_ns() - start_ns;
        close(ready[0]);
        close(release[1]);
        if (round >= WARMUP_ROUNDS)
            samples_ns[round - WARMUP_ROUNDS] = elapsed_ns;
    }
    for (int left = 0; left < SAMPLE_ROUNDS; left++) {
        for (int right = left + 1; right < SAMPLE_ROUNDS; right++) {
            if (samples_ns[right] < samples_ns[left]) {
                uint64_t swap_ns = samples_ns[left];
                samples_ns[left] = samples_ns[right];
                samples_ns[right] = swap_ns;
            }
        }
    }
    printf("EXIT_TEARDOWN_BENCH %s median_us=%llu p90_us=%llu "
           "min_us=%llu max_us=%llu rounds=%d\n", name,
           (unsigned long long)(samples_ns[SAMPLE_ROUNDS / 2] / 1000),
           (unsigned long long)(samples_ns[21] / 1000),
           (unsigned long long)(samples_ns[0] / 1000),
           (unsigned long long)(samples_ns[SAMPLE_ROUNDS - 1] / 1000),
           SAMPLE_ROUNDS);
}

int main(void)
{
    setbuf(stdout, NULL);
    measure_exit("sparse_1g_64_pages", 1, (size_t)1 << 30, 64);
    measure_exit("256_vmas_one_page_each", 256, 1 << 20, 1);
    puts("EXIT_TEARDOWN_BENCH_DONE");
    return 0;
}
