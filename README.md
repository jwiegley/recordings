# recordings

`recordings` transcribes one or more audio files directly, or watches one
directory tree as a daemon. It cleans each transcript with an LLM, publishes a
`.txt` file, and optionally archives audio.

It replaces a 1225-line shell transaction system and a 591-line Python
transcriber with one ordering argument: **the audio file is the only source
of truth**, so the audio is never moved out of the watch tree until its
cleaned transcript has been durably published (write to temp, fsync, atomic
rename). A crash at any point leaves either the audio still in place — the
next sweep redoes the work, reusing the spooled raw transcript when the file
is unchanged — or the work complete. The rare crash exactly between
publishing the transcript and archiving the audio yields a visible duplicate
(`name-2.txt`) on the next sweep, never a loss; the first copy's audio link
then names an archive path its audio never reached, while its checksum still
identifies the audio archived as `name-2`.

## Usage

```sh
# Process files once; transcript goes in current directory and audio stays put.
recordings memo.m4a interview.wav \
  --prompt ~/doc/post-process.md \
  --asr-url https://hera.lan:8443/v1 \
  --asr-model cohere-transcribe-03-2026-mlx-fp16 \
  --llm-url http://localhost:8000/v1 \
  --llm-model GLM-5.3-Flash-oQ4e

# One directory remains daemon mode; archive is optional.
recordings ~/Recordings \
  --output ~/Documents/Inbox \
  --archive ~/Documents/Recordings \
  --prompt ~/doc/post-process.md \
  --asr-url https://hera.lan:8443/v1 \
  --asr-model cohere-transcribe-03-2026-mlx-fp16 \
  --llm-url http://localhost:8000/v1 \
  --llm-model GLM-5.3-Flash-oQ4e
```

- Supplying audio file paths processes that exact set then exits. Supplying one
  directory preserves watch mode. `--once` runs one directory sweep.
- `--output` defaults to current directory. `--archive` is optional; without it,
  successfully processed audio remains in place.
- Native filesystem events (FSEvents on macOS, inotify on Linux) start a
  sweep the moment the watch tree changes, with an interval poll (default
  30 s, `--poll`) as fallback; a file is processed only once its size and
  mtime are stable across two scans, so half-synced uploads are left alone.
- Audio is first converted to 16 kHz mono WAV (the model's native input) with
  macOS-native `afconvert`. Transcription uses an OpenAI-compatible
  `/audio/transcriptions` endpoint (`--asr-url`/`--asr-model`) or a local
  command whose stdout is transcript and receives WAV path last:
  `--asr-command "mlx-speech asr --model <model-dir> --language en --audio"`.
- Cleanup posts raw transcript with prompt file contents as system message to
  `/chat/completions` (`--llm-url`, `--llm-model`).
- Each transcript opens with an Org property drawer recording its source: a
  `file:` link to the audio where it ends up (the archive when `--archive` is
  given, `~/...` when under `$HOME`) and the first 256 bits of the audio's
  SHA-512. That is org-hash's `sha512_256`, not FIPS SHA-512/256, so
  `shasum -a 512 FILE | cut -c1-64` checks it. org-ext's import keeps both as
  properties of the Org entry:

  ```org
  :PROPERTIES:
  :RECORDING_AUDIO: [[file:~/Documents/Recordings/06-50-28.m4a]]
  :RECORDING_AUDIO_SHA512_256: 9e8074485a109345c19459495646bf87451b8c0f37dfb80b41d3035127e84b23
  :END:
  Cleaned transcript text...
  ```
- `SSL_CERT_FILE`, when set, is CA bundle used to validate TLS endpoints.
  `--api-key` (default `dummy-key`) is sent as Bearer token.
- In watch mode, a recording longer than 10 minutes (as macOS `afinfo`
  measures it) stays in place untranscribed, since Cohere transcribes long
  audio poorly. It is logged once, then ignored until file changes or daemon
  restarts. Naming it on the command line transcribes it anyway, but only
  with the daemon stopped, as one instance at a time holds the state
  directory.
- A recording that keeps failing is retried on later sweeps and abandoned after
  5 attempts until file changes or daemon restarts; audio stays put on failure.
- Empty subdirectories of watch tree (including `.DS_Store`-only ones) are
  pruned after each sweep, but only once untouched for 24 hours. The watch tree
  is typically a sync folder, where a directory that reads as empty locally may
  still be the destination of an upload in flight from another device.
  State lives in `~/.local/state/recordings`.
## Deployment (nix-darwin launchd)

The daemon subsumes the old fswatch pipeline; the service reduces to a
single exec (here with local mlx-speech transcription; swap the
`--asr-command` line for `--asr-url`/`--asr-model`, and export
`SSL_CERT_FILE`, once the server endpoint transcribes this model
correctly):

```nix
flatten-recordings = {
  script = ''
    # mlx-speech comes from the per-user profile; afconvert from macOS.
    export PATH="/etc/profiles/per-user/johnw/bin:$PATH"
    exec ${pkgs.recordings}/bin/recordings "${home}/Recordings" \
      --output "${home}/Recordings" \
      --archive "${home}/Documents/Recordings" \
      --prompt "${home}/doc/post-process.md" \
      --asr-command "mlx-speech asr --model ${home}/src/mlx-speech/models/cohere/cohere_transcribe/mlx-int8 --language en --audio" \
      --llm-url http://localhost:8000/v1 \
      --llm-model GLM-5.3-Flash-oQ4e
  '';
  serviceConfig = {
    RunAtLoad = true;
    KeepAlive = true; # long-running daemon; relaunch if it ever exits
    LowPriorityIO = false;
    LowPriorityBackgroundIO = false;
    ProcessType = "Standard";
    StandardOutPath = "${home}/Library/Logs/flatten-recordings.log";
    StandardErrorPath = "${home}/Library/Logs/flatten-recordings.log";
  };
};
```

## Building

```sh
nix build            # binary and test suite
nix develop          # cargo, rustc, rustfmt, clippy
cargo test           # inside the dev shell
```
