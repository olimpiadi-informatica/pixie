#ifndef PIXIE_HTTP_SERVER_H
#define PIXIE_HTTP_SERVER_H

#include <config_file.h>
#include <sys/socket.h>

class SendBuffer {
    std::vector<uint8_t> data;
    size_t position;

  public:
    SendBuffer(std::string bytes) {
        position = 0;
        for (auto chr : bytes) data.push_back(chr);
    }

    int send(int fd) {
        ssize_t amount =
            write(fd, data.data() + position, data.size() - position);
        if (amount == -1 && errno != EINTR) {
            perror("write");
            return -1;
        } else {
            position += amount;
            return position != data.size();
        }
    }
};

class ReceiveBuffer {
    static const int read_chunk = 1024;
    std::vector<uint8_t> data;
    size_t position;

  public:
    int recv(int fd) {
        data.resize(position + read_chunk);
        ssize_t amount = read(fd, data.data() + position, read_chunk);
        if (amount == -1 && errno != EINTR) {
            perror("read");
            return -1;
        } else {
            for (unsigned i = position; i < position + amount; i++)
                if (data[i] == '\n') return 0;
            return 1;
        }
    }
    std::string get_data() { return std::string(data.begin(), data.end()); }
};

class HttpServer {
    static const int maxevents = 64;
    const std::vector<DownloadConfig>& configs;
    int sock;
    int epoll;
    int max_fd_no;

    std::string generate_script(std::string uri);

  public:
    HttpServer(const std::vector<DownloadConfig>& configs);
    HttpServer(HttpServer&) = delete;
    HttpServer(HttpServer&&) = delete;
    HttpServer& operator=(HttpServer&) = delete;
    HttpServer& operator=(HttpServer&&) = delete;
    void operator()();
};

#endif
