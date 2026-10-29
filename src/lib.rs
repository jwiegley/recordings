//! Watch a directory tree for audio recordings; transcribe, clean, and
//! archive each one.
//!
//! Design premise keeping this small: audio file is sole source of truth;
//! every other artifact can be recomputed from it. Data safety therefore
//! reduces to one rule: audio never leaves watch tree until cleaned transcript
//! is durably published, plus atomic write-to-temp, fsync, rename publication.
//! A crash leaves either audio in place (next sweep retries, reusing unchanged
//! raw transcript) or work complete.

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use chrono::Local;
use clap::{ArgGroup, Parser};
use notify::{RecursiveMode, Watcher};
use reqwest::Certificate;
use reqwest::blocking::{Client, Response, multipart};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileTimes, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf, absolute};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUDIO_EXTENSIONS: &[&str] = &[
    "m4a", "mp3", "wav", "aac", "flac", "ogg", "opus", "aiff", "aif",
];
const MAX_ATTEMPTS: usize = 5;
/// Watch mode leaves a recording longer than this in place untranscribed:
/// Cohere transcribes long audio too poorly to be worth it. Naming the file
/// on the command line still transcribes it.
const MAX_DURATION: Duration = Duration::from_secs(10 * 60);
/// How long an empty directory must sit untouched before it is pruned. Long
/// enough that a recording still uploading from a phone cannot have its date
/// directory removed underneath it.
pub const PRUNE_MIN_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

/// Cheap file identity: size and modification time.
///
/// Files are processed only when identity is unchanged across two scans a
/// couple seconds apart, leaving half-synced uploads alone.
pub type Identity = (u64, SystemTime);

#[derive(Clone)]
pub enum Transcriber {
    /// OpenAI-compatible `/audio/transcriptions` endpoint: base URL and model.
    AsrHttp { base_url: String, model: String },
    /// Local command receiving a 16 kHz mono WAV path as its final argument.
    AsrShell(String),
}

#[derive(Clone)]
pub struct Options {
    pub watch_dir: Option<PathBuf>,
    pub audio_files: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub archive_dir: Option<PathBuf>,
    pub prompt_file: PathBuf,
    pub transcriber: Transcriber,
    pub llm_url: String,
    pub llm_model: String,
    pub api_key: String,
    pub language: String,
    pub poll_secs: i64,
    pub once: bool,
}

#[derive(Parser)]
#[command(
    name = "recordings",
    about = "Transcribe audio recordings as they arrive, clean the transcripts with an LLM, and archive the audio",
    group(ArgGroup::new("transcriber").required(true).args(["asr_url", "asr_command"]))
)]
struct Cli {
    /// Audio file(s) to process, or one directory tree to watch.
    #[arg(value_name = "PATH", num_args = 1..)]
    paths: Vec<PathBuf>,

    /// Directory receiving cleaned .txt transcripts. Defaults to current directory.
    #[arg(long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Directory receiving processed audio files. Omit to keep the audio files.
    #[arg(long, value_name = "DIR")]
    archive: Option<PathBuf>,

    /// Markdown file holding the fix-transcription prompt.
    #[arg(long, value_name = "FILE")]
    prompt: PathBuf,

    /// OpenAI-compatible base URL for transcription.
    #[arg(
        long,
        value_name = "URL",
        requires = "asr_model",
        conflicts_with = "asr_command"
    )]
    asr_url: Option<String>,

    /// Transcription model name.
    #[arg(
        long,
        value_name = "NAME",
        requires = "asr_url",
        conflicts_with = "asr_command"
    )]
    asr_model: Option<String>,

    /// Local transcription command; WAV path is appended and stdout is transcript.
    #[arg(long, value_name = "CMD", conflicts_with_all = ["asr_url", "asr_model"])]
    asr_command: Option<String>,

    /// OpenAI-compatible base URL for cleanup.
    #[arg(long, value_name = "URL")]
    llm_url: String,

    /// Model name for transcript cleanup.
    #[arg(long, value_name = "NAME")]
    llm_model: String,

    /// Bearer token for both endpoints.
    #[arg(long, value_name = "KEY", default_value = "dummy-key")]
    api_key: String,

    /// ISO 639-1 language for transcription.
    #[arg(long, value_name = "CODE", default_value = "en")]
    language: String,

    /// Seconds between sweeps.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    poll: i64,

    /// Run a single directory sweep and exit.
    #[arg(long)]
    once: bool,
}

impl Options {
    pub fn parse() -> Self {
        let cli = Cli::parse();
        let transcriber = match (cli.asr_url, cli.asr_model, cli.asr_command) {
            (Some(base_url), Some(model), None) => Transcriber::AsrHttp { base_url, model },
            (None, None, Some(command)) => Transcriber::AsrShell(command),
            _ => unreachable!("transcriber arguments are validated by clap"),
        };
        let watch_dir =
            (cli.paths.len() == 1 && cli.paths[0].is_dir()).then(|| cli.paths[0].clone());
        let audio_files = if watch_dir.is_some() {
            Vec::new()
        } else {
            cli.paths
        };
        Self {
            watch_dir,
            audio_files,
            output_dir: cli.output,
            archive_dir: cli.archive,
            prompt_file: cli.prompt,
            transcriber,
            llm_url: cli.llm_url,
            llm_model: cli.llm_model,
            api_key: cli.api_key,
            language: cli.language,
            poll_secs: cli.poll,
            once: cli.once,
        }
    }
}

