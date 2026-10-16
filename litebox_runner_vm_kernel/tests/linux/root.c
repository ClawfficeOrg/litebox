// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The writable root: a later run (see root.json) sees what one wrote in /tmp.

#include <err.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char buf[64];
    if (argc > 1 && strcmp(argv[1], "write") == 0) {
        if (mkdir("/tmp/dir", 0755) < 0) err(1, "mkdir");
        int fd = open("/tmp/dir/file.txt", O_WRONLY | O_CREAT | O_EXCL, 0644);
        if (fd < 0 || write(fd, "in memory\n", 10) != 10) err(1, "write");
        close(fd);
    }
    int fd = open("/tmp/dir/file.txt", O_RDONLY);
    ssize_t n = fd < 0 ? -1 : read(fd, buf, sizeof buf);
    if (n < 0) err(1, "read");
    close(fd);
    fwrite(buf, 1, n, stdout);
    return 0;
}
