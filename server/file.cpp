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

File::File(const std::string& path, chunk_size_t chunk_size,
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
        chunk_off_t next_hole = lseek(fd, 0, SEEK_HOLE);
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
        current_position = lseek(fd, 1, SEEK_DATA);
        if (current_position == (chunk_off_t)-1)
            throw std::runtime_error("lseek: "s + strerror(errno));
        assert(current_position != file_size);
    }
}

std::vector<uint8_t> File::read_chunk(const Chunk& chunk) {
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
