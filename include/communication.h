#ifndef PIXIE_COMMUNICATION_H
#define PIXIE_COMMUNICATION_H
#include <arpa/inet.h>
#include <common.h>
#include <cstdint>
#include <cstring>

static const uint32_t maximum_data_size = 1400;

static const uint32_t chunk_list_request = 0x1;
static const uint32_t data_request = 0x2;
static const uint32_t chunk_list_info = 0x1;
static const uint32_t data_packet = 0x2;

static uint32_t extract_message_type(const uint8_t* buffer) {
    uint32_t message_n;
    memcpy(&message_n, buffer, sizeof(uint32_t));
    return ntohl(message_n);
}

struct ChunkListRequest {
    uint32_t message_type = chunk_list_request;
    sha224_t hash;

    uint32_t fill_buffer(uint8_t* buffer) const {
        uint32_t message_n = htonl(message_type);
        memcpy(buffer, &message_n, sizeof(uint32_t));
        memcpy(buffer + sizeof(uint32_t), &hash, sizeof(sha224_t));
        return sizeof(uint32_t) + sizeof(sha224_t);
    }

    void read_from_buffer(const uint8_t* buffer, uint32_t size) {
        memcpy(this, buffer, size);
        message_type = extract_message_type(buffer);
    }
};

struct DataRequest {
    uint32_t message_type = data_request;
    uint32_t start;
    uint32_t length;
    sha224_t chunk;

    uint32_t fill_buffer(uint8_t* buffer) const {
        uint32_t message_n = htonl(message_type);
        memcpy(buffer, &message_n, sizeof(uint32_t));
        message_n = htonl(start);
        memcpy(buffer + sizeof(uint32_t), &message_n, sizeof(uint32_t));
        message_n = htonl(length);
        memcpy(buffer + 2 * sizeof(uint32_t), &message_n, sizeof(uint32_t));
        memcpy(buffer + 3 * sizeof(uint32_t), &chunk, sizeof(sha224_t));
        return 3 * sizeof(uint32_t) + sizeof(sha224_t);
    }

    void read_from_buffer(const uint8_t* buffer, uint32_t size) {
        memcpy(this, buffer, size);
        message_type = extract_message_type(buffer);
        start = ntohl(start);
        length = ntohl(length);
    }
};

struct ChunkListInfo {
    uint32_t message_type = chunk_list_info;
    uint32_t length;
    sha224_t hash;

    uint32_t fill_buffer(uint8_t* buffer) const {
        uint32_t message_n = htonl(message_type);
        memcpy(buffer, &message_n, sizeof(uint32_t));
        message_n = htonl(length);
        memcpy(buffer + sizeof(uint32_t), &message_n, sizeof(uint32_t));
        memcpy(buffer + 2 * sizeof(uint32_t), &hash, sizeof(sha224_t));
        return 2 * sizeof(uint32_t) + sizeof(sha224_t);
    }

    void read_from_buffer(const uint8_t* buffer, uint32_t size) {
        memcpy(this, buffer, size);
        message_type = extract_message_type(buffer);
        length = ntohl(length);
    }
};

struct DataPacket {
    uint32_t message_type = data_packet;
    uint32_t offset;
    sha224_t chunk;
    uint8_t data[maximum_data_size];
    uint32_t data_length;

    uint32_t fill_buffer(uint8_t* buffer) const {
        uint32_t message_n = htonl(message_type);
        memcpy(buffer, &message_n, sizeof(uint32_t));
        message_n = htonl(offset);
        memcpy(buffer + sizeof(uint32_t), &message_n, sizeof(uint32_t));
        memcpy(buffer + 2 * sizeof(uint32_t), &chunk, sizeof(sha224_t));
        memcpy(buffer + 2 * sizeof(uint32_t) + sizeof(sha224_t), data,
               data_length);
        return 2 * sizeof(uint32_t) + sizeof(sha224_t) + data_length;
    }

    void read_from_buffer(const uint8_t* buffer, uint32_t size) {
        memcpy(this, buffer, size);
        message_type = extract_message_type(buffer);
        offset = ntohl(offset);
        data_length = size - (2 * sizeof(uint32_t) + sizeof(sha224_t));
    }
};

template <typename PacketType>
uint32_t fill_buffer(const PacketType& packet, uint8_t* buffer) {
    return packet.fill_buffer(buffer);
}

#endif
