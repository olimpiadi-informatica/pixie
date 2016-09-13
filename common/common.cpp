#include <common.h>

std::string sha224_t::to_string() const {
    auto to_hex = [](int i) { return i < 10 ? '0' + i : 'a' - 10 + i; };
    std::string ans;
    for (unsigned i = 0; i < size(); i++) {
        ans += to_hex(at(i) >> 4);
        ans += to_hex(at(i) & 0xF);
    }
    return ans;
}
