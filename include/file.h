#ifndef PIXIE_FILE_H
#define PIXIE_FILE_H
#include <fcntl.h>
#include <hash.h>
#include <sys/types.h>
#include <unistd.h>
#include <cassert>
#include <cerrno>
#include <cstring>
#include <iostream>
#include <vector>

struct Chunk {
    sha224_t hash;
    chunk_off_t offset;
    chunk_size_t size;
    Chunk(){};
    Chunk(int fd, chunk_off_t start, chunk_off_t end, SHA224& global_hasher);

    uint32_t fill_buffer(uint8_t* buffer) const {
        Chunk temp{};
        temp.hash = hash;
        temp.offset = htonll(offset);
        temp.size = htonl(size);
        memcpy(buffer, &temp, sizeof(temp));
        return sizeof(temp);
    }

    void read_from_buffer(const uint8_t* buffer, uint32_t _size) {
        memcpy(this, buffer, _size);
        offset = ntohll(offset);
        size = ntohl(size);
    }
};

class InFile {
    std::vector<Chunk> chunks;
    int fd;

  public:
    InFile(const std::string& path, chunk_size_t chunk_size,
           SHA224& global_hasher);
    InFile(const InFile& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
    }
    InFile(InFile&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
    }
    InFile& operator=(const InFile& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
        return *this;
    }
    InFile& operator=(InFile&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
        return *this;
    }
    ~InFile() {
        if (fd != -1) close(fd);
    }
    const std::vector<Chunk>& get_chunks() const { return chunks; }
    std::vector<uint8_t> read_chunk(const Chunk& chunk) const;
};

class OutFile {
    std::vector<Chunk> chunks;
    int fd;
    bool must_download(const Chunk& chunk) const;

  public:
    OutFile(const std::string& path, std::vector<Chunk> chunks)
        : chunks(chunks) {
        fd = open(path.c_str(), O_RDWR | O_CREAT, 0600);
        if (fd == -1)
            throw std::runtime_error(std::string("open: ") + strerror(errno));
    }
    OutFile(const OutFile& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
    }
    OutFile(OutFile&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
    }
    OutFile& operator=(const OutFile& other) {
        fd = dup(other.fd);
        chunks = other.chunks;
        return *this;
    }
    OutFile& operator=(OutFile&& other) {
        fd = other.fd;
        other.fd = -1;
        chunks = std::move(other.chunks);
        return *this;
    }
    ~OutFile() {
        if (fd != -1) close(fd);
    }
    void write_chunk(const Chunk& chunk, uint8_t* data);
    std::vector<Chunk> get_missing_chunks() const;
};

#endif
