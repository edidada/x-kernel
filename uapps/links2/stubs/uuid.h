/* Minimal libuuid ABI used by fontconfig 2.13.1. */
#ifndef XKERNEL_UUID_STUB_H
#define XKERNEL_UUID_STUB_H

#ifdef __cplusplus
extern "C" {
#endif

typedef unsigned char uuid_t[16];

void uuid_generate_random(uuid_t out);
void uuid_unparse(const uuid_t uu, char *out);
int uuid_parse(const char *in, uuid_t uu);
void uuid_copy(uuid_t dst, const uuid_t src);
int uuid_compare(const uuid_t a, const uuid_t b);

#ifdef __cplusplus
}
#endif

#endif
