// Reference Olivia decoder: runs Pawel Jalocha's MFSK_Receiver — the same
// receiver fldigi uses — over a WAV file and prints what it copies. Used to
// compare decode yield against this crate on real recordings.
//
// Build & run:
//     g++ -O2 -std=c++17 -Ijalocha decode_harness.cc -o oliviadec
//     ./oliviadec recording.wav <tones> <bandwidth> [centre_hz]
//
// e.g. ./oliviadec sample.wav 8 500 1000

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cstdint>
#include <vector>

#include "pj_mfsk.h"

// Minimal 16-bit PCM mono WAV reader (enough for what this crate produces).
static bool read_wav(const char *path, std::vector<float> &out, int &rate)
{
	FILE *f = fopen(path, "rb");
	if (!f) return false;
	uint8_t hdr[12];
	if (fread(hdr, 1, 12, f) != 12 || memcmp(hdr, "RIFF", 4) || memcmp(hdr + 8, "WAVE", 4)) {
		fclose(f);
		return false;
	}
	int channels = 1, bits = 16;
	rate = 0;
	for (;;) {
		uint8_t ch[8];
		if (fread(ch, 1, 8, f) != 8) break;
		uint32_t sz = ch[4] | (ch[5] << 8) | (ch[6] << 16) | ((uint32_t)ch[7] << 24);
		if (!memcmp(ch, "fmt ", 4)) {
			std::vector<uint8_t> fmt(sz);
			if (fread(fmt.data(), 1, sz, f) != sz) break;
			channels = fmt[2] | (fmt[3] << 8);
			rate = fmt[4] | (fmt[5] << 8) | (fmt[6] << 16) | ((uint32_t)fmt[7] << 24);
			bits = fmt[14] | (fmt[15] << 8);
		} else if (!memcmp(ch, "data", 4)) {
			std::vector<uint8_t> data(sz);
			size_t got = fread(data.data(), 1, sz, f);
			size_t frames = got / (bits / 8) / channels;
			out.resize(frames);
			for (size_t i = 0; i < frames; i++) {
				const uint8_t *p = &data[i * channels * (bits / 8)];
				int16_t s = (int16_t)(p[0] | (p[1] << 8));
				out[i] = s / 32768.0f;
			}
			break;
		} else {
			fseek(f, sz + (sz & 1), SEEK_CUR);
		}
	}
	fclose(f);
	return rate != 0 && !out.empty();
}

int main(int argc, char **argv)
{
	if (argc < 4) {
		fprintf(stderr, "usage: %s recording.wav <tones> <bandwidth> [centre_hz]\n", argv[0]);
		return 2;
	}
	int tones = atoi(argv[2]);
	int bandwidth = atoi(argv[3]);
	double centre = argc > 4 ? atof(argv[4]) : 1000.0;

	std::vector<float> samples;
	int rate = 0;
	if (!read_wav(argv[1], samples, rate)) {
		fprintf(stderr, "cannot read %s\n", argv[1]);
		return 1;
	}
	fprintf(stderr, "%zu samples at %d Hz (%.1f s)\n", samples.size(), rate,
	        samples.size() / (double)rate);

	MFSK_Receiver<float> rx;
	rx.bContestia = false;
	rx.Tones = tones;
	rx.Bandwidth = bandwidth;
	rx.SampleRate = 8000;
	rx.InputSampleRate = rate;
	// fldigi's carrier convention: the tone block starts a fixed offset below
	// the tuning frequency, expressed in units of SymbolLen/16 FFT bins.
	double fc_offset = bandwidth * (1.0 - 0.5 / tones) / 2.0;
	rx.FirstCarrierMultiplier = (centre - fc_offset) / 500.0;
	rx.Reverse = 0;
	if (rx.Preset() < 0) {
		fprintf(stderr, "preset failed\n");
		return 1;
	}
	fputs(rx.PrintParameters(), stderr);

	// feed it in blocks, printing characters as they come out
	size_t chunk = 4096;
	size_t printed = 0;
	for (size_t i = 0; i < samples.size(); i += chunk) {
		size_t n = std::min(chunk, samples.size() - i);
		rx.Process(&samples[i], n);
		uint8_t c;
		while (rx.GetChar(c)) {
			if (c) putchar(c >= ' ' && c < 127 ? c : ' ');
			printed++;
		}
	}
	rx.Flush();
	uint8_t c;
	while (rx.GetChar(c)) {
		if (c) putchar(c >= ' ' && c < 127 ? c : ' ');
		printed++;
	}
	putchar('\n');
	fprintf(stderr, "%zu characters, final S/N %.1f\n", printed, rx.SignalToNoiseRatio());
	return 0;
}
