#ifndef PIXIE_CONFIG_FILE_H
#define PIXIE_CONFIG_FILE_H
#include <file.h>
#include <map>
#include <string>
#include <vector>

#include <netinet/in.h>

class DownloadConfig {
    sha224_t config_hash;
    in_addr_t ip_address;
    in_addr_t subnet_mask;
    chunk_size_t chunk_size;
    uint64_t swap_size, root_size;
    std::string extra_args;
    std::map<std::string, InFile> file_data;

  public:
    DownloadConfig(const std::string& subnet,
                   std::vector<std::pair<std::string, std::string>> files,
                   chunk_size_t chunk_size, uint64_t swap_size,
                   uint64_t root_size, const std::string& extra_args);
    chunk_size_t get_chunk_size() const { return chunk_size; }
    bool matches_address(in_addr_t addr) const {
        return (ip_address & subnet_mask) == (addr & subnet_mask);
    }
    const std::map<std::string, InFile>& get_file_data() const {
        return file_data;
    }
    const sha224_t get_config_hash() const { return config_hash; }
    uint64_t get_root_size() const { return root_size; }
    uint64_t get_swap_size() const { return swap_size; }
    const std::string& get_extra_args() const { return extra_args; }
    std::vector<uint8_t> get_chunk_list() const;
};

std::vector<DownloadConfig> parse_config(const std::vector<std::string>&);
#endif