/// Whether a filename has a supported audio extension and is not hidden.
fn supported_audio_extension(extension: &OsStr) -> bool {
    #[cfg(unix)]
    {
        let extension = extension.as_bytes();
        AUDIO_EXTENSIONS
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate.as_bytes()))
    }
    #[cfg(not(unix))]
    {
        extension.to_str().is_some_and(|extension| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
    }
}

fn is_audio_os_name(name: &OsStr) -> bool {
    !hidden(name)
        && Path::new(name)
            .extension()
            .is_some_and(supported_audio_extension)
}

pub fn is_audio_name(name: &str) -> bool {
    is_audio_os_name(OsStr::new(name))
}

/// Files present in both scans with unchanged identity, in stable order.
pub fn ready_files(
    before: &BTreeMap<PathBuf, Identity>,
    after: &BTreeMap<PathBuf, Identity>,
) -> Vec<PathBuf> {
    after
        .iter()
        .filter(|(path, identity)| before.get(*path) == Some(*identity))
        .map(|(path, _)| path.clone())
        .collect()
}

/// Encode a watch-relative path injectively as one flat filename component.
pub fn encode_safe(path: &str) -> String {
    path.replace('%', "%25").replace('/', "%2F")
}

fn unix_seconds_floor(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i128,
        Err(error) => {
            let duration = error.duration();
            -(duration.as_secs() as i128) - i128::from(duration.subsec_nanos() != 0)
        }
    }
}

/// Raw-transcript spool name bound to audio path and identity.
///
/// A crash after transcription leaves this file behind; next sweep reuses it
/// when identity is unchanged, while changed audio misses the cache.
pub fn temp_name(relative: &str, identity: &Identity) -> String {
    format!(
        "{}.{}.{}.raw.txt",
        encode_safe(relative),
        identity.0,
        unix_seconds_floor(identity.1)
    )
}

fn spool_name(relative: &Path, identity: &Identity) -> OsString {
    #[cfg(unix)]
    {
        let mut encoded = Vec::with_capacity(relative.as_os_str().as_bytes().len());
        for byte in relative.as_os_str().as_bytes() {
            match byte {
                b'/' => encoded.extend_from_slice(b"%2F"),
                b'%' => encoded.extend_from_slice(b"%25"),
                _ => encoded.push(*byte),
            }
        }
        let mut name = OsString::from_vec(encoded);
        name.push(format!(
            ".{}.{}.raw.txt",
            identity.0,
            unix_seconds_floor(identity.1)
        ));
        name
    }
    #[cfg(not(unix))]
    {
        OsString::from(temp_name(&relative.to_string_lossy(), identity))
    }
}

/// Candidate publication stems: `x`, `x-2`, `x-3`, ...
pub fn target_stems(stem: &str) -> impl Iterator<Item = String> {
    let stem = stem.to_owned();
    std::iter::once(stem.clone()).chain((2_u64..).map(move |n| format!("{stem}-{n}")))
}

/// Keep both ends of long messages because HTTP errors often put cause last.
pub fn excerpt_ends(limit: usize, text: &str) -> String {
    let len = text.chars().count();
    if len <= 2 * limit + 3 {
        text.to_owned()
    } else {
        format!(
            "{} … {}",
            text.chars().take(limit).collect::<String>(),
            text.chars().skip(len - limit).collect::<String>()
        )
    }
}

/// Whether normalized absolute `child` is equal to or beneath `parent`.
pub fn is_inside(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

pub fn join_url(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

/// Cleanup output is roughly input-sized, so half character count is about
/// double expected tokens; bounds keep runaway generation finite.
pub fn max_tokens_for(text: &str) -> usize {
    (text.chars().count() / 2).clamp(2_048, 32_768)
}

pub fn chat_request_value(model: &str, prompt: &str, transcript: &str) -> Value {
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": prompt},
            {"role": "user", "content": transcript}
        ],
        "temperature": 0.3,
        "max_tokens": max_tokens_for(transcript),
        "stream": false,
        "chat_template_kwargs": {"enable_thinking": false}
    })
}

pub fn decode_asr_text(body: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(body)?;
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing string field `text` in transcription"))
}

pub fn decode_chat_text(body: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(body)?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array field `choices` in completion"))?
        .first()
        .ok_or_else(|| anyhow!("no choices in completion"))?;
    choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing string field `message.content` in completion"))
}

