#include <config_file.h>
#include <http_server.h>
#include <cstdio>
#include <cstdlib>
#include <thread>

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s config_file [config_file [...]]\n", argv[0]);
        return EXIT_FAILURE;
    }
    std::vector<std::string> config_files;
    for (int i = 1; i < argc; i++) config_files.emplace_back(argv[i]);
    auto configs = parse_config(config_files);
    HttpServer http_server(configs);
    std::thread server_thread([&]() { http_server(); });
    server_thread.join();
}
