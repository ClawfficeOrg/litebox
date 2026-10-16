// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The writable root over rootfs.tar and layer.tar (see root.config.json): the
// layers merged, under writes that later runs (see root.json) see.

#include <err.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void cat(const char *path) {
    char buf[256];
    int fd = open(path, O_RDONLY);
    if (fd < 0) err(1, "%s", path);
    ssize_t n;
    while ((n = read(fd, buf, sizeof buf)) > 0) fwrite(buf, 1, n, stdout);
    if (n < 0) err(1, "%s", path);
    close(fd);
}

static void put(const char *path, int flags, const char *text) {
    int fd = open(path, O_WRONLY | flags, 0644);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) err(1, "%s", path);
    close(fd);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "write") == 0) {
        // layer.tar's, writable by all: copied up, then changed.
        put("/etc/greeting.txt", O_APPEND, "and from the guest\n");
        if (mkdir("/tmp/dir", 0755) < 0) err(1, "mkdir");
        put("/tmp/dir/file.txt", O_CREAT | O_EXCL, "in memory\n");
    }
    // layer.tar's greeting.txt hides rootfs.tar's.
    cat("/etc/greeting.txt");
    cat("/etc/layer.txt");
    cat("/etc/image.txt");
    cat("/tmp/dir/file.txt");
    return 0;
}