/// Duration from the `estimated duration: 660.000 sec` line of `afinfo -r`.
pub fn afinfo_duration(output: &str) -> Option<Duration> {
    output.lines().find_map(|line| {
        let seconds = line
            .strip_prefix("estimated duration:")?
            .strip_suffix("sec")?
            .trim();
        Duration::try_from_secs_f64(seconds.parse().ok()?).ok()
    })
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

/// Write through a sibling temp file, fsync file and parent, and atomically rename.
///
/// ponytail: plain fsync, not F_FULLFSYNC; on a journaled filesystem this
/// covers crash safety short of power loss mid-barrier.
pub fn write_file_durable(path: &Path, content: &str) -> io::Result<()> {
    let temporary = sibling_with_suffix(path, ".tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    sync_parent(path)
}

/// Move a file, falling back to copy-verify-delete across filesystems.
/// Source is removed only after destination has same size.
pub fn move_path(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::rename(source, destination).is_ok() {
        sync_parent(destination)?;
        if source.parent() != destination.parent() {
            sync_parent(source)?;
        }
        return Ok(());
    }

    let metadata = fs::metadata(source)?;
    fs::copy(source, destination)?;
    let destination_file = OpenOptions::new().write(true).open(destination)?;
    destination_file.set_permissions(metadata.permissions())?;
    let mut times = FileTimes::new();
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
    }
    destination_file.set_times(times)?;

    let copied_size = destination_file.metadata()?.len();
    if copied_size != metadata.len() {
        return Err(io::Error::other(format!(
            "copy of {} to {} has wrong size",
            source.display(),
            destination.display()
        )));
    }
    destination_file.sync_all()?;
    drop(destination_file);
    sync_parent(destination)?;
    fs::remove_file(source)?;
    sync_parent(source)
}

/// First stem whose transcript and optional archive destinations are free.
///
/// ponytail: existence-check then create; process lock makes this process sole
/// writer, so no uniquification race exists.
pub fn fresh_targets(
    output_dir: &Path,
    archive_dir: Option<&Path>,
    audio: &Path,
) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let stem = audio
        .file_stem()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audio has no file stem"))?;
    let extension = audio.extension();

    for index in 1_u64.. {
        let mut candidate = stem.to_os_string();
        if index > 1 {
            candidate.push(format!("-{index}"));
        }
        let mut transcript_name = candidate.clone();
        transcript_name.push(".txt");
        let transcript = output_dir.join(transcript_name);

        let archive = archive_dir.map(|archive_dir| {
            let mut archive_name = candidate;
            if let Some(extension) = extension {
                archive_name.push(".");
                archive_name.push(extension);
            }
            archive_dir.join(archive_name)
        });
        if !transcript.try_exists()?
            && archive
                .as_ref()
                .map_or(Ok(true), |path| path.try_exists().map(|exists| !exists))?
        {
            return Ok((transcript, archive));
        }
    }
    unreachable!("target stem sequence is infinite")
}

/// First 256 bits of a file's SHA-512 digest, as 64 lowercase hex digits.
///
/// This is `sha512_256` in org-hash's sense: truncated SHA-512, not the
/// distinct FIPS SHA-512/256 algorithm. `shasum -a 512 FILE | cut -c1-64`
/// reproduces it.
pub fn audio_checksum(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut context = digest::Context::new(&digest::SHA512);
    let mut buffer = vec![0; 1 << 16];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => context.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(context.finish().as_ref()[..32]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Backslash-escape a link as Org's `org-link-escape` does: `[` and `]` gain
/// a backslash, and backslashes before them or at the end are doubled.
fn org_link_escape(link: &str) -> String {
    let mut escaped = String::with_capacity(link.len());
    let mut backslashes = 0;
    for character in link.chars() {
        match character {
            '\\' => backslashes += 1,
            '[' | ']' => {
                escaped.push_str(&"\\".repeat(2 * backslashes + 1));
                escaped.push(character);
                backslashes = 0;
            }
            _ => {
                escaped.push_str(&"\\".repeat(backslashes));
                escaped.push(character);
                backslashes = 0;
            }
        }
    }
    escaped.push_str(&"\\".repeat(2 * backslashes));
    escaped
}

/// Org property drawer heading a transcript with the audio it came from.
///
/// org-ext carries these properties onto the Org entry it imports, keeping a
/// link to the recording and the `audio_checksum` that shows the file is
/// unchanged. Audio under `home` is linked as `~/...`, unless `home` is the
/// root, which, as with Emacs's `abbreviate-file-name`, abbreviates nothing.
pub fn provenance_drawer(audio: &Path, checksum: &str, home: Option<&Path>) -> String {
    let home = home.filter(|home| home.parent().is_some());
    let path = match home.and_then(|home| audio.strip_prefix(home).ok()) {
        Some(relative) => Path::new("~").join(relative),
        None => audio.to_owned(),
    };
    format!(
        ":PROPERTIES:\n\
         :RECORDING_AUDIO: [[file:{}]]\n\
         :RECORDING_AUDIO_SHA512_256: {checksum}\n\
         :END:\n",
        org_link_escape(&path.to_string_lossy())
    )
}

/// Publish the transcript, headed by the provenance of the audio at its final
/// location, then archive the audio when an archive directory is given.
fn publish_cleaned(
    output_dir: &Path,
    archive_dir: Option<&Path>,
    audio: &Path,
    cleaned: &str,
    checksum: &str,
    home: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let (transcript, archive) = fresh_targets(output_dir, archive_dir, audio)?;
    let drawer = provenance_drawer(archive.as_deref().unwrap_or(audio), checksum, home);
    write_file_durable(&transcript, &format!("{drawer}{}\n", cleaned.trim_end()))?;
    if let Some(archive) = &archive {
        move_path(audio, archive)?;
    }
    Ok((transcript, archive))
}

fn hidden(name: &OsStr) -> bool {
    #[cfg(unix)]
    {
        name.as_bytes().first() == Some(&b'.')
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().starts_with('.')
    }
}

/// Remove non-root subdirectories empty except for `.DS_Store` files, once
/// they have gone `min_idle` without being touched.
/// Hidden directories and symlinks are left alone.
///
/// The delay matters because the watch tree is a sync folder: a directory that
/// reads as empty here may still be the destination of an upload in flight
/// from another device, and removing it would delete that destination out from
/// under the upload.
pub fn prune_empty_dirs(root: &Path, min_idle: Duration) {
    fn prune(directory: &Path, is_root: bool, min_idle: Duration, now: SystemTime) -> bool {
        // Sample before recursing: pruning a child bumps this directory's
        // mtime, which would otherwise hide how long it has really been idle.
        let modified = fs::metadata(directory).and_then(|metadata| metadata.modified());
        let Ok(entries) = fs::read_dir(directory) else {
            return false;
        };
        let mut remaining = Vec::<OsString>::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let name = entry.file_name();
            let path = entry.path();
            let is_symlink = fs::symlink_metadata(&path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(true);
            if !is_symlink && path.is_dir() && !hidden(&name) {
                if !prune(&path, false, min_idle, now) {
                    remaining.push(name);
                }
            } else {
                remaining.push(name);
            }
        }

        // An unreadable or future-dated mtime counts as "not idle yet", so a
        // surprise always costs tidiness rather than data.
        let idle_enough = modified
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|idle| idle >= min_idle);

        if !is_root
            && idle_enough
            && remaining
                .iter()
                .all(|name| name.as_os_str() == OsStr::new(".DS_Store"))
        {
            for name in remaining {
                let _ = fs::remove_file(directory.join(name));
            }
            fs::remove_dir(directory).is_ok()
        } else {
            false
        }
    }

    prune(root, true, min_idle, SystemTime::now());
}

struct Env {
    opts: Options,
    state_dir: PathBuf,
    /// Audio links below this directory are written relative to `~`.
    home: Option<PathBuf>,
    client: Client,
}

/// What sweeps have concluded about a file, standing while its identity holds.
enum Verdict {
    /// Processing has failed this many times running.
    Failed(usize),
    /// The recording runs past `MAX_DURATION`, so it stays put untranscribed.
    TooLong,
}

type Verdicts = BTreeMap<PathBuf, (Identity, Verdict)>;

fn log_msg(message: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "{} {message}",
        Local::now().format("%Y-%m-%dT%H:%M:%S%z")
    );
    let _ = stdout.flush();
}

