/* Declarations matching the GPM subset used by Links 2.30. */
#ifndef XKERNEL_GPM_STUB_H
#define XKERNEL_GPM_STUB_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Gpm_Connect {
    unsigned short eventMask;
    unsigned short defaultMask;
    unsigned short minMod;
    unsigned short maxMod;
} Gpm_Connect;

typedef struct Gpm_Event {
    unsigned char buttons;
    unsigned char modifiers;
    unsigned short vc;
    short dx;
    short dy;
    short x;
    short y;
    int type;
    int clicks;
    int margin;
    short wdx;
    short wdy;
} Gpm_Event;

#define GPM_MOVE 1
#define GPM_DRAG 2
#define GPM_DOWN 4
#define GPM_UP 8
#define GPM_B_LEFT 4
#define GPM_B_MIDDLE 2
#define GPM_B_RIGHT 1

int Gpm_Open(Gpm_Connect *conn, int flag);
int Gpm_Close(void);
int Gpm_GetEvent(Gpm_Event *event);

#ifdef __cplusplus
}
#endif

#endif
