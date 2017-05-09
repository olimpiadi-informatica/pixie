#include <sys/stat.h>
#include <sys/reboot.h>
#include <linux/reboot.h>
#include <fcntl.h>
#include <syscall.h>
#include <string.h>
#include <stdio.h>
#include <errno.h>
#include <unistd.h>
#include <stdlib.h>

#ifndef __NR_kexec_file_load
#define __NR_kexec_file_load 320
#endif
#ifndef LINUX_REBOOT_CMD_KEXEC
#define LINUX_REBOOT_CMD_KEXEC      0x45584543
#endif

int main(int argc, char** argv) {
    int bzfd = -1;
    int initfd = -1;

    if (argc != 4) {
        fprintf(stderr, "Usage: %s <bzImage> <initrd> <command line>\n", argv[0]);
        return 1;
    }

    if ((bzfd = open(argv[1], O_RDONLY)) == -1) {
        fprintf(stderr, "Cannot open %s: %s\n", argv[1], strerror(errno));
        return 2;
    }
    if ((initfd = open(argv[2], O_RDONLY)) == -1) {
        fprintf(stderr, "Cannot open %s: %s\n", argv[2], strerror(errno));
        return 2;
    }
    if (syscall(
            __NR_kexec_file_load,   // Syscall number
            bzfd,                  // File descriptor pointing to the kernel
            initfd,                 // File descriptor pointing to the initrd
            strlen(argv[3])+1,      // Length of the cmdline (including trailing \0!)
            argv[3],                // Command line
            0                       // Flags
        ) == -1) {
        fprintf(stderr, "kexec_file_load failed: %s\n", strerror(errno));
        return 7;
    }

    close(initfd);
    close(bzfd);
    sync();

    if (reboot(LINUX_REBOOT_CMD_KEXEC) == -1) {
        fprintf(stderr, "reboot failed: %s\n", strerror(errno));
        return 8;
    }
    return 0;
}
