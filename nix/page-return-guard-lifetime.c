/* Disposable VM only: shared-mm owner turnover and FD/exec lifetime. */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROOT "/sys/fs/cgroup/amc-guard-lifetime"
#define SIZE (4 * 1048576)

static void put(const char *path, const char *value)
{
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    assert(fd >= 0);
    assert(write(fd, value, strlen(value)) == (ssize_t)strlen(value));
    assert(close(fd) == 0);
}

static int move(pid_t pid, const char *name)
{
    char path[256], value[32];
    snprintf(path, sizeof(path), ROOT "/%s/cgroup.procs", name);
    snprintf(value, sizeof(value), "%d", pid);
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    assert(fd >= 0);
    ssize_t result = write(fd, value, strlen(value));
    int error = result < 0 ? errno : 0;
    assert(close(fd) == 0);
    return error;
}

static int guard(pid_t pid)
{
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/amc_mem", pid);
    return open(path, O_RDONLY | O_CLOEXEC);
}

static uint64_t field(int fd, const char *key)
{
    char path[64], line[256];
    snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
    FILE *info = fopen(path, "r");
    assert(info);
    uint64_t value = 0;
    while (fgets(line, sizeof(line), info)) {
        if (strncmp(line, key, strlen(key)) == 0) {
            assert(sscanf(line + strlen(key), ":%" SCNu64, &value) == 1);
            break;
        }
    }
    assert(fclose(info) == 0);
    assert(value > 0);
    return value;
}

