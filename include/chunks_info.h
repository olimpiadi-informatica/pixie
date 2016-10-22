#ifndef PIXIE_CHUNKS_INFO_H
#define PIXIE_CHUNKS_INFO_H
#include <common.h>
#include <file.h>
#include <map>
#include <unordered_map>

class ChunksInfo {
    std::map<std::string, OutFile> files;
    std::map<sha224_t, std::vector<std::pair<Chunk, OutFile*>>> chunk_map;
    std::vector<Chunk> chunks_needed;

  public:
    ChunksInfo(const uint8_t* data, uint32_t size);
    void write_chunk(sha224_t hash, uint8_t* data) {
        for (auto ch : chunk_map[hash]) ch.second->write_chunk(ch.first, data);
    }
    const std::vector<Chunk>& get_chunks_needed() const {
        return chunks_needed;
    }
};

#endif
