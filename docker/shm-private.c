// Preloaded into everything on a desktop. Chromium makes its window's shared memory readable and
// writable by every user (mode 0606), so that an X server running as another user could attach it,
// and any process in the box could read a person's page out of it. Here X runs as the desktop's own
// user, so owner-only is all it needs. Failing closed isn't this file's job: the Bot's check from
// outside refuses a desktop with a segment left open.
#define _GNU_SOURCE
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include <unistd.h>

int shmget(key_t key, size_t size, int flags) {
    return syscall(SYS_shmget, key, size, flags & ~0077);
}
