# Handoff — recordings cutover (2026-09-01)

State at handoff: recordings repo committed through `e3533ab` (FS events,
verified live: 2 s detection with `--poll 3600`). John replaced my simple
`flake.nix` with a haskell.nix-based one (pushme style) — check `git status`
in `~/src/recordings`; that change (and this file) may be uncommitted. The
three requests below were interrupted by allocation limits; the `nix develop`
failure is DIAGNOSED (fix not yet applied). obr claim/create commands were
interrupted and did NOT run: `record-3tf` is still open/unclaimed, and no
issue exists yet for the flake fix.

## 1. Fix `nix develop` in ~/src/recordings — diagnosed; two fixes APPLIED

Issue A (the reported error, reproduced):

    error: Nixpkgs 26.11 has dropped support for x86_64-darwin.

Cause: `flake-utils.lib.eachDefaultSystem` enumerates `x86_64-darwin`,
and nixpkgs 26.11 (via the `haskellNix/nixpkgs-unstable` follow) now
*throws at import* for that system. APPLIED in flake.nix (uncommitted):
`eachSystem [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ]`. After
this, evaluation proceeds.

Issue B — SOLVED at the root (2026-09-01). The IOG cache was already
correctly configured system-side (`/etc/nix/nix.custom.conf`:
extra-substituters = https://cache.iog.io + hydra.iohk.io key;
`config/nix-trust.nix` defines and trusts it). The actual cause: the
hand-written `~/.config/nix/nix.conf` used bare `substituters =` /
`trusted-public-keys =` lines, which REPLACE the accumulated system
values and so wiped the IOG entries for every client invocation. Fixed by
switching those two lines to `extra-substituters` /
`extra-trusted-public-keys` (tron only; cache.nixos.org is a built-in
default) with a warning comment; backup at `~/.config/nix/nix.conf.bak`.
Verified: `nix config show` now lists cache.iog.io + hydra key, and the
devShell dry-run has ZERO ghc/bootstrap drvs both pinned and floating
(IOG serves aarch64-darwin GHC 9.10 toolchains — tested with
--override-input to the floating rev). The flake.lock pin to pushme's
haskellNix rev a695b259 is kept as an optimization (local store reuse),
no longer load-bearing. Optional follow-up: manage
`~/.config/nix/nix.conf` declaratively on hera in johnw.nix (today its
`xdg.configFile."nix/nix.conf"` block is mkIf isPositronRemoteLinux
only), so the clobber cannot regress. Tracked and closed: `record-r2g`.

VERIFIED at plan level: under the pin,
`nix build --dry-run .#devShells.aarch64-darwin.default` lists ZERO
ghc/bootstrap drvs — only recordings' own dependency libraries (tls,
crypton-x509*, sydtest's safe-coloured-text/autodocodec/opt-env-conf/
validity family, path, quickcheck-io, ...) and their haddocks. Remaining:
run `nix develop --command ghc --version` to actually build the shell
(one-time compile of those dep libs — expect tens of minutes), then
commit flake.nix + flake.lock (+ HANDOFF.md as appropriate).

Cosmetic wart, not a bug: `devShells.default = flake.devShells.default //
{ withHoogle = true; }` merges an attr onto a *derivation* and does
nothing — `withHoogle = true` already sits correctly inside `shell { }`.
Suggest reducing to `flake.devShells.default`. `nix flake check` will also
exercise the new fourmolu/hlint checks; sources were formatted and
hlint-clean as of `e3533ab`.

obr: create an issue for this fix (my attempt was interrupted), claim,
close when `nix develop` verified.

## 2. Update ~/src/nix — launchd block + recordings dependency

Claim `record-3tf` first (`obr update record-3tf --status=in_progress`,
in ~/src/recordings).

a. Replace the `flatten-recordings` service in
`~/src/nix/config/launchd.nix` with (this exact invocation is E2E-tested;
`transcribe` and `fswatch` are no longer used):

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

SSL_CERT_FILE deliberately dropped (local ASR + plain-HTTP LLM); re-add
`export SSL_CERT_FILE="${recordingCaBundle}"` only when switching to
`--asr-url https://hera.lan:8443/v1` (issue `record-2ov`, blocked on the
omlx cohere STT fix, `record-a84`).

b. Provide `pkgs.recordings`. Preferred wiring (avoids dragging haskell.nix
into the system config; builds against the system nixpkgs GHC): flake input
with `flake = false` + one overlay line

```nix
    recordings = final.haskellPackages.callCabal2nix "recordings" inputs.recordings { };
```

Follow the idiom of `~/src/nix/overlays/30-user-scripts.nix` (an overlay
parameterized on the input; `my-scripts` is the model — I had not yet read
how the `scripts` input is declared in `~/src/nix/flake.nix`, so mirror
that declaration). NOTE: ~/src/recordings has NO git remote yet — either
push to github:jwiegley/recordings first, or use a local input
(`git+file:///Users/johnw/src/recordings`) until it is published.
Everything referenced must be committed in the recordings repo for a flake
input to see it.

c. `darwin-rebuild switch` (or John's `switch` wrapper), then verify:
`tail -f ~/Library/Logs/flatten-recordings.log` should show
"watching ... -> ..." and "filesystem events active"; drop a test m4a into
`~/Recordings/<subdir>/` and expect transcription within seconds, the
cleaned `.txt` at the top of `~/Recordings`, audio archived to
`~/Documents/Recordings`. State/lock live in `~/.local/state/recordings`.

## 3. Stage (do not commit) old-script deletions in ~/src/scripts

```sh
cd ~/src/scripts && git rm flatten-recordings transcribe
```

Leave staged for John's review — he asked for staging only. Do NOT touch
`dispatch-agent-note` (open question `record-ct9`).

## 4. obr bookkeeping (AGENTS.md session protocol)

- Close `record-3tf` once the switch is built and the service verified.
- Close the new flake-fix issue once `nix develop` works.
- `obr sync --flush-only`; commit PLAN.org together with the code changes
  (recordings repo). The ~/src/nix and ~/src/scripts changes are John's
  repos — commit only if he asks.
- Still open besides those: `record-a84` (report omlx garbling upstream —
  full diagnosis in the issue), `record-2ov` (blocked on a84),
  `record-ct9`, `record-q5r` (org importer vs new `name.txt` naming).

## Facts the next session should not have to rediscover

- API key for omlx endpoints is literally `dummy-key`; hera serves
  `https://hera.lan:8443/v1` (private CA = ca-bundle-with-vulcan) fronting
  `http://localhost:8000/v1`. LLM model `GLM-5.3-Flash-oQ4e`.
- omlx `/v1/audio/transcriptions` garbles cohere-transcribe (server-side;
  details in `record-a84` and memory note `hera-omlx-endpoints`).
- The daemon feeds transcriber commands a 16 kHz mono WAV (afconvert); the
  mlx-speech CLI cannot read m4a (libsndfile), so the WAV hand-off is
  load-bearing.
- Test recording used throughout: any archived m4a from
  `~/Documents/Recordings`; expected transcript of `23-55-30.m4a` is
  "Set up a meeting between Nassim and the tax attorneys."
