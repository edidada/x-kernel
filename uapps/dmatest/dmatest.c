/*
 * dmatest.c — dma-buf / dma-heap acceptance test.
 *
 *     ~ # /bin/dmatest
 *
 * What it proves: the whole userspace-visible path works —
 *     open(/dev/dma_heap/...) -> DMA_HEAP_IOCTL_ALLOC -> mmap -> read/write,
 * and that repeated allocation does not leak (the fd table and the buffer
 * objects are both bounded, so a leak shows up within a few hundred rounds
 * rather than after the machine has been up for a day).
 *
 * What it does NOT prove: cache-coherent DMA. The two DMA_BUF_IOCTL_SYNC
 * calls execute and must return success, but the same CPU wrote and read the
 * buffer, so its own caches agree either way. Real coverage needs a device
 * that actually DMAs; until then this only confirms the ioctls are wired.
 *
 * The structs and ioctl numbers below are written out by hand because
 * userspace cannot include the kernel headers. _IOC encodes sizeof into the
 * request code, so a wrong struct size is not a misaligned argument — it is a
 * *different ioctl number*, and the kernel answers with a loud error instead
 * of reading the wrong bytes.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

/* Must match io/dmabuf/src/uapi.rs byte for byte. */
struct dma_heap_allocation_data {
    uint64_t len;
    uint32_t fd;
    uint32_t fd_flags;
    uint64_t heap_flags;
};
#define DMA_HEAP_IOCTL_ALLOC _IOWR('H', 0, struct dma_heap_allocation_data)

#define DMA_BUF_SYNC_READ  (1U << 0)
#define DMA_BUF_SYNC_WRITE (1U << 1)
#define DMA_BUF_SYNC_START (0U << 2)
#define DMA_BUF_SYNC_END   (1U << 2)

struct dma_buf_sync {
    uint64_t flags;
};
#define DMA_BUF_IOCTL_SYNC _IOW('b', 0, struct dma_buf_sync)

static int g_pass, g_fail;

static void ok(const char *what)
{
    printf("  [pass] %s\n", what);
    g_pass++;
}

static void fail(const char *what, const char *why)
{
    printf("  [FAIL] %s — %s\n", what, why);
    g_fail++;
}

static uint8_t pattern(uint8_t seed, size_t i)
{
    return (uint8_t)(seed + i * 7U);
}

/* One full allocate -> map -> write -> sync -> verify -> unmap -> close. */
static int roundtrip(int heap_fd, size_t len, uint8_t seed, const char *tag)
{
    struct dma_heap_allocation_data args;
    memset(&args, 0, sizeof(args));
    args.len = len;
    args.fd_flags = O_RDWR;

    if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &args) < 0) {
        printf("  [FAIL] %s: alloc(%zu): %s\n", tag, len, strerror(errno));
        g_fail++;
        return -1;
    }
    if (args.fd == 0 || args.fd == (uint32_t)-1) {
        printf("  [FAIL] %s: implausible fd %d\n", tag, (int)args.fd);
        g_fail++;
        return -1;
    }

    int bfd = (int)args.fd;
    uint8_t *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, bfd, 0);
    if (p == MAP_FAILED) {
        printf("  [FAIL] %s: mmap(%zu): %s\n", tag, len, strerror(errno));
        g_fail++;
        close(bfd);
        return -1;
    }

    for (size_t i = 0; i < len; i++)
        p[i] = pattern(seed, i);

    /* Wrote it and want it published to the device: END|WRITE = clean. */
    struct dma_buf_sync s;
    s.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE;
    if (ioctl(bfd, DMA_BUF_IOCTL_SYNC, &s) < 0) {
        printf("  [FAIL] %s: SYNC(END|WRITE): %s\n", tag, strerror(errno));
        g_fail++;
    }

    /* About to read what the device wrote: START|READ = invalidate. */
    s.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ;
    if (ioctl(bfd, DMA_BUF_IOCTL_SYNC, &s) < 0) {
        printf("  [FAIL] %s: SYNC(START|READ): %s\n", tag, strerror(errno));
        g_fail++;
    }

    for (size_t i = 0; i < len; i++) {
        if (p[i] != pattern(seed, i)) {
            printf("  [FAIL] %s: mismatch @%zu (want 0x%02x got 0x%02x)\n", tag, i,
                   pattern(seed, i), p[i]);
            g_fail++;
            break;
        }
    }

    munmap(p, len);
    close(bfd); /* The last reference returns the pages to the heap. */
    return 0;
}

