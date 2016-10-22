#include <file.h>

using namespace std::string_literals;

Chunk::Chunk(int fd, chunk_off_t start, chunk_off_t end,
             SHA224& global_hasher) {
    std::vector<uint8_t> data(end - start);
    for (ssize_t to_read = data.size(); to_read > 0;) {
        ssize_t off = data.size() - to_read;
        ssize_t bytes_read = pread(fd, data.data() + off, to_read, start + off);
        if (bytes_read == -1)
            throw std::runtime_error("pread: "s + strerror(errno));
        assert(bytes_read != 0);
        to_read -= bytes_read;
    }
    global_hasher.update(data.data(), data.data() + data.size());
    SHA224 sha224;
    sha224.update(data.data(), data.data() + data.size());
    hash = sha224.get();
    offset = start;
    size = end - start;
}

InFile::InFile(const std::string& path, chunk_size_t chunk_size,
               SHA224& global_hasher) {
    using namespace std::string_literals;
    fd = open(path.c_str(), O_RDONLY);
    if (fd == -1) throw std::runtime_error("open: "s + strerror(errno));
    chunk_off_t file_size = lseek(fd, 0, SEEK_END);
    if (file_size == (chunk_off_t)-1)
        throw std::runtime_error("lseek: "s + strerror(errno));
    if (lseek(fd, 0, SEEK_SET) == (off_t)-1)
        throw std::runtime_error("lseek: "s + strerror(errno));
    chunk_off_t current_position = 0;
    while (current_position < file_size) {
        chunk_off_t next_hole = lseek(fd, current_position, SEEK_HOLE);
        if (next_hole == (chunk_off_t)-1)
            throw std::runtime_error("lseek: "s + strerror(errno));
        assert(next_hole <= file_size);
        for (; current_position < next_hole; current_position += chunk_size) {
            auto chunk_end = std::min(next_hole, current_position + chunk_size);
            chunks.emplace_back(fd, current_position, chunk_end, global_hasher);
        }
        if (next_hole == file_size) continue;
        if (lseek(fd, next_hole, SEEK_SET) == (off_t)-1)
            throw std::runtime_error("lseek: "s + strerror(errno));
        current_position = lseek(fd, current_position, SEEK_DATA);
        if (current_position == (chunk_off_t)-1 && errno == ENXIO) break;
        if (current_position == (chunk_off_t)-1)
            throw std::runtime_error("lseek: "s + strerror(errno));
        assert(current_position != file_size);
    }
}

std::vector<uint8_t> InFile::read_chunk(const Chunk& chunk) const {
    std::vector<uint8_t> data(chunk.size);
    for (ssize_t to_read = data.size(); to_read > 0;) {
        ssize_t off = data.size() - to_read;
        ssize_t bytes_read =
            pread(fd, data.data() + off, to_read, chunk.offset + off);
        if (bytes_read == -1)
            throw std::runtime_error("pread: "s + strerror(errno));
        assert(bytes_read != 0);
        to_read -= bytes_read;
    }
    return data;
}

bool OutFile::must_download(const Chunk& chunk) const {
    std::vector<uint8_t> data(chunk.size);
    for (ssize_t to_read = data.size(); to_read > 0;) {
        ssize_t off = data.size() - to_read;
        ssize_t bytes_read =
            pread(fd, data.data() + off, to_read, chunk.offset + off);
        if (bytes_read == -1)
            throw std::runtime_error("pread: "s + strerror(errno));
        if (bytes_read == 0) return true;
        to_read -= bytes_read;
    }
    SHA224 hasher;
    hasher.update(data.data(), data.data() + data.size());
    return hasher.get() != chunk.hash;
}

std::vector<Chunk> OutFile::get_missing_chunks() const {
    std::vector<Chunk> ans;
    for (const auto& chunk : chunks)
        if (must_download(chunk)) ans.push_back(chunk);
    return ans;
}

void OutFile::write_chunk(const Chunk& chunk, uint8_t* data) {
    for (ssize_t to_write = chunk.size; to_write > 0;) {
        ssize_t off = chunk.size - to_write;
        ssize_t bytes_written =
            pwrite(fd, data + off, to_write, chunk.offset + off);
        if (bytes_written == -1)
            throw std::runtime_error("pwrite: "s + strerror(errno));
        assert(bytes_written != 0);
        to_write -= bytes_written;
    }
}
