#include <bchooser.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <sys/types.h>
#include <cstring>
#include <iostream>

using namespace std::literals;

BroadcastChooser::BroadcastChooser() {
    struct ifaddrs* head;
    if (getifaddrs(&head) == -1)
        throw std::runtime_error("getifaddrs: "s + strerror(errno));
    for (struct ifaddrs* addr = head; addr != nullptr; addr = addr->ifa_next) {
        if (addr->ifa_addr->sa_family != AF_INET) continue;
        if ((addr->ifa_flags & IFF_BROADCAST) == 0) continue;
        struct in_addr if_addr =
            ((struct sockaddr_in*)addr->ifa_addr)->sin_addr;
        struct in_addr nm_addr =
            ((struct sockaddr_in*)addr->ifa_netmask)->sin_addr;
        struct in_addr bd_addr =
            ((struct sockaddr_in*)addr->ifa_broadaddr)->sin_addr;
        std::cerr << "Found interface " << addr->ifa_name << ", with ip "
                  << std::string(inet_ntoa(if_addr)) << ", netmask "
                  << std::string(inet_ntoa(nm_addr)) << " and broadcast "
                  << std::string(inet_ntoa(bd_addr)) << std::endl;
        addresses.emplace_back(if_addr.s_addr, nm_addr.s_addr, bd_addr.s_addr);
    }
    freeifaddrs(head);
}
