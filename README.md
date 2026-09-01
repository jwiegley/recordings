# recordings

A small daemon that watches a directory tree for audio recordings and, for
each one: transcribes it, cleans the transcript with an LLM and a prompt
file, publishes the result as a `.txt` file, and archives the audio.

It replaces a 1225-line shell transaction system and a 591-line Python
transcriber with one ordering argument: **the audio file is the only source
of truth**, so the audio is never moved out of the watch tree until its
cleaned transcript has been durably published (write to temp, fsync, atomic
rename). A crash at any point leaves either the audio still in place — the
next sweep redoes the work, reusing the spooled raw transcript when the file
is unchanged — or the work complete. The rare crash exactly between
publishing the transcript and archiving the audio yields a visible duplicate
(`name-2.txt`) on the next sweep, never a loss.

## Usage

```sh
recordings ~/Recordings \
  --output ~/Recordings \
  --archive ~/Documents/Recordings \
  --prompt ~/doc/post-process.md \
  --asr-url https://hera.lan:8443/v1 \
  --asr-model cohere-transcribe-03-2026-mlx-fp16 \
  --llm-url http://localhost:8000/v1 \
  --llm-model GLM-5.3-Flash-oQ4e
```

- The watch tree is polled (default every 30 s, `--poll`); a file is
  processed only once its size and mtime are stable across two scans, so
  half-synced uploads are left alone. `--once` runs a single sweep.
- Transcription goes to an OpenAI-compatible `/audio/transcriptions`
  endpoint, uploading 16 kHz mono WAV converted with macOS-native
  `afconvert`. Until the server transcribes correctly, substitute a local
  command whose stdout is the transcript:
  `--asr-command "mlx-speech asr --model cohere-asr --audio"` (the audio
  path is appended as the final argument).
- Cleanup posts the raw transcript with the prompt file's contents as the
  system message to `/chat/completions` (`--llm-url`, `--llm-model`).
- `SSL_CERT_FILE`, when set, is the CA bundle used to validate TLS
  endpoints. `--api-key` (default `dummy-key`) is sent as a Bearer token.
- A recording that keeps failing is retried on later sweeps and abandoned
  after 5 attempts until its file changes or the daemon restarts; the audio
  always stays put on failure.
- Empty subdirectories of the watch tree (including `.DS_Store`-only ones)
  are pruned after each sweep. State lives in `~/.local/state/recordings`.

## Deployment (nix-darwin launchd)

The daemon subsumes the old fswatch pipeline; the service reduces to:

```nix
flatten-recordings = {
  script = ''
    export SSL_CERT_FILE="${recordingCaBundle}"
    exec ${recordings}/bin/recordings "${home}/Recordings" \
      --output "${home}/Recordings" \
      --archive "${home}/Documents/Recordings" \
      --prompt "${home}/doc/post-process.md" \
      --asr-url https://hera.lan:8443/v1 \
      --asr-model cohere-transcribe-03-2026-mlx-fp16 \
      --llm-url http://localhost:8000/v1 \
      --llm-model GLM-5.3-Flash-oQ4e
  '';
  serviceConfig = {
    RunAtLoad = true;
    KeepAlive = true;
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
nix develop          # cabal, HLS, hlint, fourmolu
cabal test           # inside the dev shell
```
