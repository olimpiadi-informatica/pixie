#ifndef PIXIE_FILE_H
#define PIXIE_FILE_H
#include <fcntl.h>
#include <hash.h>
#include <sys/types.h>
#include <unistd.h>
#include <cassert>
#include <cerrno>
#include <cstring>
#include <vector>

struct Chunk {
    sha224_t hash;
    chunk_off_t offset;
    chunk_size_t size;
    Chunk(int fd, chunk_off_t start, chunk_off_t end, SHA224& global_hasher);
};

class File {
    std::vector<Chunk> chunks;
    int fd;

  public:
    File(const std::string& path, chunk_size_t chunk_size,
         SHA224& global_hasher);
    File(const File& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
    }
    File(File&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
    }
    File& operator=(const File& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
        return *this;
    }
    File& operator=(File&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
        return *this;
    }
    ~File() {
        if (fd != -1) close(fd);
    }
    const std::vector<Chunk>& get_chunks() { return chunks; }
    std::vector<uint8_t> read_chunk(const Chunk& chunk);
};

#endif
