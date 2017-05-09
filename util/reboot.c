#include <sys/reboot.h>
#include <stdio.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>

#ifndef LINUX_REBOOT_CMD_RESTART
#define LINUX_REBOOT_CMD_RESTART      0x01234567
#endif

int main() {
    if (reboot(LINUX_REBOOT_CMD_RESTART) == -1) {
        fprintf(stderr, "reboot failed: %s\n", strerror(errno));
        return 8;
    }
    return 0;
}
