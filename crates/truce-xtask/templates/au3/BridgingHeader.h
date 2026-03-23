// Bridging header: exposes C types from the Rust AU framework to Swift.
#import "au_shim_types.h"

// Swift can't use C macros, so provide inline helpers.
static inline uint32_t au_fourcc(const uint8_t b[4]) {
    return ((uint32_t)b[0] << 24) | ((uint32_t)b[1] << 16) |
           ((uint32_t)b[2] << 8)  | (uint32_t)b[3];
}
