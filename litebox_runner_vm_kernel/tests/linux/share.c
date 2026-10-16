// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Virtio-9p shares; see dev_tools/run_linux_on_vm_userland.sh for which host
// directory each is. /mnt/ro sees /mnt/rw's writes, and /data none of
// /mnt/host's. With `again`, checks that a previous run's changes persist.

#include <dirent.h>
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

static void put(const char *path, int flags, const char *text) {
    int fd = open(path, O_WRONLY | flags, 0644);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) err(1, "%s", path);
    close(fd);
}

static void expect_errno(int result, int expected, const char *what) {
    if (result >= 0 || errno != expected)
        errx(1, "%s: expected errno %d (result %d, errno %d)", what, expected, result, errno);
}

static int listed(const char *path, const char *name) {
    DIR *dir = opendir(path);
    if (dir == NULL) err(1, "%s", path);
    int found = 0;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) found |= strcmp(entry->d_name, name) == 0;
    closedir(dir);
    return found;
}

int main(int argc, char **argv) {
    struct stat st;
    if (argc > 1 && strcmp(argv[1], "again") == 0) {
        cat("/mnt/host/dir/new.txt");
        expect_errno(stat("/mnt/host/greeting.txt", &st), ENOENT, "stat removed");
        cat("/mnt/ro/greeting.txt");
        printf("again\n");
        return 0;
    }

    cat("/mnt/host/greeting.txt");
    if (!listed("/mnt/host", "greeting.txt")) errx(1, "greeting.txt not listed");
    put("/mnt/host/greeting.txt", O_APPEND, "and from the guest\n");
    cat("/mnt/host/greeting.txt");
    if (mkdir("/mnt/host/dir", 0755) < 0) err(1, "mkdir");
    put("/mnt/host/dir/new.txt", O_CREAT | O_EXCL, "new\n");
    cat("/mnt/host/dir/new.txt");
    if (!listed("/mnt/host/dir", "new.txt")) errx(1, "new.txt not listed");
    // The host's entries and the guest's, merged.
    if (!listed("/mnt/host", "greeting.txt") || !listed("/mnt/host", "dir"))
        errx(1, "/mnt/host not merged");
    if (unlink("/mnt/host/greeting.txt") < 0) err(1, "unlink");
    expect_errno(stat("/mnt/host/greeting.txt", &st), ENOENT, "stat removed");
    if (listed("/mnt/host", "greeting.txt")) errx(1, "greeting.txt still listed");
    printf("writable in memory\n");

    cat("/mnt/ro/greeting.txt");
    expect_errno(open("/mnt/ro/greeting.txt", O_WRONLY), EROFS, "open for writing");
    expect_errno(open("/mnt/ro/new.txt", O_WRONLY | O_CREAT, 0644), EROFS, "create");
    expect_errno(mkdir("/mnt/ro/dir", 0755), EROFS, "mkdir");
    expect_errno(unlink("/mnt/ro/greeting.txt"), EROFS, "unlink");
    // The 9P backend itself would panic the kernel on this one.
    expect_errno(open("/mnt/ro", O_WRONLY), EROFS, "open the directory for writing");
    printf("read-only\n");

    put("/mnt/rw/greeting.txt", O_APPEND, "and from the guest\n");
    if (mkdir("/mnt/rw/dir", 0755) < 0) err(1, "mkdir");
    put("/mnt/rw/dir/new.txt", O_CREAT | O_EXCL, "new\n");
    // Must fail without truncating it.
    expect_errno(open("/mnt/rw/greeting.txt", O_WRONLY | O_CREAT | O_EXCL | O_TRUNC, 0644), EEXIST,
                 "exclusive creation");
    cat("/mnt/ro/greeting.txt");
    cat("/mnt/ro/dir/new.txt");
    if (unlink("/mnt/rw/dir/new.txt") < 0 || rmdir("/mnt/rw/dir") < 0) err(1, "remove");
    expect_errno(stat("/mnt/ro/dir", &st), ENOENT, "stat removed");
    // An open directory still reads after its removal.
    if (mkdir("/mnt/rw/gone", 0755) < 0) err(1, "mkdir");
    DIR *gone = opendir("/mnt/rw/gone");
    if (gone == NULL || rmdir("/mnt/rw/gone") < 0) err(1, "gone");
    errno = 0;
    while (readdir(gone) != NULL) {
    }
    if (errno != 0) err(1, "readdir of a removed directory");
    closedir(gone);
    printf("read-write\n");

    cat("/data/greeting.txt");
    expect_errno(stat("/data/image.txt", &st), ENOENT, "stat the image's /data");
    if (!listed("/", "data") || !listed("/mnt/host", "nested")) errx(1, "mount point not listed");
    cat("/mnt/host/nested/greeting.txt");
    expect_errno(open("/mnt/host/nested/new.txt", O_WRONLY | O_CREAT, 0644), EROFS, "create");
    printf("composed\n");
    return 0;
}