fn load_ca_bundle() -> Result<Option<Vec<Certificate>>> {
    let Some(path) = env::var_os("SSL_CERT_FILE") else {
        return Ok(None);
    };
    let bytes = fs::read(&path)
        .with_context(|| format!("cannot read SSL_CERT_FILE {}", Path::new(&path).display()))?;
    let certificates = Certificate::from_pem_bundle(&bytes).with_context(|| {
        format!(
            "SSL_CERT_FILE has no usable certificates: {}",
            Path::new(&path).display()
        )
    })?;
    if certificates.is_empty() {
        bail!(
            "SSL_CERT_FILE has no usable certificates: {}",
            Path::new(&path).display()
        );
    }
    Ok(Some(certificates))
}

fn new_http_client() -> Result<Client> {
    let builder = Client::builder();
    let builder = match load_ca_bundle()? {
        Some(certificates) => builder.tls_certs_only(certificates),
        None => builder,
    };
    builder.build().context("cannot initialize HTTP client")
}

fn expect_ok(what: &str, response: Response) -> Result<Vec<u8>> {
    let status = response.status();
    let body = response.bytes()?.to_vec();
    if !status.is_success() {
        let excerpt: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        bail!("{what} failed with HTTP {}: {excerpt}", status.as_u16());
    }
    Ok(body)
}

fn first_chars(limit: usize, bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().take(limit).collect()
}

/// A recording's duration, as macOS's `afinfo` counts it from the file's
/// packets, when it runs past `MAX_DURATION`.
fn too_long(audio: &Path) -> Result<Option<Duration>> {
    let output = Command::new("afinfo")
        // Without -r, afinfo extrapolates the duration of a file lacking a
        // packet table, like raw AAC or VBR MP3, from its first packets.
        .arg("-r")
        .arg(audio)
        .output()
        .with_context(|| format!("cannot run afinfo on {}", audio.display()))?;
    if !output.status.success() {
        bail!(
            "afinfo failed on {}: {}",
            audio.display(),
            first_chars(300, &output.stderr)
        );
    }
    let duration = afinfo_duration(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| anyhow!("afinfo reported no duration for {}", audio.display()))?;
    Ok((duration > MAX_DURATION).then_some(duration))
}

/// Transcribe one audio file after conversion to model-native 16 kHz mono WAV.
fn run_asr(env: &Env, audio: &Path) -> Result<String> {
    let wav = env.state_dir.join("convert.wav");
    remove_quietly(&wav);
    let output = Command::new("afconvert")
        .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
        .arg(audio)
        .arg(&wav)
        .output()
        .with_context(|| format!("cannot run afconvert on {}", audio.display()))?;
    if !output.status.success() {
        bail!(
            "afconvert failed on {}: {}",
            audio.display(),
            first_chars(300, &output.stderr)
        );
    }
    let result = transcribe_wav(env, &wav);
    remove_quietly(&wav);
    result
}

