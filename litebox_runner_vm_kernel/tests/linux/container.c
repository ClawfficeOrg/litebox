// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// container.config.json: rootfs.tar, read-only, with tmpfs at /run and
// /var/tmp. A later run (see container.json) sees the tmpfs writes.

#include <err.h>
#include <errno.h>
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

static void create(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) err(1, "%s", path);
    close(fd);
}

static void expect_erofs(int result, const char *what) {
    if (result >= 0 || errno != EROFS)
        errx(1, "%s: not EROFS (result %d, errno %d)", what, result, errno);
}

int main(int argc, char **argv) {
    struct stat run, var_tmp;
    cat("/etc/greeting.txt");

    expect_erofs(open("/etc/greeting.txt", O_WRONLY), "open for writing");
    expect_erofs(open("/etc/new.txt", O_WRONLY | O_CREAT, 0644), "create");
    expect_erofs(mkdir("/etc/dir", 0755), "mkdir");
    expect_erofs(unlink("/etc/image.txt"), "unlink");
    if (stat("/tmp", &run) == 0) errx(1, "/tmp exists without a writable layer");
    printf("read-only\n");

    if (argc > 1 && strcmp(argv[1], "write") == 0) {
        create("/run/pid", "1\n");
        if (mkdir("/var/tmp/dir", 0755) < 0) err(1, "mkdir");
        create("/var/tmp/dir/file.txt", "tmpfs\n");
    }
    cat("/var/tmp/dir/file.txt");
    if (stat("/run/pid", &run) < 0 || stat("/var/tmp/dir/file.txt", &var_tmp) < 0) err(1, "stat");
    if (run.st_dev == var_tmp.st_dev) errx(1, "/run and /var/tmp on one device");
    return 0;
}
