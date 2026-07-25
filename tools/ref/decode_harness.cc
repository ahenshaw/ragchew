// Offline driver for the reference JS8 decoder (fate): slide a 15 s window over
// a WAV file and run FT8::go() on each cycle, printing every decode.
#include <stdio.h>
#include <string>
#include <vector>
#include <algorithm>
#include <sys/time.h>
#include <thread>
#include <mutex>
#include <atomic>
#include <complex>
#include "util.h"
#include "js8.h"

extern std::string unpack(const int a87[87], std::string &other_call);

static double g_cycle_start = 0.0;

int my_cb(int *a87, double hz0, double /*hz1*/, double off,
          const char * /*comment*/, double snr, int pass, int /*correct_bits*/) {
  std::string oc;
  std::string txt = unpack(a87, oc);
  printf("t=%6.2f  hz=%6.1f  snr=%3.0f  pass=%d  %s\n",
         g_cycle_start + off, hz0, snr, pass, txt.c_str());
  fflush(stdout);
  return 2;
}

int main(int argc, char **argv) {
  if (argc < 2) { fprintf(stderr, "usage: decode_harness file.wav\n"); return 2; }
  int rate = 0;
  std::vector<double> all = readwav(argv[1], rate);
  fprintf(stderr, "read %d samples @ %d Hz (%.1f s)\n",
          (int)all.size(), rate, all.size() / (double)rate);

  int cyc = 15 * rate;
  int hints[1] = { 0 };
  for (size_t c = 0; c + rate <= all.size(); c += cyc) {
    size_t end = std::min(c + (size_t)cyc, all.size());
    std::vector<double> win(all.begin() + c, all.begin() + end);
    g_cycle_start = c / (double)rate;
    double dl = now() + 8.0, fdl = now() + 8.0;
    FT8 ft(win, 200, 2500, (int)(0.5 * rate), rate, hints, hints, dl, fdl, my_cb);
    ft.go();
  }
  return 0;
}
