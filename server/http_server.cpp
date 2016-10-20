#include <arpa/inet.h>
#include <http_server.h>
#include <sys/epoll.h>
#include <sys/resource.h>
#include <iostream>
#include <memory>
using namespace std::literals;

std::string HttpServer::generate_script(std::string uri) {
    std::string filename;
    std::string ip;
    size_t filename_start = 0;
    while (uri[filename_start] == '/' && filename_start < uri.size())
        filename_start++;
    size_t qmark_pos = uri.find("?");
    const DownloadConfig* config = nullptr;
    if (qmark_pos != std::string::npos) {
        filename = uri.substr(filename_start, qmark_pos - filename_start);
        ip = uri.substr(qmark_pos + 1);
        for (const auto& conf : configs)
            if (conf.matches_address(inet_addr(ip.c_str()))) {
                config = &conf;
                break;
            }
    }
    if (config == nullptr)
        return R"del(#!ipxe
echo Unknown host!
shell
)del";
    std::string answer = "#!ipxe\n\n:retry\ndhcp && isset ${filename} || goto retry\necho Booting from ${filename}\nkernel ";
    answer += IMAGE_METHOD;
    answer +=
        "://${next-server}//vmlinuz.img quiet pixie_server=${next-server} ip=";
    answer += config->get_ip_method() + " ";
    if (filename.substr(0, 4) == "wipe")
        answer += "pixie_wipe=" + filename.substr(5) + " ";
    answer +=
        "pixie_root_size=" + std::to_string(config->get_root_size()) + " ";
    answer +=
        "pixie_swap_size=" + std::to_string(config->get_swap_size()) + " ";
    answer += "pixie_sha224=" + config->get_config_hash().to_string() + " ";
    answer += config->get_extra_args() + " ";
    answer += " || goto error\ninitrd ";
    answer += IMAGE_METHOD;
    answer += "://${next-server}//initrd.img || goto error\nboot || goto error\nerror:\nshell";
    return answer;
}

HttpServer::HttpServer(const std::vector<DownloadConfig>& configs)
    : configs(configs) {
    const char* port_ = getenv("PIXIE_HTTP_PORT");
    uint16_t port;
    if (port_ == nullptr)
        port = DEFAULT_HTTP_PORT;
    else
        port = stoi(std::string(port_));
    port = htons(port);
    sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock == -1) throw std::runtime_error("socket: "s + strerror(errno));
    struct sockaddr_in server_address;
    memset(&server_address, 0, sizeof(server_address));
    server_address.sin_family = AF_INET;
    server_address.sin_port = port;
    const char* addr = getenv("PIXIE_HTTP_ADDR");
    if (addr == nullptr) addr = DEFAULT_HTTP_ADDR;
    if (inet_aton(addr, &server_address.sin_addr) != 1)
        throw std::runtime_error("inet_aton: "s + strerror(errno));
    int one = 1;
    if (setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(int)) < 0)
        throw std::runtime_error("setsockopt: "s + strerror(errno));
    if (bind(sock, (struct sockaddr*)&server_address, sizeof(server_address)))
        throw std::runtime_error("bind: "s + strerror(errno));
    if (listen(sock, SOMAXCONN) == -1)
        throw std::runtime_error("listen: "s + strerror(errno));
    if (fcntl(sock, F_SETFL, O_NONBLOCK) == -1)
        throw std::runtime_error("fcntl: "s + strerror(errno));
    epoll = epoll_create(1);
    if (epoll == -1)
        throw std::runtime_error("epoll_create: "s + strerror(errno));
    struct rlimit rlim;
    if (getrlimit(RLIMIT_NOFILE, &rlim) == -1)
        throw std::runtime_error("getrlimit: "s + strerror(errno));
    max_fd_no = rlim.rlim_cur;
}

void HttpServer::operator()() {
    struct epoll_event read_event;
    struct epoll_event write_event;
    std::vector<struct epoll_event> events(maxevents);
    read_event.events = EPOLLIN;
    write_event.events = EPOLLOUT;
    read_event.data.fd = sock;
    std::vector<std::unique_ptr<ReceiveBuffer>> to_read(max_fd_no);
    std::vector<std::unique_ptr<SendBuffer>> to_write(max_fd_no);
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, sock, &read_event) == -1)
        throw std::runtime_error("epoll_ctl: "s + strerror(errno));
    while (true) {
        int num_fd = epoll_wait(epoll, events.data(), maxevents, -1);
        if (num_fd == -1 && errno != EINTR)
            throw std::runtime_error("epoll_wait: "s + strerror(errno));
        for (int i = 0; i < num_fd; i++) {
            int fd = events[i].data.fd;
            if (events[i].events & EPOLLERR || events[i].events & EPOLLHUP) {
                fprintf(stderr, "Something went wrong with fd %d\n", fd);
                close(fd);
                continue;
            } else if (events[i].events & EPOLLOUT) {
                int ret = to_write[fd]->send(fd);
                if (ret == 1) continue;
                write_event.data.fd = fd;
                if (epoll_ctl(epoll, EPOLL_CTL_DEL, fd, &write_event) == -1)
                    perror("epoll_ctl");
                close(fd);
            } else if (events[i].events & EPOLLIN) {
                if (fd == sock) {
                    int client = accept4(sock, nullptr, nullptr, SOCK_NONBLOCK);
                    if (client == -1) {
                        perror("accept4");
                    } else {
                        to_read[client] = std::make_unique<ReceiveBuffer>();
                        read_event.data.fd = client;
                        if (epoll_ctl(epoll, EPOLL_CTL_ADD, client,
                                      &read_event) == -1)
                            perror("epoll_ctl");
                    }
                } else {
                    int ret = to_read[fd]->recv(fd);
                    if (ret == -1) {
                        close(fd);
                        continue;
                    } else if (ret == 1)
                        continue;
                    std::string request_data = to_read[fd]->get_data();
                    int newline_location = request_data.find("\n");
                    request_data = request_data.substr(0, newline_location);
                    if (request_data.back() == '\r') request_data.pop_back();
                    std::cerr << request_data << std::endl;
                    int status_code = 200;
                    std::string status = "OK";
                    std::string content;
                    if (request_data.substr(0, 4) != "GET ") {
                        status = "Method Not Allowed";
                        status_code = 405;
                    } else {
                        try {
                            std::string uri = request_data.substr(4);
                            size_t space_pos = uri.find(" ");
                            uri.erase(space_pos);
                            content = generate_script(uri);
                        } catch (...) {
                            status = "Bad request";
                            status_code = 500;
                        }
                    }
                    std::string answer_data = "HTTP/1.0 ";
                    answer_data += std::to_string(status_code);
                    answer_data += " ";
                    answer_data += status;
                    answer_data += "\r\n";
                    answer_data += "Content-Length: ";
                    answer_data += std::to_string(content.size());
                    answer_data += "\r\n\r\n";
                    answer_data += content;
                    to_write[fd] = std::make_unique<SendBuffer>(answer_data);
                    write_event.data.fd = fd;
                    if (epoll_ctl(epoll, EPOLL_CTL_MOD, fd, &write_event) == -1)
                        perror("epoll_ctl");
                }
            }
        }
    }
}
