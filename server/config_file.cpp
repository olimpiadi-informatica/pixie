#include <arpa/inet.h>
#include <config_file.h>
#include <algorithm>
#include <climits>

DownloadConfig::DownloadConfig(
    const std::string& subnet,
    std::vector<std::pair<std::string, std::string>> files,
    chunk_size_t chunk_size)
    : chunk_size(chunk_size) {
    auto slash = subnet.begin() + subnet.find("/");
    if (slash == subnet.end())
        throw std::runtime_error("Invalid subnet given!");
    std::string ip{subnet.begin(), slash};
    std::string mask{slash + 1, subnet.end()};
    ip_address = inet_addr(ip.c_str());
    if (ip_address == INADDR_NONE)
        throw std::runtime_error("Invalid subnet given!");
    int integer_mask = std::stoi(mask);
    if (integer_mask < 0 || integer_mask > 32)
        throw std::runtime_error("Invalid subnet given!");
    subnet_mask = htonl(UINT_MAX << (32 - integer_mask));

    // Sort files to avoid order changes causing hash changes.
    std::sort(files.begin(), files.end());
    SHA224 hasher;
    for (auto file : files)
        file_data.emplace(file.first, File{file.second, chunk_size, hasher});
    config_hash = hasher.get();
}

std::vector<DownloadConfig> parse_config(
    const std::vector<std::string>& configs) {
    return {};
}