int main(void)
{
    printf("=== dmatest: dma-buf / dma-heap ===\n");

    /* 1. Any heap node. Vendors reach for cma first and fall back to system. */
    const char *names[] = { "/dev/dma_heap/cma", "/dev/dma_heap/system" };
    int heap_fd = -1;
    const char *heap_name = NULL;
    for (unsigned i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
        heap_fd = open(names[i], O_RDWR);
        if (heap_fd >= 0) {
            heap_name = names[i];
            break;
        }
    }
    if (heap_fd < 0) {
        printf("[FAIL] no dma-heap node (tried %s, %s): %s\n", names[0], names[1],
               strerror(errno));
        return 1;
    }
    printf("heap: %s (fd=%d)\n", heap_name, heap_fd);
    ok("open(dma_heap)");

    /* 2. Round trips across a few sizes, including a non-page-multiple. */
    static const size_t sizes[] = { 4096, 64 * 1024, 1024 * 1024, 1000 };
    for (unsigned i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
        if (roundtrip(heap_fd, sizes[i], (uint8_t)(0x10 * (i + 1)), "roundtrip") == 0)
            ok("alloc/mmap/write/sync/read/close");
    }

    /* 3. Eight buffers alive at once, each holding its own pattern — a heap
     *    that aliased storage would fail here and nowhere else. */
    {
        enum { N = 8 };
        const size_t len = 16 * 1024;
        int fds[N];
        uint8_t *maps[N];
        int live = 0;

        for (int i = 0; i < N; i++) {
            struct dma_heap_allocation_data a;
            memset(&a, 0, sizeof(a));
            a.len = len;
            a.fd_flags = O_RDWR;
            if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0)
                break;
            fds[i] = (int)a.fd;
            maps[i] = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fds[i], 0);
            if (maps[i] == MAP_FAILED)
                break;
            for (size_t j = 0; j < len; j++)
                maps[i][j] = pattern((uint8_t)i, j);
            live++;
        }

        if (live != N) {
            fail("8 buffers live at once", "allocation or mapping stopped early");
        } else {
            int clean = 1;
            for (int i = 0; i < N && clean; i++)
                for (size_t j = 0; j < len; j++)
                    if (maps[i][j] != pattern((uint8_t)i, j)) {
                        clean = 0;
                        break;
                    }
            if (clean)
                ok("8 concurrent buffers stay independent");
            else
                fail("8 concurrent buffers stay independent", "one overwrote another");
        }
        for (int i = 0; i < live; i++) {
            munmap(maps[i], len);
            close(fds[i]);
        }
    }

    /* 4. Repeat until a leak would run out of descriptors. The fd table is
     *    bounded well below this count, so a missing decrement on close cannot
     *    hide. */
    {
        enum { ROUNDS = 300 };
        int stopped = -1;
        for (int i = 0; i < ROUNDS; i++) {
            struct dma_heap_allocation_data a;
            memset(&a, 0, sizeof(a));
            a.len = 8192;
            a.fd_flags = O_RDWR;
            if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
                stopped = i;
                break;
            }
            close((int)a.fd);
        }
        if (stopped < 0)
            ok("300 alloc/close rounds without exhausting");
        else
            printf("  [FAIL] 300 alloc/close rounds — stopped at %d: %s\n", stopped,
                   strerror(errno)), g_fail++;
    }

    /* 5. A mapping longer than the buffer must be refused, not silently
     *    rounded up into someone else's memory. */
    {
        struct dma_heap_allocation_data a;
        memset(&a, 0, sizeof(a));
        a.len = 4096;
        a.fd_flags = O_RDWR;
        if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
            fail("oversized mmap is refused", "alloc failed");
        } else {
            void *p = mmap(NULL, 1u << 20, PROT_READ | PROT_WRITE, MAP_SHARED,
                           (int)a.fd, 0);
            if (p == MAP_FAILED)
                ok("oversized mmap is refused");
            else {
                fail("oversized mmap is refused", "it succeeded");
                munmap(p, 1u << 20);
            }
            close((int)a.fd);
        }
    }

    /* 6. A child forked while the buffer is mapped must see the same pages —
     *    and its exit must not free them out from under the parent. */
    {
        struct dma_heap_allocation_data a;
        memset(&a, 0, sizeof(a));
        a.len = 4096;
        a.fd_flags = O_RDWR;
        if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
            fail("fork shares the buffer", "alloc failed");
        } else {
            const size_t len = 4096;
            int bfd = (int)a.fd;
            uint8_t *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, bfd, 0);
            if (p == MAP_FAILED) {
                fail("fork shares the buffer", "mmap failed");
                close(bfd);
            } else {
                for (size_t i = 0; i < len; i++)
                    p[i] = pattern(0x5a, i);

                pid_t pid = fork();
                if (pid < 0) {
                    fail("fork shares the buffer", "fork failed");
                } else if (pid == 0) {
                    /* Child only reads, then exits immediately — forking a VMA
                     * of a device buffer must not end its life. */
                    for (size_t i = 0; i < len; i++)
                        if (p[i] != pattern(0x5a, i))
                            _exit(2);
                    _exit(0);
                } else {
                    int status = 0;
                    waitpid(pid, &status, 0);
                    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
                        fail("fork shares the buffer", "child did not observe the data");
                    } else {
                        int intact = 1;
                        for (size_t i = 0; i < len; i++)
                            if (p[i] != pattern(0x5a, i)) {
                                intact = 0;
                                break;
                            }
                        if (intact)
                            ok("fork shares the buffer, parent's stays valid");
                        else
                            fail("fork shares the buffer",
                                 "child exit corrupted the parent's mapping");
                    }
                }
                munmap(p, len);
                close(bfd);
            }
        }
    }

    /* 7. A sub-page request is rounded up to a page before it becomes the
     *    buffer's size, so lseek, mmap and SYNC all name the same range. The
     *    whole allocation must read as zero — including the page tail — and
     *    must stay zero across a CPU invalidate, which would otherwise discard
     *    zeros that never reached memory. */
    {
        struct dma_heap_allocation_data a;
        memset(&a, 0, sizeof(a));
        a.len = 1000;
        a.fd_flags = O_RDWR;
        if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
            fail("sub-page request rounds to a page", "alloc failed");
        } else {
            int bfd = (int)a.fd;
            off_t visible = lseek(bfd, 0, SEEK_END);
            if (visible != 4096) {
                fail("sub-page request rounds to a page",
                     "lseek did not report one page");
            } else {
                uint8_t *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED,
                                  bfd, 0);
                if (p == MAP_FAILED) {
                    fail("sub-page request rounds to a page",
                         "the rounded range did not map");
                } else {
                    int zeroed = 1;
                    for (size_t i = 0; i < 4096; i++)
                        if (p[i] != 0) {
                            zeroed = 0;
                            break;
                        }
                    if (zeroed)
                        ok("fresh allocation and page tail are zeroed");
                    else
                        fail("fresh allocation and page tail are zeroed",
                             "stale bytes visible");

                    struct dma_buf_sync s;
                    s.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ;
                    if (ioctl(bfd, DMA_BUF_IOCTL_SYNC, &s) < 0) {
                        fail("invalidated allocation stays zeroed", "SYNC failed");
                    } else {
                        zeroed = 1;
                        for (size_t i = 0; i < 4096; i++)
                            if (p[i] != 0) {
                                zeroed = 0;
                                break;
                            }
                        if (zeroed)
                            ok("invalidated allocation stays zeroed");
                        else
                            fail("invalidated allocation stays zeroed",
                                 "invalidate revived stale bytes");
                    }
                    munmap(p, 4096);
                    ok("sub-page request rounds to a page");
                }
            }
            close(bfd);
        }
    }

    /* 8. fd_flags is the descriptor's access mode: a buffer requested
     *    O_RDONLY may be read-mapped but must refuse a shared writable
     *    mapping. */
    {
        struct dma_heap_allocation_data a;
        memset(&a, 0, sizeof(a));
        a.len = 4096;
        a.fd_flags = O_RDONLY;
        if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
            fail("O_RDONLY buffer refuses a writable mapping", "alloc failed");
        } else {
            int bfd = (int)a.fd;
            void *ro = mmap(NULL, 4096, PROT_READ, MAP_SHARED, bfd, 0);
            void *rw = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED,
                            bfd, 0);
            if (ro == MAP_FAILED) {
                fail("O_RDONLY buffer maps read-only", "read-only mapping failed");
            } else {
                ok("O_RDONLY buffer maps read-only");
                munmap(ro, 4096);
            }
            if (rw == MAP_FAILED)
                ok("O_RDONLY buffer refuses a writable mapping");
            else {
                fail("O_RDONLY buffer refuses a writable mapping", "it succeeded");
                munmap(rw, 4096);
            }
            close(bfd);
        }
    }

    /* 9. O_WRONLY is a valid fd_flags value and must still yield a mappable
     *    descriptor: mmap always needs file READ access, so the descriptor
     *    keeps READ even though its access mode is write-only. */
    {
        struct dma_heap_allocation_data a;
        memset(&a, 0, sizeof(a));
        a.len = 4096;
        a.fd_flags = O_WRONLY;
        if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &a) < 0) {
            fail("O_WRONLY buffer is mappable", "alloc failed");
        } else {
            int bfd = (int)a.fd;
            void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED,
                           bfd, 0);
            if (p == MAP_FAILED) {
                printf("  [FAIL] O_WRONLY buffer is mappable: %s\n",
                       strerror(errno));
                g_fail++;
            } else {
                ok("O_WRONLY buffer is mappable");
                munmap(p, 4096);
            }
            close(bfd);
        }
    }

    close(heap_fd);
    printf("=== result: %d passed, %d failed ===\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
