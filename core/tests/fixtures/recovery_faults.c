#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int selected(const char *operation, const char *path) {
    const char *op = getenv("KURISU_RECOVERY_OP");
    const char *match = getenv("KURISU_RECOVERY_MATCH");
    const char *arm = getenv("KURISU_RECOVERY_ARM");
    const char *after = getenv("KURISU_RECOVERY_AFTER");
    return (!after || access(after, F_OK) == 0) &&
        op && match && arm && access(arm, F_OK) == 0 &&
        strcmp(op, operation) == 0 && strstr(path, match);
}

static int fault(const char *operation, const char *path) {
    if (!selected(operation, path)) return 0;
    const char *gate = getenv("KURISU_RECOVERY_GATE");
    if (gate) {
        int fd = open(gate, O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (fd < 0) _exit(90);
        close(fd);
        for (int i = 0; i < 15000; i++) {
            if (access(getenv("KURISU_RECOVERY_ARM"), F_OK) != 0) return 0;
            usleep(1000);
        }
        _exit(91);
    }
    errno = ENOSPC;
    return 1;
}

static int fd_fault(const char *operation, int fd) {
    char name[64], path[4096];
    snprintf(name, sizeof(name), "/proc/self/fd/%d", fd);
    ssize_t len = readlink(name, path, sizeof(path) - 1);
    if (len < 0) return 0;
    path[len] = 0;
    return fault(operation, path);
}

#define OPEN_WRAPPER(name) \
int name(const char *path, int flags, ...) { \
    mode_t mode = 0; \
    if (flags & O_CREAT) { \
        va_list args; va_start(args, flags); mode = va_arg(args, int); va_end(args); \
        if (fault("create", path)) return -1; \
    } \
    int (*real)(const char *, int, ...) = dlsym(RTLD_NEXT, #name); \
    return real(path, flags, mode); \
}
OPEN_WRAPPER(open)
OPEN_WRAPPER(open64)

ssize_t write(int fd, const void *buffer, size_t size) {
    if (fd_fault("write", fd)) return -1;
    ssize_t (*real)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    return real(fd, buffer, size);
}

int fsync(int fd) {
    if (fd_fault("sync", fd)) return -1;
    int (*real)(int) = dlsym(RTLD_NEXT, "fsync");
    return real(fd);
}

static int deny_link(const char *source) {
    const char *match = getenv("KURISU_RECOVERY_NO_LINKS");
    if (match && strstr(source, match)) {
        const char *error = getenv("KURISU_RECOVERY_LINK_ERRNO");
        errno = error && strcmp(error, "perm") == 0 ? EPERM : EOPNOTSUPP;
        return 1;
    }
    return 0;
}

int link(const char *source, const char *destination) {
    if (deny_link(source)) return -1;
    int (*real)(const char *, const char *) = dlsym(RTLD_NEXT, "link");
    int result = real(source, destination);
    if (result == 0 && fault("linked", destination)) return -1;
    return result;
}

int linkat(int source_dir, const char *source, int destination_dir,
           const char *destination, int flags) {
    if (deny_link(source)) return -1;
    int (*real)(int, const char *, int, const char *, int) = dlsym(RTLD_NEXT, "linkat");
    int result = real(source_dir, source, destination_dir, destination, flags);
    if (result == 0 && fault("linked", destination)) return -1;
    return result;
}

int rename(const char *source, const char *destination) {
    int (*real)(const char *, const char *) = dlsym(RTLD_NEXT, "rename");
    int result = real(source, destination);
    if (result == 0 && fault("renamed", destination)) return -1;
    return result;
}

int renameat2(int source_dir, const char *source, int destination_dir,
              const char *destination, unsigned int flags) {
    const char *collision = getenv("KURISU_RECOVERY_COLLISION");
    if (collision && strcmp(destination, collision) == 0) {
        int fd = open(destination, O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (fd < 0) _exit(92);
        if (write(fd, "preserve collision", 18) != 18) _exit(93);
        close(fd);
    }
    int (*real)(int, const char *, int, const char *, unsigned int) = dlsym(RTLD_NEXT, "renameat2");
    int result = real(source_dir, source, destination_dir, destination, flags);
    if (result == 0 && fault("renamed", destination)) return -1;
    return result;
}
