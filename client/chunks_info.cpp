#include <chunks_info.h>

ChunksInfo::ChunksInfo(const uint8_t* data, uint32_t size) {
    uint32_t pos = 0;
    while (pos < size) {
        std::string filename;
        while (data[pos] != 0) filename.push_back(data[pos++]);
        pos++;
        uint32_t chunk_count;
        memcpy(&chunk_count, data + pos, sizeof(uint32_t));
        chunk_count = ntohl(chunk_count);
        pos += sizeof(uint32_t);
        std::vector<Chunk> chunks;
        for (uint32_t i = 0; i < chunk_count; i++) {
            Chunk ch;
            ch.read_from_buffer(data + pos, sizeof(Chunk));
            pos += sizeof(Chunk);
        }
        assert(pos <= size);
        files.emplace(filename, OutFile(filename, chunks));
        OutFile* of = &files.at(filename);
        for (const auto& chunk : chunks)
            chunk_map[chunk.hash].emplace_back(chunk, of);
    }
}
