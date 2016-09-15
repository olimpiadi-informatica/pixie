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

sha224_t::sha224_t(const std::string& text) {
    if (text.size() != 56) throw std::runtime_error("Invalid sha224 given!");
    auto from_hex = [](char c) {
        return (c | ' ') < 'a' ? (c - '0') : ((c | ' ') - 'a' + 10);
    };
    for (unsigned i = 0; i < size(); i++)
        at(i) = from_hex(text[2 * i]) << 4 | from_hex(text[2 * i + 1]);
}