static int peer(void *arg)
{
    int socket = *(int *)arg;
    assert(move(getpid(), "outside") == 0);
    char ready = 'r';
    assert(write(socket, &ready, 1) == 1);
    for (;;) {
        char command;
        assert(read(socket, &command, 1) == 1);
        uint64_t result = 0;
        if (command == 'c') {
            int group = open(ROOT "/released", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
            assert(group >= 0);
            void *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            assert(stack != MAP_FAILED);
            struct clone_args args = {
                .flags = CLONE_VM | CLONE_INTO_CGROUP,
                .exit_signal = SIGCHLD,
                .stack = (uintptr_t)stack,
                .stack_size = 65536,
                .cgroup = (uint64_t)group,
            };
            long child = syscall(SYS_clone3, &args, sizeof(args));
            if (child == 0)
                _exit(2);
            assert(child == -1 && errno == EBUSY);
            assert(close(group) == 0);
            assert(munmap(stack, 65536) == 0);
            result = 1;
        } else if (command == 'f') {
            pid_t child = fork();
            assert(child >= 0);
            if (child == 0) {
                int fd = guard(getpid());
                assert(fd >= 0);
                uint64_t cookie = field(fd, "amc_guard_cookie");
                assert(close(fd) == 0);
                assert(write(socket, &cookie, sizeof(cookie)) == sizeof(cookie));
                _exit(0);
            }
            int status;
            assert(waitpid(child, &status, 0) == child);
            assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
            continue;
        } else {
            assert(command == 'e');
            execl("/run/current-system/sw/bin/sleep", "sleep", "60", NULL);
            abort();
        }
        assert(write(socket, &result, sizeof(result)) == sizeof(result));
    }
}

static uint64_t command(int socket, char value)
{
    assert(write(socket, &value, 1) == 1);
    uint64_t result;
    assert(read(socket, &result, sizeof(result)) == sizeof(result));
    return result;
}

static void residency(int fd, pid_t pid, uintptr_t address, uint64_t inode)
{
    char path[64], page[4096];
    snprintf(path, sizeof(path), "/proc/%d/pagemap", pid);
    int maps = open(path, O_RDONLY | O_CLOEXEC);
    int owners = open("/proc/kpagecgroup", O_RDONLY | O_CLOEXEC);
    assert(maps >= 0 && owners >= 0);
    for (size_t offset = 0; offset < SIZE; offset += sizeof(page)) {
        assert(pread(fd, page, sizeof(page), address + offset) == sizeof(page));
        assert(page[0] == 7);
        uint64_t pte, charge;
        assert(pread(maps, &pte, 8, (address + offset) / sizeof(page) * 8) == 8);
        assert((pte & (3ULL << 62)) == (1ULL << 63));
        uint64_t pfn = pte & ((1ULL << 55) - 1);
        assert(pfn > 0);
        assert(pread(owners, &charge, 8, pfn * 8) == 8);
        assert(charge == inode);
    }
    assert(close(maps) == 0 && close(owners) == 0);
}

int main(void)
{
    alarm(90);
    assert(mkdir(ROOT, 0700) == 0);
    put(ROOT "/cgroup.subtree_control", "+memory");
    const char *groups[] = {"original", "backed", "outside", "released"};
    for (size_t i = 0; i < 4; i++) {
        char path[256];
        snprintf(path, sizeof(path), ROOT "/%s", groups[i]);
        assert(mkdir(path, 0700) == 0);
        snprintf(path, sizeof(path), ROOT "/%s/memory.max", groups[i]);
        put(path, "134217728");
    }
    int control[2], metadata[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, control) == 0);
    assert(pipe2(metadata, O_CLOEXEC) == 0);
    pid_t owner = fork();
    assert(owner >= 0);
    if (owner == 0) {
        assert(close(control[0]) == 0 && close(metadata[0]) == 0);
        assert(move(getpid(), "original") == 0);
        char *memory = mmap(NULL, SIZE, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        char *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        assert(memory != MAP_FAILED && stack != MAP_FAILED);
        assert(madvise(memory, SIZE, MADV_NOHUGEPAGE) == 0);
        for (size_t offset = 0; offset < SIZE; offset += 4096)
            memory[offset] = 7;
        assert(madvise(memory, SIZE, MADV_PAGEOUT) == 0);
        pid_t sharer = clone(peer, stack + 65536, CLONE_VM | SIGCHLD, &control[1]);
        assert(sharer > 0);
        uintptr_t info[] = {(uintptr_t)sharer, (uintptr_t)memory};
        assert(write(metadata[1], info, sizeof(info)) == sizeof(info));
        for (;;)
            pause();
    }
    assert(close(control[1]) == 0 && close(metadata[1]) == 0);
    uintptr_t info[2];
    assert(read(metadata[0], info, sizeof(info)) == sizeof(info));
    assert(close(metadata[0]) == 0);
    pid_t sharer = info[0];
    char ready;
    assert(read(control[0], &ready, 1) == 1 && ready == 'r');
    int reclaim = open(ROOT "/original/memory.reclaim", O_WRONLY | O_CLOEXEC);
    assert(reclaim >= 0);
    const char *evict = "16777216 swappiness=max";
    ssize_t result = write(reclaim, evict, strlen(evict));
    assert(result >= 0 || errno == EAGAIN);
    assert(close(reclaim) == 0);
    assert(move(owner, "backed") == 0);
    assert(rmdir(ROOT "/original") == 0);
    char maps_path[64];
    snprintf(maps_path, sizeof(maps_path), "/proc/%d/pagemap", sharer);
    int maps = open(maps_path, O_RDONLY | O_CLOEXEC);
    assert(maps >= 0);
    for (size_t offset = 0; offset < SIZE; offset += 4096) {
        uint64_t pte;
        assert(pread(maps, &pte, 8, (info[1] + offset) / 4096 * 8) == 8);
        assert((pte & (3ULL << 62)) == (1ULL << 62));
    }
    assert(close(maps) == 0);
    int memory = guard(owner);
    int reader = guard(getpid());
    assert(memory >= 0 && reader >= 0);
    uint64_t cookie = field(memory, "amc_guard_cookie");
    struct stat backed;
    assert(stat(ROOT "/backed", &backed) == 0);
    assert(field(memory, "amc_guard_memcg_ino") == backed.st_ino);
    int duplicate = dup(memory), independent = guard(owner);
    assert(duplicate >= 0 && independent >= 0);
    assert(field(independent, "amc_guard_cookie") == cookie);
    assert(close(memory) == 0 && close(independent) == 0);
    memory = duplicate;
    put(ROOT "/cgroup.subtree_control", "+memory");
    assert(guard(sharer) == -1 && errno == EBUSY);
    assert(move(owner, "released") == EBUSY);
    assert(move(sharer, "released") == EBUSY);
    assert(command(control[0], 'c') == 1);
    assert(command(control[0], 'f') != cookie);
    assert(kill(owner, SIGKILL) == 0);
    int status;
    assert(waitpid(owner, &status, 0) == owner && WIFSIGNALED(status));
    assert(rmdir(ROOT "/backed") == -1 && errno == EBUSY);
    int subtree = open(ROOT "/cgroup.subtree_control", O_WRONLY | O_CLOEXEC);
    assert(subtree >= 0);
    assert(write(subtree, "-memory", 7) == -1 && errno == EBUSY);
    assert(close(subtree) == 0);
    residency(memory, sharer, info[1], backed.st_ino);
    assert(write(control[0], "e", 1) == 1);
    char exe[64], link[256], byte;
    snprintf(exe, sizeof(exe), "/proc/%d/exe", sharer);
    for (;;) {
        ssize_t count = readlink(exe, link, sizeof(link) - 1);
        assert(count > 0);
        link[count] = 0;
        if (strstr(link, "/sleep") || strstr(link, "/coreutils"))
            break;
        usleep(10000);
    }
    int replacement = guard(sharer);
    assert(replacement >= 0 && field(replacement, "amc_guard_cookie") != cookie);
    assert(pread(memory, &byte, 1, info[1]) == 0);
    assert(close(replacement) == 0);
    assert(move(sharer, "released") == 0);
    assert(rmdir(ROOT "/backed") == -1 && errno == EBUSY);
    assert(close(memory) == 0 && close(reader) == 0);
    assert(rmdir(ROOT "/backed") == 0);
    assert(kill(sharer, SIGKILL) == 0);
    /* The former CLONE_VM child was reparented on owner death. */
    for (int i = 0; rmdir(ROOT "/released") != 0; i++) {
        assert(errno == EBUSY && i < 3000);
        usleep(10000);
    }
    assert(close(control[0]) == 0);
    assert(rmdir(ROOT "/outside") == 0 && rmdir(ROOT) == 0);
    puts("{\"sharedMmMigrationDenied\":true,\"conflictingGuardDenied\":true,"
         "\"cloneIntoCgroupDenied\":true,\"privateForkReset\":true,"
         "\"ownerTurnoverPinnedCharge\":true,\"offliningDenied\":true,"
         "\"controllerDisableDenied\":true,\"execCookieChanged\":true,"
         "\"unchangedControllersAllowed\":true,"
         "\"obsoleteMmReadEmpty\":true,\"lastCloseReleased\":true}");
    return 0;
}