fn transcribe_wav(env: &Env, wav: &Path) -> Result<String> {
    match &env.opts.transcriber {
        Transcriber::AsrShell(command_line) => {
            let mut words = command_line.split_whitespace();
            let Some(program) = words.next() else {
                bail!("--asr-command is empty");
            };
            let output = Command::new(program)
                .args(words)
                .arg(wav)
                .output()
                .with_context(|| format!("cannot run {program}"))?;
            if !output.status.success() {
                let status = output
                    .status
                    .code()
                    .map_or_else(|| "by signal".to_owned(), |code| code.to_string());
                bail!(
                    "{program} exited {status}: {}",
                    first_chars(300, &output.stderr)
                );
            }
            String::from_utf8(output.stdout)
                .with_context(|| format!("{program} emitted non-UTF-8 stdout"))
        }
        Transcriber::AsrHttp { base_url, model } => {
            let form = multipart::Form::new()
                .text("model", model.clone())
                .text("language", env.opts.language.clone())
                .file("file", wav)?;
            let response = env
                .client
                .post(join_url(base_url, "audio/transcriptions"))
                .bearer_auth(&env.opts.api_key)
                .timeout(Duration::from_secs(3_600))
                .multipart(form)
                .send()?;
            let body = expect_ok("transcription", response)?;
            decode_asr_text(&body).context("bad transcription response")
        }
    }
}

/// Clean a raw transcript through the LLM using prompt file contents.
fn run_llm(env: &Env, transcript: &str) -> Result<String> {
    let prompt = fs::read_to_string(&env.opts.prompt_file)
        .with_context(|| format!("cannot read prompt file {}", env.opts.prompt_file.display()))?;
    let request = chat_request_value(&env.opts.llm_model, prompt.trim(), transcript);
    let response = env
        .client
        .post(join_url(&env.opts.llm_url, "chat/completions"))
        .bearer_auth(&env.opts.api_key)
        .timeout(Duration::from_secs(600))
        .json(&request)
        .send()?;
    let body = expect_ok("cleanup", response)?;
    decode_chat_text(&body).context("bad completion response")
}

fn file_identity(path: &Path) -> Option<Identity> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path);
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// Per-file crash-safe pipeline:
///
/// 1. spool raw transcript durably before cleanup;
/// 2. publish cleaned transcript, headed by the audio's final location and
///    checksum, durably before touching audio;
/// 3. optionally move audio to archive, then remove spool.
///
/// ponytail: crash exactly between 2 and 3 republishes under `name-2` on next
/// sweep: visible duplicate, never loss. The first copy's audio link then
/// names an archive path the audio never reached, which a later recording with
/// the same stem may take; its checksum still identifies the audio, now
/// archived as `name-2`. A failed move has the same effect.
fn process_file(env: &Env, audio: &Path, identity: &Identity) -> Result<()> {
    let relative = match &env.opts.watch_dir {
        Some(watch_dir) => audio
            .strip_prefix(watch_dir)
            .with_context(|| format!("{} is outside watch tree", audio.display()))?,
        None => audio,
    };
    let spool = env.state_dir.join(spool_name(relative, identity));

    let raw = if spool.is_file() {
        log_msg(&format!(
            "reusing spooled transcript for {}",
            relative.display()
        ));
        fs::read_to_string(&spool)
            .with_context(|| format!("cannot read spool {}", spool.display()))?
    } else {
        log_msg(&format!("transcribing {}", relative.display()));
        let transcript = run_asr(env, audio)?;
        if blank(&transcript) {
            bail!("empty transcription for {}", relative.display());
        }
        write_file_durable(&spool, &transcript)
            .with_context(|| format!("cannot publish spool {}", spool.display()))?;
        transcript
    };

    log_msg(&format!("cleaning {}", relative.display()));
    let cleaned = run_llm(env, &raw)?;
    if blank(&cleaned) {
        bail!("empty cleanup result for {}", relative.display());
    }
    // Checksum before the identity check, so the digest is of the very file
    // that was transcribed.
    let checksum =
        audio_checksum(audio).with_context(|| format!("cannot checksum {}", relative.display()))?;
    if file_identity(audio) != Some(*identity) {
        bail!("{} changed while being processed", relative.display());
    }

    let (transcript, archive) = publish_cleaned(
        env.opts
            .output_dir
            .as_deref()
            .expect("output directory is set"),
        env.opts.archive_dir.as_deref(),
        audio,
        &cleaned,
        &checksum,
        env.home.as_deref(),
    )?;
    remove_quietly(&spool);
    let transcript = transcript
        .file_name()
        .unwrap_or(transcript.as_os_str())
        .to_string_lossy();
    match archive {
        Some(archive) => log_msg(&format!(
            "published {transcript}; archived {}",
            archive
                .file_name()
                .unwrap_or(archive.as_os_str())
                .to_string_lossy()
        )),
        None => log_msg(&format!(
            "published {transcript}; kept {}",
            relative.display()
        )),
    }
    Ok(())
}

fn scan_audio(root: &Path) -> BTreeMap<PathBuf, Identity> {
    fn walk(directory: &Path, files: &mut BTreeMap<PathBuf, Identity>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if hidden(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk(&path, files);
            } else if is_audio_os_name(&name)
                && let Some(identity) = file_identity(&path)
            {
                files.insert(path, identity);
            }
        }
    }

    let mut files = BTreeMap::new();
    walk(root, &mut files);
    files
}

