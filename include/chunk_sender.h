#ifndef PIXIE_CHUNK_SENDER_H
#define PIXIE_CHUNK_SENDER_H
#include <common.h>
#include <file.h>
#include <map>
#include <mutex>
#include <queue>
#include <set>
#include <tuple>
#include <vector>

class ChunkSender {
    typedef std::tuple<sha224_t, uint32_t, uint32_t, in_addr_t> message_t;
    const std::map<sha224_t, std::vector<uint8_t>>& chunk_lists;
    const std::map<sha224_t, std::pair<Chunk, const InFile*>>& file_chunks;
    std::set<message_t> enqueued;
    std::queue<message_t> queue;
    std::mutex queue_mutex;
    int sock;

  public:
    ChunkSender(
        const std::map<sha224_t, std::vector<uint8_t>>& chunk_lists,
        const std::map<sha224_t, std::pair<Chunk, const InFile*>>& file_chunks);

    void enqueue(sha224_t hash, uint32_t start, uint32_t length,
                 in_addr_t dest) {
        std::lock_guard<std::mutex> queue_lock(queue_mutex);
        if (enqueued.count({hash, start, length, dest})) return;
        enqueued.emplace(hash, start, length, dest);
        queue.emplace(hash, start, length, dest);
    }

    void operator()();
};

#endif
