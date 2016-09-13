#ifndef PIXIE_COMMON_H
#define PIXIE_COMMON_H
#include <array>

// This value can be overridden by the PIXIE_HTTP_PORT environment variable.
#define DEFAULT_HTTP_PORT 80

// This value can be overridden by the chunk_size property in a JSON config.
#define DEFAULT_CHUNK_SIZE (1 << 22)

class sha224_t : public std::array<uint8_t, 28> {};

typedef uint32_t chunk_size_t;
typedef uint64_t chunk_off_t;

#endif
