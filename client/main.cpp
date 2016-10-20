#include <arpa/inet.h>
#include <chunk_rebuilder.h>
#include <chunks_info.h>
#include <common.h>
#include <communication.h>
#include <file.h>
#include <cstring>
#include <iostream>
#include <thread>

using namespace std::literals;

int main(int argc, char** argv) {
    if (argc != 3) {
        std::cerr << "Usage: " << argv[0] << " server_ip image_hash"
                  << std::endl;
        return 1;
    }
    sha224_t hash(argv[2]);
    int listen_sock = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (listen_sock == -1)
        throw std::runtime_error("socket: "s + strerror(errno));
    sockaddr_in client_info;
    memset(&client_info, 0, sizeof(sockaddr_in));
    client_info.sin_family = AF_INET;
    client_info.sin_port = htons(PIXIE_CLIENT_PORT);
    inet_aton("0.0.0.0", &client_info.sin_addr);
    if (bind(listen_sock, (struct sockaddr*)&client_info,
             sizeof(client_info)) == -1)
        throw std::runtime_error("bind: "s + strerror(errno));

    int answer_sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (answer_sock == -1)
        throw std::runtime_error("socket: "s + strerror(errno));
    struct sockaddr_in server_addr;
    memset(&server_addr, 0, sizeof(sockaddr_in));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PIXIE_SERVER_PORT);
    server_addr.sin_addr.s_addr = inet_addr(argv[1]);
    if (connect(answer_sock, (struct sockaddr*)&server_addr,
                sizeof(server_addr)) == -1)
        throw std::runtime_error("connect: "s + strerror(errno));

    uint8_t send_buffer[buff_size];
    uint8_t recv_buffer[buff_size];
    ChunkListRequest request;
    request.hash = hash;
    uint32_t request_size = request.fill_buffer(send_buffer);
    ssize_t received_size = 0;
    time_t last_request = time(NULL);
    if (send(answer_sock, send_buffer, request_size, 0) == -1)
        throw std::runtime_error("send: "s + strerror(errno));
    ChunkListInfo answer;
    while (true) {
        if (last_request + CLIENT_TIMEOUT < time(NULL)) {
            last_request = time(NULL);
            std::cerr << "Sent" << std::endl;
            if (send(answer_sock, send_buffer, request_size, 0) == -1)
                throw std::runtime_error("send: "s + strerror(errno));
        }
        received_size = recv(listen_sock, recv_buffer, buff_size, 0);
        if (received_size < 1 && errno == EAGAIN) {
            std::this_thread::sleep_for(1ms);
        } else if (received_size == -1) {
            throw std::runtime_error("recv: "s + strerror(errno));
        } else if (received_size > 0) {
            answer.read_from_buffer(recv_buffer, received_size);
            break;
        }
    }
    Chunk list_chunk;
    list_chunk.hash = answer.hash;
    list_chunk.offset = 0;
    list_chunk.size = answer.length;

    ChunkRebuilder rebuilder(listen_sock, answer_sock);
    std::thread rebuilder_thread([&]() { rebuilder(); });

    rebuilder.set_interesting(list_chunk);
    while (true) {
        std::this_thread::sleep_for(1ms);
        if (rebuilder.count() == 0) break;
    }

    auto chunklist = rebuilder.get_complete_chunk().second;
    ChunksInfo chunk_list(chunklist.second.data(), chunklist.second.size());
}
