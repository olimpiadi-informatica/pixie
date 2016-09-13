#ifndef PIXIE_HTTP_SERVER_H
#define PIXIE_HTTP_SERVER_H

#include <config_file.h>

class SendBuffer {
    std::vector<uint8_t> data;
    size_t position;

  public:
    SendBuffer(std::string bytes) {
        position = bytes.size();
        for (auto chr : bytes) data.push_back(chr);
    }

    int send(int fd) {
        ssize_t amount =
            write(fd, data.data() + position, data.size() - position);
        if (amount == -1) {
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
        if (amount == -1) {
            perror("read");
            return -1;
        } else {
            for (int i = position; i < position + amount; i++)
                if (data[i] == '\n') return true;
            return false;
        }
    }
};

class HttpServer {
    const std::vector<DownloadConfig>& configs;
    uint16_t port;

  public:
    HttpServer(const std::vector<DownloadConfig>& configs) : configs(configs) {
        char* port_ = getenv("PIXIE_HTTP_PORT");
        if (port_ == nullptr)
            port = DEFAULT_HTTP_PORT;
        else
            port = stoi(std::string(port_));
        port = htons(port);
    }
};

#endif
