#ifndef PIXIE_CHUNK_REBUILDER_H
#define PIXIE_CHUNK_REBUILDER_H
#include <common.h>
#include <communication.h>
#include <file.h>
#include <atomic>
#include <ctime>
#include <map>
#include <mutex>
#include <queue>
#include <set>
#include <vector>

class ChunkRebuilder {
    int listen_sock, answer_sock;
    std::map<sha224_t, chunk_size_t> interesting_chunks;
    std::map<sha224_t, std::vector<uint8_t>> chunk_data;
    // TODO: improve this
    std::map<sha224_t, std::pair<uint32_t, std::vector<bool>>>
        chunk_missing_data;
    std::map<sha224_t, time_t> last_data;
    std::set<std::pair<time_t, sha224_t>> expiring_packets;
    std::queue<std::pair<sha224_t, std::vector<uint8_t>>> complete_chunks;
    std::mutex queue_mutex;
    std::mutex send_mutex;
    std::atomic<bool> quit;

    void send_chunk_request(sha224_t hash, uint32_t start, uint32_t length) {
        uint8_t send_buffer[sizeof(DataRequest)];
        DataRequest request;
        request.chunk = hash;
        request.start = start;
        request.length = length;
        uint32_t reqlen = request.fill_buffer(send_buffer);
        std::lock_guard<std::mutex> send_lock(send_mutex);
        if (send(answer_sock, send_buffer, reqlen, 0) == -1) perror("send");
    }

  public:
    ChunkRebuilder(int listen_sock, int answer_sock)
        : listen_sock(listen_sock), answer_sock(answer_sock), quit(false) {}

    void set_interesting(Chunk chunk) {
        interesting_chunks[chunk.hash] = chunk.size;
        send_chunk_request(chunk.hash, 0, chunk.size);
    }
    size_t count() {
        std::lock_guard<std::mutex> queue_lock(queue_mutex);
        return interesting_chunks.size();
    }
    void stop() { quit = true; }
    std::pair<bool, std::pair<sha224_t, std::vector<uint8_t>>>
    get_complete_chunk() {
        std::lock_guard<std::mutex> queue_lock(queue_mutex);
        if (complete_chunks.empty()) return {false, {{}, {}}};
        auto ans = complete_chunks.front();
        complete_chunks.pop();
        return {true, ans};
    }
    void operator()();
};

#endif
