#include <bchooser.h>
#include <chunk_sender.h>
#include <communication.h>
#include <config_file.h>
#include <http_server.h>
#include <sys/socket.h>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <thread>

using namespace std::literals;

static const uint32_t buff_size = 200;

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s config_file [config_file [...]]\n", argv[0]);
        return EXIT_FAILURE;
    }
    std::vector<std::string> config_files;
    for (int i = 1; i < argc; i++) config_files.emplace_back(argv[i]);
    auto configs = parse_config(config_files);
    HttpServer http_server(configs);
    std::thread http_thread([&]() { http_server(); });

    std::map<sha224_t, std::vector<uint8_t>> chunk_lists;
    std::map<sha224_t, sha224_t> chunk_lists_hashes;
    std::map<sha224_t, std::pair<Chunk, const InFile*>> file_chunks;
    for (const auto& config : configs) {
        chunk_lists.emplace(config.get_config_hash(), config.get_chunk_list());
        for (const auto& file : config.get_file_data())
            for (const auto& chunk : file.second.get_chunks())
                file_chunks.emplace(chunk.hash,
                                    std::make_pair(chunk, &file.second));
    }

    for (const auto& chunk_list : chunk_lists) {
        SHA224 hasher;
        hasher.update(chunk_list.second.data(),
                      chunk_list.second.data() + chunk_list.second.size());
        chunk_lists_hashes[chunk_list.first] = hasher.get();
    }

    BroadcastChooser broadcast_chooser;
    ChunkSender chunk_sender(chunk_lists, file_chunks);
    std::thread sender_thread([&]() { chunk_sender(); });

    int listen_sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (listen_sock == -1)
        throw std::runtime_error("socket: "s + strerror(errno));
    struct sockaddr_in listen_addr;
    memset(&listen_addr, 0, sizeof(struct sockaddr_in));
    listen_addr.sin_port = htons(PIXIE_SERVER_PORT);
    if (bind(listen_sock, (struct sockaddr*)&listen_addr,
             sizeof(struct sockaddr_in)) == -1)
        throw std::runtime_error("bind: "s + strerror(errno));

    int answer_sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (answer_sock == -1)
        throw std::runtime_error("socket: "s + strerror(errno));
    struct sockaddr_in answer_addr;
    memset(&answer_addr, 0, sizeof(struct sockaddr_in));
    answer_addr.sin_port = htons(PIXIE_CLIENT_PORT);

    struct sockaddr_in client_addr;
    memset(&client_addr, 0, sizeof(struct sockaddr_in));

    uint8_t recv_buffer[buff_size];
    uint8_t send_buffer[buff_size];

    while (true) {
        socklen_t client_addr_len;
        ssize_t recv_size =
            recvfrom(listen_sock, (void*)recv_buffer, buff_size, 0,
                     (struct sockaddr*)&client_addr, &client_addr_len);
        if (recv_size == -1 && errno == EINTR) continue;
        if (recv_size == -1)
            throw std::runtime_error("recvfrom: "s + strerror(errno));
        if (recv_size == buff_size)
            std::cerr << "Received a message too long" << std::endl;
        switch (extract_message_type(recv_buffer)) {
            case chunk_list_request: {
                if (recv_size != sizeof(ChunkListRequest)) {
                    std::cerr << "Unknown message received" << std::endl;
                    continue;
                }
                ChunkListRequest request;
                request.read_from_buffer(recv_buffer, recv_size);
                if (!chunk_lists.count(request.hash)) {
                    std::cerr << "Request for unknown chunk list received"
                              << std::endl;
                    continue;
                }
                ChunkListInfo info;
                info.length = chunk_lists[request.hash].size();
                info.hash = chunk_lists_hashes[request.hash];
                uint32_t answer_size = info.fill_buffer(send_buffer);
                answer_addr.sin_addr.s_addr = client_addr.sin_addr.s_addr;
                if (sendto(answer_sock, send_buffer, answer_size, 0,
                           (struct sockaddr*)&client_addr,
                           sizeof(struct sockaddr_in)) < 0)
                    perror("sendto");
                break;
            }
            case data_request: {
                if (recv_size != sizeof(DataRequest)) {
                    std::cerr << "Unknown message received" << std::endl;
                    continue;
                }
                DataRequest request;
                request.read_from_buffer(recv_buffer, recv_size);
                chunk_sender.enqueue(request.chunk, request.start,
                                     request.length,
                                     broadcast_chooser.get_bc_address(
                                         client_addr.sin_addr.s_addr));
                break;
            }
            default:
                std::cerr << "Unknown message received" << std::endl;
                continue;
        }
    }

    sender_thread.join();
    http_thread.join();
}
