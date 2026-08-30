/* SPDX-License-Identifier: Apache-2.0 */
/* Tiny libuuid for static fontconfig. Cache file names only need uniqueness. */

#define _GNU_SOURCE
#define _DEFAULT_SOURCE

#include "uuid.h"

#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <unistd.h>

void uuid_generate_random(uuid_t out)
{
    if (getentropy(out, 16) != 0) {
        memset(out, 0, 16);
    }
    out[6] = (unsigned char)((out[6] & 0x0f) | 0x40);
    out[8] = (unsigned char)((out[8] & 0x3f) | 0x80);
}

void uuid_unparse(const uuid_t uu, char *out)
{
    sprintf(out,
            "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x",
            uu[0], uu[1], uu[2], uu[3], uu[4], uu[5], uu[6], uu[7], uu[8], uu[9],
            uu[10], uu[11], uu[12], uu[13], uu[14], uu[15]);
}

int uuid_parse(const char *in, uuid_t uu)
{
    unsigned int b[16];
    int n;

    n = sscanf(in,
               "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x",
               &b[0], &b[1], &b[2], &b[3], &b[4], &b[5], &b[6], &b[7], &b[8],
               &b[9], &b[10], &b[11], &b[12], &b[13], &b[14], &b[15]);
    if (n != 16) {
        return -1;
    }
    for (n = 0; n < 16; n++) {
        uu[n] = (unsigned char)b[n];
    }
    return 0;
}

void uuid_copy(uuid_t dst, const uuid_t src)
{
    memcpy(dst, src, 16);
}

int uuid_compare(const uuid_t a, const uuid_t b)
{
    return memcmp(a, b, 16);
}
