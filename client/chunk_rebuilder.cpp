#include <chunk_rebuilder.h>
#include <chrono>
#include <iostream>
#include <thread>

using namespace std::literals;

void ChunkRebuilder::operator()() {
    uint8_t data_buffer[sizeof(DataPacket)];
    time_t last_packet_time = time(NULL);
    while (!quit) {
        time_t current_time = time(NULL);
        if (last_packet_time + CLIENT_TIMEOUT < current_time) {
            last_packet_time = current_time;
            size_t chunk_counter = 0;
            for (const auto& missing_chunk : interesting_chunks) {
                if (chunk_counter++ > 50) break;
                const auto& hash = missing_chunk.first;
                if (chunk_missing_data.count(hash)) {
                    for (unsigned i = 0; i < interesting_chunks[hash]; i++) {
                        if (!chunk_missing_data[hash].second[i]) {
                            continue;
                        }
                        unsigned move_to = i + 1;
                        while (move_to < interesting_chunks[hash] &&
                               chunk_missing_data[hash].second[move_to])
                            move_to++;

                        /*std::cerr << "Asking for hash " << hash.to_string()
                                  << ": [" << i << "; " << move_to << ")"
                                  << std::endl;*/
                        send_chunk_request(hash, i, move_to - i);
                        i = move_to - 1;
                    }
                } else {
                    /*std::cerr << "Asking for hash " << hash.to_string() << ":
                       ["
                              << 0 << "; " << interesting_chunks[hash] << ")"
                              << std::endl;*/
                    send_chunk_request(hash, 0, interesting_chunks[hash]);
                }
            }
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
        last_packet_time = current_time;
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
                std::lock_guard<std::mutex> queue_lock(queue_mutex);
                interesting_chunks.erase(hash);
                complete_chunks.emplace(hash, std::move(ch_data));
            }
        }
    }
}
