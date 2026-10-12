// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Timer files: blocking reads, periodic expirations, and readiness through
// poll and epoll, each no earlier than the timer; and a nonblocking epoll
// sees a timer that expired while the program made no syscalls. See
// timerfd.json.

#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>
#include <x86intrin.h>

static long long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000LL + ts.tv_nsec / 1000;
}

static double ticks_per_us;

// Calibrates `spin_us` against the monotonic clock.
static void calibrate(void) {
    unsigned long long ticks = __rdtsc();
    long long start = now_us();
    struct timespec five_ms = {0, 5 * 1000 * 1000};
    nanosleep(&five_ms, NULL);
    ticks_per_us = (double)(__rdtsc() - ticks) / (double)(now_us() - start);
}

// Busy-waits without syscalls.
static void spin_us(long long us) {
    unsigned long long end = __rdtsc() + (unsigned long long)(us * ticks_per_us);
    while (__rdtsc() < end) {
    }
}

static void arm(int fd, long value_ms, long interval_ms) {
    struct itimerspec spec = {
        .it_interval = {interval_ms / 1000, interval_ms % 1000 * 1000000},
        .it_value = {value_ms / 1000, value_ms % 1000 * 1000000},
    };
    timerfd_settime(fd, 0, &spec, NULL);
}

int main(void) {
    int ok = 1;
    calibrate();
    int fd = timerfd_create(CLOCK_MONOTONIC, 0);
    uint64_t expirations = 0;

    long long start = now_us();
    arm(fd, 20, 0);
    int read_ok = read(fd, &expirations, sizeof(expirations)) == sizeof(expirations);
    int one_shot = read_ok && expirations == 1 && now_us() - start >= 20000;
    printf("one-shot: %d\n", one_shot);
    ok &= one_shot;

    arm(fd, 10, 10);
    struct timespec thirty_five_ms = {0, 35 * 1000 * 1000};
    nanosleep(&thirty_five_ms, NULL);
    read_ok = read(fd, &expirations, sizeof(expirations)) == sizeof(expirations);
    int periodic = read_ok && expirations >= 3;
    printf("periodic: %d\n", periodic);
    ok &= periodic;
    arm(fd, 0, 0);

    start = now_us();
    arm(fd, 15, 0);
    struct pollfd pfd = {.fd = fd, .events = POLLIN};
    int polled = poll(&pfd, 1, 1000) == 1 && (pfd.revents & POLLIN) && now_us() - start >= 15000;
    read(fd, &expirations, sizeof(expirations));
    printf("poll: %d\n", polled);
    ok &= polled;

    int ep = epoll_create1(0);
    struct epoll_event event = {.events = EPOLLIN, .data.fd = fd};
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &event);
    start = now_us();
    arm(fd, 15, 0);
    struct epoll_event ready;
    int epolled = epoll_wait(ep, &ready, 1, 1000) == 1 && ready.data.fd == fd &&
                  now_us() - start >= 15000;
    printf("epoll: %d\n", epolled);
    ok &= epolled;

    // A timer that was never ready while registered, so only its expiry's
    // notification can put it on the ready list.
    int fresh = timerfd_create(CLOCK_MONOTONIC, 0);
    int fresh_ep = epoll_create1(0);
    struct epoll_event fresh_event = {.events = EPOLLIN, .data.fd = fresh};
    epoll_ctl(fresh_ep, EPOLL_CTL_ADD, fresh, &fresh_event);
    arm(fresh, 20, 0);
    spin_us(60000);
    int nonblocking = epoll_wait(fresh_ep, &ready, 1, 0) == 1 && ready.data.fd == fresh;
    printf("epoll nonblocking: %d\n", nonblocking);
    ok &= nonblocking;
    return ok ? 0 : 1;
}
