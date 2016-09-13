#ifndef PIXIE_HASH_H
#define PIXIE_HASH_H
#include <common.h>
#include <cstdint>
#include <cstdlib>

class SHA224 {
    uint32_t hash[8];
    uint8_t buff[64];
    size_t buff_used;
    size_t total_len;
    void process(const uint8_t* begin, const uint8_t* end);

  public:
    SHA224();
    // Hashes bytes between begin and end
    void update(const uint8_t* begin, const uint8_t* end);
    // Gets the hash value, making the object no longer updateable.
    sha224_t get();
};

#endif
