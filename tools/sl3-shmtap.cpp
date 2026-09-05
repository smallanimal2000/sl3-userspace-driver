// sl3-shmtap — attach to the sl3d shared-memory ring and report liveness +
// capture throughput/level. Verifies the daemon<->plugin bridge without CoreAudio.
#include "sl3_shm.h"

#include <cmath>
#include <cstdio>
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <time.h>

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 2;

    int fd = shm_open(SL3_SHM_NAME, O_RDWR, 0666);
    if (fd < 0) { perror("shm_open (is sl3d running?)"); return 1; }
    sl3_shm *s = (sl3_shm *)mmap(NULL, sizeof(sl3_shm), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (s == MAP_FAILED) { perror("mmap"); return 1; }
    if (s->magic != SL3_SHM_MAGIC) { fprintf(stderr, "bad magic\n"); return 1; }

    printf("present=%u rate=%u channels=%u\n",
           s->device_present.load(), s->sample_rate.load(), s->channels);

    uint64_t start_w = s->cap_write.load();
    uint64_t start_pw = s->play_write.load();
    uint64_t start_pr = s->play_read.load();
    uint64_t start_hb = s->daemon_heartbeat.load();
    for (int t = 0; t < secs; t++) {
        struct timespec ts = { 1, 0 }; nanosleep(&ts, NULL);
        uint64_t w = s->cap_write.load();
        uint64_t pw = s->play_write.load(), pr = s->play_read.load();
        printf("     play_write +%llu  play_read +%llu  (backlog %lld)\n",
               (unsigned long long)(pw - start_pw), (unsigned long long)(pr - start_pr),
               (long long)(pw - pr));
        start_pw = pw; start_pr = pr;
        // RMS of the most recent ~4096 frames actually written
        uint64_t n = 4096; if (w < n) n = w;
        double sq[SL3_SHM_CHANNELS] = {0};
        for (uint64_t i = w - n; i < w; i++) {
            const float *f = &s->cap[(uint32_t)(i & SL3_RING_MASK) * SL3_SHM_CHANNELS];
            for (int c = 0; c < SL3_SHM_CHANNELS; c++) sq[c] += (double)f[c] * f[c];
        }
        uint64_t cr = s->cap_read.load();
        printf("[%d] cap_write=%llu (+%llu)  cap_read(plugin) backlog=%lld  hb=%llu  rms:",
               t, (unsigned long long)w, (unsigned long long)(w - start_w),
               (long long)(w - cr),
               (unsigned long long)(s->daemon_heartbeat.load() - start_hb));
        for (int c = 0; c < SL3_SHM_CHANNELS; c++)
            printf(" ch%d=%.4f", c + 1, n ? sqrt(sq[c] / n) : 0.0);
        printf("\n");
        start_w = w;
    }
    return 0;
}
