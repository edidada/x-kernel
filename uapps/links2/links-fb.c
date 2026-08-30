/* SPDX-License-Identifier: Apache-2.0 */
/* Guest wrapper: CJK faces + SSL certs. Avoids busybox shell I/O bugs. */

#define _GNU_SOURCE

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define LINKS_BIN "/usr/local/bin/links"
#define FONT_PATH "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"

int main(int argc, char **argv)
{
    static char *extra[] = {
        "links",
        "-font",
        FONT_PATH,
        "-font-bold",
        FONT_PATH,
        "-font-monospaced",
        FONT_PATH,
        "-font-monospaced-bold",
        FONT_PATH,
        "-font-italic",
        FONT_PATH,
        "-font-italic-bold",
        FONT_PATH,
        "-font-monospaced-italic",
        FONT_PATH,
        "-font-monospaced-italic-bold",
        FONT_PATH,
        "-ssl.builtin-certificates",
        "1",
        "-html-assume-codepage",
        "utf-8",
        /* QEMU user net has no working IPv6 path; AAAA-first lookups EPIPE. */
        "-address-preference",
        "3",
        /* Skip image CDNs; extra connections often RST and show Broken pipe. */
        "-html-display-images",
        "0",
    };
    const int nextra = (int)(sizeof(extra) / sizeof(extra[0]));
    char **nargv;
    int i;

    if (setenv("FONTCONFIG_FILE", "/etc/fonts/fonts.conf", 1) != 0 ||
        setenv("FONTCONFIG_PATH", "/etc/fonts", 1) != 0) {
        perror("setenv");
        return 1;
    }

    nargv = calloc((size_t)(nextra + argc), sizeof(*nargv));
    if (!nargv) {
        perror("calloc");
        return 1;
    }
    for (i = 0; i < nextra; i++) {
        nargv[i] = extra[i];
    }
    for (i = 1; i < argc; i++) {
        nargv[nextra + i - 1] = argv[i];
    }

    execv(LINKS_BIN, nargv);
    perror("execv " LINKS_BIN);
    return 127;
}
