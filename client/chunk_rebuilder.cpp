#include <chunk_rebuilder.h>
#include <chrono>
#include <iostream>
#include <thread>

using namespace std::literals;

void ChunkRebuilder::operator()() {
    uint8_t data_buffer[sizeof(DataPacket)];
    while (!quit) {
        time_t current_time = time(NULL);
        while (!expiring_packets.empty() &&
               expiring_packets.begin()->first + CLIENT_TIMEOUT <
                   current_time) {
            sha224_t hash = expiring_packets.begin()->second;
            expiring_packets.erase(expiring_packets.begin());
            last_data.erase(hash);
            send_chunk_request(hash, 0, interesting_chunks[hash]);
        }
        ssize_t received_data = recvfrom(
            listen_sock, data_buffer, sizeof(DataPacket), 0, nullptr, nullptr);
        if (received_data == -1) {
            if (errno == EAGAIN) {
                std::this_thread::sleep_for(1ms);
            } else if (errno != EINTR) {
                perror("recvfrom");
            }
            continue;
        }
        if (extract_message_type(data_buffer) != data_packet ||
            received_data < DataPacket::min_packet_size) {
            std::cerr << "Unknown packet received" << std::endl;
            continue;
        }
        DataPacket data;
        data.read_from_buffer(data_buffer, received_data);
        sha224_t hash = data.chunk;
        if (!interesting_chunks.count(hash)) continue;
        if (!chunk_data.count(hash)) {
            chunk_data.emplace(hash, interesting_chunks[hash]);
            chunk_missing_data[hash] = {interesting_chunks[hash], {}};
            chunk_missing_data[hash].second.resize(interesting_chunks[hash], 1);
        }
        expiring_packets.erase({last_data[hash], hash});
        last_data[hash] = time(NULL);
        expiring_packets.emplace(last_data[hash], hash);
        auto& md = chunk_missing_data[hash];
        auto& dt = chunk_data[hash];
        for (uint32_t pos = 0; pos < data.data_length; pos++) {
            if (md.second[data.offset + pos]) {
                md.first--;
            } else if (dt[data.offset + pos] != data.data[pos]) {
                std::cerr << "Received conflicting data!" << std::endl;
            }
            md.second[data.offset + pos] = 0;
            dt[data.offset + pos] = data.data[pos];
        }
        if (md.first == 0) {
            std::vector<uint8_t> ch_data = std::move(chunk_data[hash]);
            chunk_data.erase(hash);
            chunk_missing_data.erase(hash);
            SHA224 checker;
            checker.update(ch_data.data(), ch_data.data() + ch_data.size());
            sha224_t real_hash = checker.get();
            if (real_hash != hash) {
                std::cerr << "Wanted " << hash.to_string() << ", received "
                          << real_hash.to_string() << std::endl;
            } else {
                expiring_packets.erase({last_data[hash], hash});
                last_data.erase(hash);
                {
                    std::lock_guard<std::mutex> queue_lock(queue_mutex);
                    interesting_chunks.erase(hash);
                    complete_chunks.emplace(hash, std::move(ch_data));
                }
            }
        }
    }
}
