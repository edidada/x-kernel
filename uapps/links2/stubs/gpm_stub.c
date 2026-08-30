/* SPDX-License-Identifier: Apache-2.0 */
/* GPM stub: Links framebuffer configure requires libgpm; runtime uses mice. */

#include "gpm.h"

int Gpm_Open(Gpm_Connect *conn, int flag)
{
    (void)conn;
    (void)flag;
    return -1;
}

int Gpm_Close(void)
{
    return 0;
}

int Gpm_GetEvent(Gpm_Event *event)
{
    (void)event;
    return -1;
}
