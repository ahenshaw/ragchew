// Olivia FEC reference vectors, from Pawel Jalocha's MFSK implementation
// (the same code fldigi uses). Prints, for each mode and message:
//
//     bits_per_symbol|chars(hex)|symbols(64)|tones(64)|decoded(hex)
//
// where `symbols` are the FEC block's 64 symbol values, `tones` are those
// Gray-coded into carrier positions as the modulator sends them, and `decoded`
// is what the reference soft decoder recovers from an ideal receiver.
//
// Build & regenerate:
//     g++ -O2 -std=c++17 -Ijalocha harness.cc -o oliviaref
//     ./oliviaref > ../../../tests/vectors/olivia_vectors.txt

#include <cstdio>
#include <cstring>
#include <cstdint>

#include "pj_mfsk.h"

// The modes worth pinning down, by bits/symbol (4, 8, 16 and 32 tones). The FEC
// depends on nothing else about a mode — bandwidth only sets timing.
static const size_t kBitsPerSymbol[] = { 2, 3, 4, 5 };

static const char *kMessages[] = {
	"CQ CQ DE W1AW W1AW K",
	"ragchew 73 de olivia",
	"\x01\x02\x7f~} |{ZY",          // edge characters, including the top-bit half
	"",                             // all idle
};

int main()
{
	for (size_t m = 0; m < sizeof(kBitsPerSymbol) / sizeof(*kBitsPerSymbol); m++) {
		size_t bps = kBitsPerSymbol[m];

		MFSK_Encoder encoder;
		encoder.bContestia = false;
		encoder.BitsPerSymbol = bps;
		if (encoder.Preset() < 0) return 1;

		MFSK_SoftDecoder<float, float> decoder;
		decoder.bContestia = false;
		decoder.BitsPerSymbol = bps;
		if (decoder.Preset() < 0) return 1;

		for (size_t t = 0; t < sizeof(kMessages) / sizeof(*kMessages); t++) {
			const char *msg = kMessages[t];
			size_t len = strlen(msg);

			// one FEC block carries exactly BitsPerSymbol characters
			for (size_t off = 0; off < len || off == 0; off += bps) {
				uint8_t chars[8];
				for (size_t i = 0; i < bps; i++)
					chars[i] = (off + i < len) ? (uint8_t)msg[off + i] : 0;

				encoder.EncodeBlock(chars);

				printf("%d|", (int)bps);
				for (size_t i = 0; i < bps; i++) printf("%02x", chars[i] & 0x7f);
				printf("|");
				for (size_t i = 0; i < encoder.SymbolsPerBlock; i++)
					printf("%s%d", i ? " " : "", (int)encoder.OutputBlock[i]);
				printf("|");
				for (size_t i = 0; i < encoder.SymbolsPerBlock; i++)
					printf("%s%d", i ? " " : "", (int)GrayCode(encoder.OutputBlock[i]));

				// feed a perfect receiver: +1 for a 0 bit, -1 for a 1 bit
				decoder.Reset();
				for (size_t i = 0; i < encoder.SymbolsPerBlock; i++) {
					float soft[8];
					for (size_t b = 0; b < bps; b++)
						soft[b] = (encoder.OutputBlock[i] >> b) & 1 ? -1.0f : +1.0f;
					decoder.Input(soft);
				}
				decoder.Process();
				printf("|");
				for (size_t i = 0; i < bps; i++) printf("%02x", decoder.OutputBlock[i]);
				printf("\n");
			}
		}
	}
	return 0;
}
