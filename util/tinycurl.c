#include <stdio.h>
#include <sys/socket.h>
#include <arpa/inet.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char** argv) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s url_to_download\n", argv[0]);
        return 127;
    }
    const char* url = argv[1];
    const char* url_prefix = "http://";

    if (strncmp(url_prefix, url, strlen(url_prefix)) == 0)
        url += strlen(url_prefix);

    struct sockaddr_in address;
    memset(&address, 0, sizeof(address));
    address.sin_family = AF_INET;
    const char* path = strchr(url, '/');
    if (path == NULL) {
        fprintf(stderr, "Invalid URL\n");
        return 127;
    }
    if (strlen(path) > 32768) {
        fprintf(stderr, "URL too long\n");
        return 127;
    }
    const char* end = path;
    const char* port = strchr(url, ':');
    int prt = 80;
    if (port != NULL && port < path) {
        char* e;
        prt = strtol(port, &e, 10);
        if (e != path) {
            fprintf(stderr, "Invalid URL\n");
            return 127;
        }
        end = port;
    }
    address.sin_port = htons(prt);
    int ip_length = end - url;
    if (ip_length >= 1023) {
        fprintf(stderr, "Invalid IP specified\n");
        return 127;
    }
    char ip[1024];
    strncpy(ip, url, ip_length);
    ip[ip_length] = 0;
    if (inet_pton(AF_INET, ip, &address.sin_addr) == 0) {
        fprintf(stderr, "inet_pton: Error reading IP from %s\n", ip);
        return 127;
    }


    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock == -1) {
        perror("socket");
        return 126;
    }
    if (connect(sock, (struct sockaddr*)&address, sizeof(address)) == -1) {
        perror("connect");
        return 126;
    }
    char request[65536] = {};
    snprintf(request, sizeof(request), "GET %s HTTP/1.0\r\n\r\n", path);
    ssize_t pos = 0;
    ssize_t limit = strlen(request);
    do {
        ssize_t ret = write(sock, request+pos, limit-pos);
        if (ret == -1) {
            perror("write");
            return 126;
        }
        pos += ret;
    } while (pos < limit);


    int nread;
    size_t len = 0;
    char* line = NULL;
    int firstline = 1;
    int body = 0;
    int error = 0;
    FILE* sock_file = fdopen(sock, "r");
    if (sock_file == NULL) {
        perror("fdopen");
        return 126;
    }
    while ((nread = getline(&line, &len, sock_file)) != -1) {
        if (body) {
            fprintf(error?stderr:stdout, "%s", line);
            continue;
        }
        if (firstline) {
            if (strstr(line, "200") == NULL) {
                error = 1;
            } else {
                error = 0;
            }
            firstline = 0;
            continue;
        }
        if (strcmp(line, "\r\n") == 0) {
            body = 1;
        }
    }
    free(line);
    fclose(sock_file);
    return error;
}
