// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// SIGALRM from alarm and setitimer: it ends pause() and runs the handler,
// arrives while the program makes syscalls, survives cancelling a timer that
// expired while the program made none, and kills a program without a handler
// (first argument "default"). See alarm.json.

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#include <x86intrin.h>

static volatile sig_atomic_t fired;

static void on_alarm(int signal) {
    (void)signal;
    fired = 1;
}

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

static void arm_ms(long ms) {
    struct itimerval timer = {.it_value = {0, ms * 1000}};
    setitimer(ITIMER_REAL, &timer, NULL);
}

int main(int argc, char *argv[]) {
    if (argc > 1 && strcmp(argv[1], "default") == 0) {
        alarm(1);
        pause();
        return 1;
    }
    struct sigaction action = {.sa_handler = on_alarm};
    sigaction(SIGALRM, &action, NULL);
    calibrate();

    long long start = now_us();
    arm_ms(30);
    int paused = pause() == -1 && errno == EINTR && fired && now_us() - start >= 30000;
    printf("pause: %d\n", paused);

    fired = 0;
    start = now_us();
    arm_ms(20);
    while (!fired) {
        getppid();
    }
    int busy = now_us() - start >= 20000;
    printf("syscalls: %d\n", busy);

    fired = 0;
    alarm(5);
    unsigned int remaining = alarm(0);
    int cancelled = remaining >= 4 && remaining <= 5 && !fired;
    printf("alarm: %d\n", cancelled);

    fired = 0;
    arm_ms(30);
    spin_us(200000);
    alarm(0);
    int kept = fired;
    printf("expired then cancelled: %d\n", kept);
    return paused && busy && cancelled && kept ? 0 : 1;
}
