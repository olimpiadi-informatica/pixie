#ifndef PIXIE_COMMON_H
#define PIXIE_COMMON_H
#include <arpa/inet.h>
#include <array>

#define htonll(x)    \
    ((1 == htonl(1)) \
         ? (x)       \
         : ((uint64_t)htonl((x)&0xFFFFFFFF) << 32) | htonl((x) >> 32))
#define ntohll(x)    \
    ((1 == ntohl(1)) \
         ? (x)       \
         : ((uint64_t)ntohl((x)&0xFFFFFFFF) << 32) | ntohl((x) >> 32))

// This value can be overridden by the PIXIE_HTTP_PORT environment variable.
#define DEFAULT_HTTP_PORT 80

// This value can be overridden by the PIXIE_HTTP_ADDR environment variable.
#define DEFAULT_HTTP_ADDR "0.0.0.0"

// This value can be overridden by the chunk_size property in a JSON config.
#define DEFAULT_CHUNK_SIZE (1 << 22)

#define IMAGE_METHOD "tftp"
#define PIXIE_SERVER_PORT 7494
#define PIXIE_CLIENT_PORT 7495
#define CLIENT_TIMEOUT 5

static const uint32_t buff_size = 200;

class sha224_t : public std::array<uint8_t, 28> {
  public:
    std::string to_string() const;
    sha224_t(const std::string& text);
    sha224_t() {}
};

typedef uint32_t chunk_size_t;
typedef uint64_t chunk_off_t;

#endif