fn compact_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn step_file(env: &Env, scan: &BTreeMap<PathBuf, Identity>, verdicts: &mut Verdicts, audio: &Path) {
    let Some(identity) = scan.get(audio).copied() else {
        return;
    };
    let attempts = match verdicts.get(audio) {
        Some((seen, verdict)) if *seen == identity => match verdict {
            Verdict::TooLong => return,
            Verdict::Failed(attempts) if *attempts >= MAX_ATTEMPTS => return,
            Verdict::Failed(attempts) => *attempts,
        },
        _ => 0,
    };

    let result = match too_long(audio) {
        Ok(Some(duration)) => {
            // Rounded up, so a recording just past the limit never reads as within it.
            let seconds = duration.as_secs_f64().ceil() as u64;
            log_msg(&format!(
                "leaving {} in place untranscribed: it runs {}:{:02}, over the {}-minute limit",
                audio.display(),
                seconds / 60,
                seconds % 60,
                MAX_DURATION.as_secs() / 60
            ));
            verdicts.insert(audio.to_owned(), (identity, Verdict::TooLong));
            return;
        }
        Ok(None) => process_file(env, audio, &identity),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            verdicts.remove(audio);
        }
        Err(error) => {
            let attempts = attempts + 1;
            log_msg(&format!(
                "ERROR processing {}: {}",
                audio.display(),
                excerpt_ends(400, &compact_error(&error))
            ));
            if attempts >= MAX_ATTEMPTS {
                log_msg(&format!(
                    "giving up on {} until it changes ({attempts} attempts)",
                    audio.display()
                ));
            }
            verdicts.insert(audio.to_owned(), (identity, Verdict::Failed(attempts)));
        }
    }
}

fn sweep(env: &Env, verdicts: &mut Verdicts) {
    let watch_dir = env
        .opts
        .watch_dir
        .as_deref()
        .expect("sweep only runs in watch mode");
    let before = scan_audio(watch_dir);
    if !before.is_empty() {
        thread::sleep(Duration::from_secs(2));
    }
    let after = scan_audio(watch_dir);
    for audio in ready_files(&before, &after) {
        step_file(env, &after, verdicts, &audio);
    }
    prune_empty_dirs(watch_dir, PRUNE_MIN_IDLE);
    verdicts.retain(|path, _| after.contains_key(path));
}

fn state_dir() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(directory).join("recordings"));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/state/recordings"))
}

fn canonicalize_if_exists(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_owned()),
        result => result,
    }
}

fn start_watcher(watch: &Path, sender: SyncSender<()>) -> Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = sender.try_send(());
        }
    })?;
    watcher.watch(watch, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn wait_for_kick(receiver: &Receiver<()>, timeout: Duration, watching: bool) {
    if watching {
        match receiver.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(timeout),
        }
    } else {
        thread::sleep(timeout);
    }
}

