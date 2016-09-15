#ifndef PIXIE_BCHOOSER_H
#define PIXIE_BCHOOSER_H
#include <arpa/inet.h>
#include <sys/socket.h>
#include <tuple>
#include <vector>

class BroadcastChooser {
    std::vector<std::tuple<in_addr_t, in_addr_t, in_addr_t>> addresses;

  public:
    in_addr_t get_bc_address(in_addr_t addr) {
        for (auto i : addresses) {
            if ((std::get<0>(i) & std::get<1>(i)) == (addr & std::get<1>(i)))
                return std::get<2>(i);
        }
        struct in_addr a;
        a.s_addr = addr;
        throw std::runtime_error("Unknown address " +
                                 std::string(inet_ntoa(a)));
    }
    BroadcastChooser();
};

#endif
