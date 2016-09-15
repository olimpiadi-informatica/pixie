#include <arpa/inet.h>
#include <config_file.h>
#include <json/json.h>
#include <algorithm>
#include <climits>
#include <fstream>
#include <utility>

DownloadConfig::DownloadConfig(
    const std::string& subnet,
    std::vector<std::pair<std::string, std::string>> files,
    chunk_size_t chunk_size, uint64_t swap_size, uint64_t root_size,
    const std::string& ip_method)
    : chunk_size(chunk_size),
      swap_size(swap_size),
      root_size(root_size),
      ip_method(ip_method) {
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
        file_data.emplace(file.first, InFile{file.second, chunk_size, hasher});
    config_hash = hasher.get();
}

std::vector<DownloadConfig> parse_config(
    const std::vector<std::string>& configs) {
    using namespace std::string_literals;
    std::vector<DownloadConfig> configurations;
    for (const auto& config : configs) {
        std::ifstream config_file(config,
                                  std::ifstream::binary | std::ifstream::in);
        Json::Value config_root;
        config_file >> config_root;
        double swap_size = config_root.get("swap_size", 1.0).asDouble();
        if (swap_size < 0.0) throw std::runtime_error("swap_size is negative!");
        double root_size = config_root.get("root_size", 10.0).asDouble();
        if (root_size <= 0.0)
            throw std::runtime_error("root_size is not positive!");
        std::string subnet = config_root.get("subnet", "").asString();
        if (subnet == "")
            throw std::runtime_error("Subnet missing in the config file!");
        chunk_size_t chunk_size =
            config_root.get("chunk_size", DEFAULT_CHUNK_SIZE).asUInt();
        std::string ip_method =
            config_root.get("ip_method", DEFAULT_IP_METHOD).asString();
        auto& file_list = config_root["files"];
        if (!file_list.isObject()) throw std::runtime_error("Wrong file list!");
        std::vector<std::pair<std::string, std::string>> files;
        std::string canonical_path;
        if (config.front() != '/')
            canonical_path = config;
        else
            canonical_path = "./"s + config;
        while (canonical_path.back() != '/') canonical_path.pop_back();
        for (auto fname : file_list.getMemberNames()) {
            std::string path = file_list.get(fname, "").asString();
            if (path.front() != '/') path = canonical_path + path;
            files.push_back(make_pair(fname, path));
        }
        configurations.emplace_back(subnet, files, chunk_size,
                                    swap_size * (1ULL << 20),
                                    root_size * (1ULL << 20), ip_method);
    }
    return configurations;
}

std::vector<uint8_t> DownloadConfig::get_chunk_list() const {
    std::vector<uint8_t> ans;
    for (const auto& file : file_data) {
        ans.insert(ans.end(), file.first.begin(), file.first.end());
        ans.push_back(0);
        ans.resize(ans.size() + sizeof(uint32_t));
        uint32_t chunk_count = file.second.get_chunks().size();
        chunk_count = htonl(chunk_count);
        memcpy(ans.data() + (ans.size() - sizeof(uint32_t)), &chunk_count,
               sizeof(uint32_t));
        for (const auto& chunk : file.second.get_chunks()) {
            ans.resize(ans.size() + sizeof(chunk));
            chunk.fill_buffer(ans.data() + (ans.size() - sizeof(chunk)));
        }
    }
    return ans;
}