pub fn run(mut options: Options) -> Result<()> {
    options.watch_dir = options.watch_dir.map(|path| absolute(&path)).transpose()?;
    options.audio_files = options
        .audio_files
        .into_iter()
        .map(|path| absolute(&path))
        .collect::<io::Result<_>>()?;
    options.output_dir = Some(absolute(options.output_dir.unwrap_or(env::current_dir()?))?);
    options.archive_dir = options
        .archive_dir
        .map(|path| absolute(&path))
        .transpose()?;
    let state_dir = state_dir()?;

    fs::create_dir_all(&state_dir)?;
    fs::create_dir_all(
        options
            .output_dir
            .as_ref()
            .expect("output directory is set"),
    )?;
    if let Some(archive_dir) = &options.archive_dir {
        fs::create_dir_all(archive_dir)?;
    }
    if let Some(watch_dir) = &options.watch_dir {
        let checked_watch = canonicalize_if_exists(watch_dir)
            .with_context(|| format!("cannot resolve watch tree {}", watch_dir.display()))?;
        for (name, directory) in std::iter::once(("state", &state_dir)).chain(
            options
                .archive_dir
                .as_ref()
                .map(|directory| ("--archive", directory)),
        ) {
            let checked_directory = fs::canonicalize(directory).with_context(|| {
                format!("cannot resolve {name} directory {}", directory.display())
            })?;
            if is_inside(&checked_watch, &checked_directory) {
                bail!(
                    "{name} directory {} lies inside watch tree {}",
                    directory.display(),
                    watch_dir.display()
                );
            }
        }
    }
    if !options.prompt_file.is_file() {
        log_msg(&format!(
            "WARN prompt file missing (will fail per file until it appears): {}",
            options.prompt_file.display()
        ));
    }

    let lock = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(state_dir.join("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another instance holds the lock; exiting"),
        Err(TryLockError::Error(error)) => {
            return Err(error).context("cannot lock state directory");
        }
    }

    let env = Env {
        opts: options,
        state_dir,
        home: env::var_os("HOME").map(PathBuf::from),
        client: new_http_client()?,
    };
    if env.opts.watch_dir.is_none() {
        let mut failed = 0;
        for audio in &env.opts.audio_files {
            if !audio.is_file() || !audio.file_name().is_some_and(is_audio_os_name) {
                bail!("not a supported audio file: {}", audio.display());
            }
            let identity = file_identity(audio)
                .ok_or_else(|| anyhow!("cannot read audio file {}", audio.display()))?;
            if let Err(error) = process_file(&env, audio, &identity) {
                failed += 1;
                log_msg(&format!("ERROR processing {}: {error:#}", audio.display()));
            }
        }
        if failed != 0 {
            bail!("{failed} audio file(s) failed");
        }
        return Ok(());
    }

    let watch_dir = env
        .opts
        .watch_dir
        .as_deref()
        .expect("watch directory is set");
    log_msg(&format!(
        "watching {} -> {} ({})",
        watch_dir.display(),
        env.opts
            .output_dir
            .as_ref()
            .expect("output directory is set")
            .display(),
        env.opts.archive_dir.as_ref().map_or_else(
            || "keeping audio".to_owned(),
            |path| format!("archive {}", path.display())
        )
    ));

    let mut verdicts = BTreeMap::new();
    if env.opts.once {
        sweep(&env, &mut verdicts);
        return Ok(());
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    let watcher = match start_watcher(watch_dir, sender) {
        Ok(watcher) => {
            log_msg("filesystem events active");
            Some(watcher)
        }
        Err(error) => {
            let error = compact_error(&error);
            log_msg(&format!(
                "WARN filesystem events unavailable, polling only: {}",
                excerpt_ends(200, &error)
            ));
            None
        }
    };
    let poll = Duration::from_secs(env.opts.poll_secs.max(1) as u64);
    loop {
        sweep(&env, &mut verdicts);
        wait_for_kick(&receiver, poll, watcher.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_without_archive_keeps_audio_and_links_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.m4a");
        let output = dir.path().join("transcripts");
        fs::write(&audio, "audio").unwrap();
        fs::create_dir(&output).unwrap();

        let (transcript, archive) =
            publish_cleaned(&output, None, &audio, "clean", "c0ffee", None).unwrap();

        assert_eq!(
            fs::read_to_string(transcript).unwrap(),
            format!(
                ":PROPERTIES:\n\
                 :RECORDING_AUDIO: [[file:{}]]\n\
                 :RECORDING_AUDIO_SHA512_256: c0ffee\n\
                 :END:\n\
                 clean\n",
                audio.display()
            )
        );
        assert_eq!(archive, None);
        assert!(audio.is_file());
    }

    #[test]
    fn publishing_with_archive_links_the_name_the_audio_was_archived_under() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.m4a");
        let output = dir.path().join("transcripts");
        let archive_dir = dir.path().join("archive");
        fs::write(&audio, "audio").unwrap();
        fs::create_dir(&output).unwrap();
        fs::create_dir(&archive_dir).unwrap();
        // An earlier transcript holds the plain stem, so this one is `memo-2`.
        fs::write(output.join("memo.txt"), "earlier").unwrap();

        let (transcript, archive) =
            publish_cleaned(&output, Some(&archive_dir), &audio, "clean", "c0ffee", None).unwrap();

        let archived = archive_dir.join("memo-2.m4a");
        assert_eq!(archive.as_deref(), Some(archived.as_path()));
        assert_eq!(fs::read(&archived).unwrap(), b"audio");
        assert!(
            fs::read_to_string(transcript)
                .unwrap()
                .starts_with(&format!(
                    ":PROPERTIES:\n:RECORDING_AUDIO: [[file:{}]]\n",
                    archived.display()
                ))
        );
    }

    /// 8 kHz 8-bit mono `samples` as a WAV file.
    fn write_wav(path: &Path, samples: &[u8]) {
        let rate = 8_000_u32;
        let size = u32::try_from(samples.len()).unwrap();
        let wav = [
            &b"RIFF"[..],
            &(36 + size).to_le_bytes(),
            b"WAVEfmt \x10\0\0\0\x01\0\x01\0", // 16-byte PCM mono format chunk
            &rate.to_le_bytes(),
            &rate.to_le_bytes(), // bytes per second
            b"\x01\0\x08\0data", // 1-byte frames of 8 bits
            &size.to_le_bytes(),
            samples,
        ]
        .concat();
        fs::write(path, wav).unwrap();
    }

    /// `length` of silence as 8 kHz unsigned 8-bit samples.
    fn silence(length: Duration) -> Vec<u8> {
        // Unsigned 8-bit samples are silent at their midpoint.
        vec![0x80; length.as_millis() as usize * 8]
    }

    /// `length` of silence as a WAV file.
    fn write_silence(path: &Path, length: Duration) {
        write_wav(path, &silence(length));
    }

    /// Whether macOS's afinfo or afconvert is missing, as on Linux or in a Nix
    /// build whose PATH omits /usr/bin; tests needing them then pass without
    /// running.
    fn without_audio_tools() -> bool {
        let missing = ["afinfo", "afconvert"]
            .iter()
            .any(|tool| Command::new(tool).arg("-h").output().is_err());
        if missing {
            eprintln!("afinfo or afconvert is not on PATH; skipping");
        }
        missing
    }

    /// A monitor of `dir/watch` keeping its transcripts, archive and state in
    /// `dir`; its transcriber, if it ever runs, creates `dir/transcribed`.
    fn monitor(dir: &Path) -> Env {
        let watch = dir.join("watch");
        fs::create_dir(&watch).unwrap();
        Env {
            opts: Options {
                watch_dir: Some(watch),
                audio_files: Vec::new(),
                output_dir: Some(dir.to_owned()),
                archive_dir: Some(dir.to_owned()),
                prompt_file: dir.join("prompt.md"),
                transcriber: Transcriber::AsrShell(format!(
                    "touch {}",
                    dir.join("transcribed").display()
                )),
                llm_url: "http://127.0.0.1:9".to_owned(),
                llm_model: "model".to_owned(),
                api_key: "key".to_owned(),
                language: "en".to_owned(),
                poll_secs: 30,
                once: true,
            },
            state_dir: dir.to_owned(),
            home: None,
            client: Client::new(),
        }
    }

    #[test]
    fn too_long_draws_the_line_just_past_ten_minutes() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let at_limit = dir.path().join("at-limit.wav");
        let past_limit = dir.path().join("past-limit.wav");
        write_silence(&at_limit, Duration::from_secs(600));
        // Half a second past ten minutes is still past them.
        write_silence(&past_limit, Duration::from_millis(600_500));

        assert_eq!(too_long(&at_limit).unwrap(), None);
        assert_eq!(
            too_long(&past_limit).unwrap(),
            Some(Duration::from_millis(600_500))
        );
    }

    #[test]
    fn too_long_counts_raw_aac_instead_of_guessing_from_a_quiet_opening() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("memo.wav");
        let aac = dir.path().join("memo.aac");
        // A minute that opens with two seconds of silence, then noise. Raw AAC
        // has no packet table, and judged by its quiet first packets alone,
        // this reads as over twenty minutes.
        let mut state = 1_u32;
        let noise = (0..58 * 8_000).map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        });
        let mut samples = silence(Duration::from_secs(2));
        samples.extend(noise);
        write_wav(&wav, &samples);
        let converted = Command::new("afconvert")
            .args(["-f", "adts", "-d", "aac"])
            .arg(&wav)
            .arg(&aac)
            .status()
            .unwrap();
        assert!(converted.success());

        assert_eq!(too_long(&aac).unwrap(), None);
    }

    #[test]
    fn too_long_fails_rather_than_pass_audio_it_cannot_measure() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.m4a");
        fs::write(&audio, "not audio").unwrap();

        assert!(too_long(&audio).is_err());
    }

    #[test]
    fn monitor_transcribes_a_recording_of_exactly_ten_minutes() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let env = monitor(dir.path());
        let watch = dir.path().join("watch");
        let audio = watch.join("memo.wav");
        write_silence(&audio, Duration::from_secs(600));

        step_file(&env, &scan_audio(&watch), &mut Verdicts::new(), &audio);

        // The transcriber ran, though its empty transcript then failed the run.
        assert!(dir.path().join("transcribed").is_file());
    }

    #[test]
    fn monitor_leaves_a_recording_past_the_limit_in_place_untranscribed() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let env = monitor(dir.path());
        let watch = dir.path().join("watch");
        let audio = watch.join("memo.wav");
        write_silence(&audio, MAX_DURATION + Duration::from_secs(1));
        let mut verdicts = Verdicts::new();

        step_file(&env, &scan_audio(&watch), &mut verdicts, &audio);

        assert!(matches!(verdicts[&audio], (_, Verdict::TooLong)));
        assert!(audio.is_file());
        // No transcript, archived audio, spool, or transcriber run beside it.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["watch"]);
    }

    #[test]
    fn monitor_measures_a_long_recording_again_only_once_it_changes() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let env = monitor(dir.path());
        let watch = dir.path().join("watch");
        let audio = watch.join("memo.wav");
        write_silence(&audio, MAX_DURATION + Duration::from_secs(1));
        let mut verdicts = Verdicts::new();
        step_file(&env, &scan_audio(&watch), &mut verdicts, &audio);

        // Same size and mtime, so the same identity, but no longer audio:
        // measuring it again would fail and record a failure instead.
        let metadata = fs::metadata(&audio).unwrap();
        fs::write(&audio, vec![0; metadata.len() as usize]).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&audio)
            .unwrap()
            .set_modified(metadata.modified().unwrap())
            .unwrap();
        step_file(&env, &scan_audio(&watch), &mut verdicts, &audio);
        assert!(matches!(verdicts[&audio], (_, Verdict::TooLong)));

        // A changed file is measured afresh.
        write_silence(&audio, MAX_DURATION + Duration::from_secs(2));
        let scan = scan_audio(&watch);
        step_file(&env, &scan, &mut verdicts, &audio);
        assert!(matches!(
            verdicts[&audio],
            (identity, Verdict::TooLong) if identity == scan[&audio]
        ));
    }

    #[test]
    fn monitor_retries_audio_it_cannot_measure_then_gives_up_until_it_changes() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let env = monitor(dir.path());
        let watch = dir.path().join("watch");
        let audio = watch.join("memo.m4a");
        fs::write(&audio, "not audio").unwrap();
        let scan = scan_audio(&watch);
        let mut verdicts = Verdicts::new();

        for attempt in 1..=5 {
            step_file(&env, &scan, &mut verdicts, &audio);
            assert!(matches!(verdicts[&audio], (_, Verdict::Failed(n)) if n == attempt));
        }
        // Abandoned, so a further sweep makes no attempt.
        step_file(&env, &scan, &mut verdicts, &audio);
        assert!(matches!(verdicts[&audio], (_, Verdict::Failed(5))));
        assert!(audio.is_file());

        // A changed file starts over.
        fs::write(&audio, "still not audio").unwrap();
        step_file(&env, &scan_audio(&watch), &mut verdicts, &audio);
        assert!(matches!(verdicts[&audio], (_, Verdict::Failed(1))));
    }

    #[test]
    fn processing_a_named_recording_ignores_the_limit() {
        if without_audio_tools() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut env = monitor(dir.path());
        env.opts.watch_dir = None;
        let audio = dir.path().join("watch").join("memo.wav");
        write_silence(&audio, MAX_DURATION + Duration::from_secs(1));

        // The transcriber ran, though its empty transcript then failed the run.
        assert!(process_file(&env, &audio, &file_identity(&audio).unwrap()).is_err());
        assert!(dir.path().join("transcribed").is_file());
    }
}
