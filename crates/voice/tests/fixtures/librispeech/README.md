# Test speech

Four clips (6 s each, trimmed, mono, 16 kHz, 16-bit) from the LibriSpeech corpus, dev-clean, used
by `tests/echo_speech.rs`:

| File | Utterance | Speaker |
|---|---|---|
| `far-a.wav` | 2277-149896-0001 | 2277 |
| `far-b.wav` | 2277-149896-0000 | 2277 |
| `near-a.wav` | 2086-149214-0000 | 2086 |
| `near-b.wav` | 2086-149214-0001 | 2086 |

LibriSpeech ASR corpus, V. Panayotov, G. Chen, D. Povey and S. Khudanpur, "LibriSpeech: an ASR
corpus based on public domain audio books", ICASSP 2015. Licensed under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) (https://www.openslr.org/12). The clips
were cut to 6 seconds and converted from FLAC to WAV; nothing else was changed.
