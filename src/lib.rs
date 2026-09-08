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
use std::io::{self, Write};
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

fn publish_cleaned(
    output_dir: &Path,
    archive_dir: Option<&Path>,
    audio: &Path,
    cleaned: &str,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let (transcript, archive) = fresh_targets(output_dir, archive_dir, audio)?;
    write_file_durable(&transcript, &format!("{}\n", cleaned.trim_end()))?;
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
    client: Client,
}

type Failures = BTreeMap<PathBuf, (Identity, usize)>;

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
/// 2. publish cleaned transcript durably before touching audio;
/// 3. optionally move audio to archive, then remove spool.
///
/// ponytail: crash exactly between 2 and 3 republishes under `name-2` on next
/// sweep: visible duplicate, never loss.
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

fn step_file(env: &Env, scan: &BTreeMap<PathBuf, Identity>, failures: &mut Failures, audio: &Path) {
    let Some(identity) = scan.get(audio).copied() else {
        return;
    };
    if matches!(
        failures.get(audio),
        Some((failed_identity, attempts))
            if *failed_identity == identity && *attempts >= MAX_ATTEMPTS
    ) {
        return;
    }

    match process_file(env, audio, &identity) {
        Ok(()) => {
            failures.remove(audio);
        }
        Err(error) => {
            let attempts = match failures.get(audio) {
                Some((failed_identity, attempts)) if *failed_identity == identity => attempts + 1,
                _ => 1,
            };
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
            failures.insert(audio.to_owned(), (identity, attempts));
        }
    }
}

fn sweep(env: &Env, failures: &mut Failures) {
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
        step_file(env, &after, failures, &audio);
    }
    prune_empty_dirs(watch_dir, PRUNE_MIN_IDLE);
    failures.retain(|path, _| after.contains_key(path));
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

    let mut failures = BTreeMap::new();
    if env.opts.once {
        sweep(&env, &mut failures);
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
        sweep(&env, &mut failures);
        wait_for_kick(&receiver, poll, watcher.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_without_archive_keeps_audio() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.m4a");
        let output = dir.path().join("transcripts");
        fs::write(&audio, "audio").unwrap();
        fs::create_dir(&output).unwrap();

        let (transcript, archive) = publish_cleaned(&output, None, &audio, "clean").unwrap();

        assert_eq!(fs::read_to_string(transcript).unwrap(), "clean\n");
        assert_eq!(archive, None);
        assert!(audio.is_file());
    }
}
