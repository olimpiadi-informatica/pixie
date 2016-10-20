#include <chunk_sender.h>
#include <communication.h>
#include <iostream>
#include <thread>
using namespace std::literals;

ChunkSender::ChunkSender(
    const std::map<sha224_t, std::vector<uint8_t>>& cl,
    const std::map<sha224_t, sha224_t>& chunk_lists_hashes,
    const std::map<sha224_t, std::pair<Chunk, const InFile*>>& file_chunks)
    : file_chunks(file_chunks) {
    for (const auto& chunk : cl)
        chunk_lists[chunk_lists_hashes.at(chunk.first)] = chunk.second;
    sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock == -1) throw std::runtime_error("socket: "s + strerror(errno));
    int broadcastEnable = 1;
    if (setsockopt(sock, SOL_SOCKET, SO_BROADCAST, &broadcastEnable,
                   sizeof(broadcastEnable)) == -1)
        throw std::runtime_error("setsockopt: "s + strerror(errno));
}

void ChunkSender::operator()() {
    struct sockaddr_in s;
    memset(&s, 0, sizeof(s));
    s.sin_family = AF_INET;
    s.sin_port = htons(PIXIE_CLIENT_PORT);
    uint8_t buffer[sizeof(DataPacket)];
    while (true) {
        message_t mess;
        {
            std::lock_guard<std::mutex> queue_lock(queue_mutex);
            if (queue.empty()) {
                // TODO: use condition variables
                std::this_thread::sleep_for(1ms);
                continue;
            }
            mess = queue.front();
            queue.pop();
            enqueued.erase(mess);
        }
        sha224_t hash = std::get<0>(mess);
        std::vector<uint8_t> data;
        if (chunk_lists.count(hash)) {
            data = chunk_lists.at(hash);
        } else if (file_chunks.count(hash)) {
            auto info = file_chunks.at(hash);
            data = info.second->read_chunk(info.first);
        } else {
            std::cerr << "Unknown chunk requested" << std::endl;
            continue;
        }
        s.sin_addr.s_addr = std::get<3>(mess);
        uint32_t start = std::get<1>(mess);
        uint32_t length = std::get<2>(mess);
        DataPacket packet;
        packet.chunk = hash;
        while (length > 0) {
            packet.offset = start;
            packet.data_length = std::min(length, maximum_data_size);
            memcpy(packet.data, data.data() + start, packet.data_length);
            start += packet.data_length;
            length -= packet.data_length;
            uint32_t message_size = packet.fill_buffer(buffer);
            if (sendto(sock, buffer, message_size, 0, (struct sockaddr*)&s,
                       sizeof(struct sockaddr_in)) < 0)
                perror("sendto");
        }
    }
}
