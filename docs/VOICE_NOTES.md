# Voice notes

Record from the microphone into the page you're on, and optionally turn
the recording into text **on your own computer** with
[whisper.cpp](https://github.com/ggml-org/whisper.cpp). Design notes:
decision 52 in `ARCHITECTURE.md`.

## Using it

- **Record voice note** (Ctrl+K palette, or the sidebar row) starts
  recording. A pill at the top right shows the time (at most 30 minutes)
  with **Stop** and **Cancel**. You can keep typing while it records.
- **Stop** (or the same command again) saves
  `assets/voice-<date>-<time>.wav` and adds a block
  `![voice note](../assets/voice-….wav)` below the block you were editing,
  or at the end of the page. **Cancel** deletes the recording.
- In reading view the note shows **▶ Play** (your system's audio player)
  and **Transcribe** (when a whisper program is set).
- The transcript is a child block of the note: `**Transcript:** …`.
  **Transcribe voice notes on page** transcribes every note on the page
  that has none yet.

## Setup on Arch Linux

### Recording

NoteSec uses the first recorder it finds on `PATH`:

| Program     | Package (Arch) |
|-------------|----------------|
| `pw-record` | `pipewire` (usually installed already) |
| `parecord`  | `libpulse` |
| `arecord`   | `alsa-utils` |
| `ffmpeg`    | `ffmpeg` |

```sh
sudo pacman -S --needed pipewire pipewire-pulse
```

It records the default input device. Pick it with `pavucontrol` or
`wpctl set-default <id>` (`wpctl status` lists them). Settings > Voice
notes shows the recorder it found.

To use another recorder or device, put a command in `config.toml`
(program first, then arguments, `{file}` where the WAV goes; no shell, so
no quoting or pipes). It must write 16 kHz mono 16-bit WAV and stop
cleanly on SIGINT:

```toml
voice_recorder = ["arecord", "-D", "plughw:1,0", "-f", "S16_LE", "-r", "16000", "-c", "1", "-t", "wav", "{file}"]
```

### Transcription (optional)

1. Install whisper.cpp: the AUR package `whisper.cpp` (or
   `whisper.cpp-cuda` / `-vulkan` for a GPU), or build it:

   ```sh
   git clone https://github.com/ggml-org/whisper.cpp
   cd whisper.cpp && cmake -B build && cmake --build build -j --config Release
   # the program: build/bin/whisper-cli
   ```

2. Download a model yourself. NoteSec never downloads anything. For
   example `ggml-base.bin` (~150 MB; `small` is better and slower) from
   the whisper.cpp model page, or with its script
   `models/download-ggml-model.sh base`.
3. In Settings > Voice notes, **Choose…** the program (`whisper-cli`) and
   the model, then press **Test**. It should say "Ready: …".
4. Optionally turn on **Transcribe new voice notes automatically**.

The language is detected automatically. To force one, set it in
`config.toml`, e.g. `whisper_language = "de"`. All voice settings:

```toml
voice_recorder = []            # empty: detect
whisper_binary = "/usr/bin/whisper-cli"
whisper_model = "/home/me/models/ggml-base.bin"
whisper_language = "auto"
voice_auto_transcribe = false
```

A transcription may take up to 2 minutes plus 6× the recording's length
before it's stopped. One runs at a time, and the rest wait.

## Privacy

- While recording, the audio goes to a folder only you can open
  (`$XDG_RUNTIME_DIR/notesec-voice/…`, or a private folder in `/tmp`),
  then moves into `assets/` with the same permissions as your other
  attachments. whisper's output is handled the same way.
- Audio is recorded and kept on this computer. NoteSec sends nothing to
  any service, and transcription runs only the program you chose.
- **We can't check what that program does.** NoteSec runs it with the
  recording's path and reads back its text. Use a whisper.cpp build you
  trust.
- Export to HTML and Publish leave voice notes out ("Voice note not
  included"); the WAV files are never copied into a published site.

## Size and backup

WAV is uncompressed: about **1.9 MB per minute** (a 30-minute note is
~58 MB). With git backup on, recordings in `assets/` are committed like
images and stay in the repository's history even after you delete them.
To keep the repository small, record short notes, or delete long ones
early and remember the history keeps them.

## Troubleshooting

- *"No recorder found"*: install one of the packages above.
- *"The recorder stopped by itself"*: the message ends with the
  recorder's own error (no device, busy device). Check the input in
  `pavucontrol`.
- *"Transcription failed (exit N): …"*: the end of whisper's own output
  is shown. A wrong or truncated model is the usual cause; **Test** says
  so.
- If NoteSec is closed while recording, the recording is saved in
  `assets/` without a block pointing at it.
