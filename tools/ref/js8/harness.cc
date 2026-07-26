//
// Cross-check harness around Robert Morris's reference JS8 code (fate).
// Links the reference pack/unpack/ldpc/crc and prints bit-exact
// intermediate values so we can validate the Rust implementation.
//
#include <stdio.h>
#include <string.h>
#include <string>
#include <vector>
#include <cmath>
#include <cstdint>

// ---- reference functions (external linkage in pack.cc/unpack.cc/libldpc.cc) ----
extern void pack_huffman(std::string text, int a72[72], int &consumed);
extern void setbits(int a87[87], int off, int n, unsigned long long x);
extern void ft8_crc(int msg1[], int msglen, int out[12]);
extern void ldpc_encode(int plain[87], int codeword[174]);
extern std::vector<double> pack_cq(std::string call, std::string grid, int rate, double hz);
extern std::vector<double> pack_directed(std::string my_call, std::string other_call,
                                         int cmd, int extra, int itype, int rate, double hz);
extern std::vector<double> fsk(std::vector<int> symbols, double hz, double spacing,
                               int rate, int symsamples);
extern std::string unpack(const int a87[87], std::string &other_call);

// costas + recode are defined in js8.cc which we don't link; provide them
// verbatim from js8.cc so pack.cc's pack_any() resolves.
int costas[] = { 4, 2, 5, 6, 1, 3, 0 };
std::vector<int> recode(int a174[]) {
  int i174 = 0;
  std::vector<int> out79;
  for (int i79 = 0; i79 < 79; i79++) {
    if (i79 < 7) out79.push_back(costas[i79]);
    else if (i79 >= 36 && i79 < 36 + 7) out79.push_back(costas[i79 - 36]);
    else if (i79 >= 72) out79.push_back(costas[i79 - 72]);
    else {
      int sym = (a174[i174 + 0] << 2) | (a174[i174 + 1] << 1) | (a174[i174 + 2] << 0);
      i174 += 3;
      out79.push_back(sym);
    }
  }
  return out79;
}

// pack.cc's test_pack references writewav; provide a real 16-bit PCM writer.
static void wr32(FILE *f, uint32_t v){ fputc(v&0xff,f);fputc((v>>8)&0xff,f);fputc((v>>16)&0xff,f);fputc((v>>24)&0xff,f); }
static void wr16(FILE *f, uint16_t v){ fputc(v&0xff,f);fputc((v>>8)&0xff,f); }
void writewav(const std::vector<double> &samples, const char *filename, int rate) {
  FILE *f = fopen(filename, "wb");
  if (!f) { perror(filename); return; }
  double mx = 1e-9;
  for (double s : samples) mx = std::max(mx, std::fabs(s));
  int n = samples.size();
  fwrite("RIFF", 1, 4, f); wr32(f, 36 + n*2); fwrite("WAVE", 1, 4, f);
  fwrite("fmt ", 1, 4, f); wr32(f, 16); wr16(f, 1); wr16(f, 1);
  wr32(f, rate); wr32(f, rate*2); wr16(f, 2); wr16(f, 16);
  fwrite("data", 1, 4, f); wr32(f, n*2);
  for (double s : samples) {
    int v = (int)lround((s / mx) * 20000.0);
    if (v > 32767) v = 32767; if (v < -32768) v = -32768;
    wr16(f, (uint16_t)(int16_t)v);
  }
  fclose(f);
}

static void print_frame(const char *kind, int itype, const std::string &text, int a87[87]) {
  int a174[174];
  ldpc_encode(a87, a174);
  int a79[79] = {0}; { int t[174]; memcpy(t,a174,sizeof t); auto v=recode(t); for(int i=0;i<79;i++)a79[i]=v[i]; }
  printf("%s|%d|%s|", kind, itype, text.c_str());
  for (int i = 0; i < 87; i++) putchar('0' + a87[i]);
  putchar('|');
  for (int i = 0; i < 174; i++) putchar('0' + a174[i]);
  putchar('|');
  for (int i = 0; i < 79; i++) printf("%s%d", i?" ":"", a79[i]);
  putchar('|');
  std::string other;
  printf("%s", unpack(a87, other).c_str());
  putchar('\n');
}

// reproduce pack.cc's pack_text() orchestration to expose a87
static void do_freetext(const std::string &text, int itype) {
  int a87[87]; memset(a87, 0, sizeof a87);
  int consumed;
  pack_huffman(text, a87, consumed);
  setbits(a87, 72, 3, itype);
  int crc[12]; ft8_crc(a87, 76, crc);
  for (int i = 0; i < 12; i++) a87[87 - 12 + i] = crc[i];
  print_frame("FREETEXT", itype, text, a87);
}

int main(int argc, char **argv) {
  // Deterministic set of frames covering the varicode + framing paths.
  const char *msgs[] = {
    "HELLO WORLD", "CQ", "TEST", "THE QUICK BROWN FOX",
    "73", "SNR -15", "A", "12345", "K1ABC/P", "", 0
  };
  for (int i = 0; msgs[i]; i++) do_freetext(msgs[i], 0);
  for (int i = 0; msgs[i]; i++) do_freetext(msgs[i], 3);

  // one WAV for the Rust decoder to chew on: freetext "HELLO WORLD" at 1500 Hz
  {
    int a87[87]; memset(a87,0,sizeof a87); int consumed;
    pack_huffman("HELLO WORLD", a87, consumed);
    setbits(a87, 72, 3, 0);
    int crc[12]; ft8_crc(a87, 76, crc);
    for (int i=0;i<12;i++) a87[87-12+i]=crc[i];
    int a174[174]; ldpc_encode(a87, a174);
    int t[174]; memcpy(t,a174,sizeof t); std::vector<int> a79=recode(t);
    std::vector<double> v = fsk(a79, 1500.0, 6.25, 12000, 1920);
    // pad to 15 s with leading 0.5 s silence like a real cycle
    std::vector<double> out(12000/2, 0.0);
    out.insert(out.end(), v.begin(), v.end());
    out.resize(15*12000, 0.0);
    writewav(out, (std::string(argv[1?0:0]),"hello_1500.wav"), 12000);
    fprintf(stderr, "wrote hello_1500.wav (%d symbols)\n", (int)a79.size());
  }
  return 0;
}
