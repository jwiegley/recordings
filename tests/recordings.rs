use recordings::*;
use serde_json::to_vec;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

fn some_time() -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_788_134_400)
}

/// Age a directory so prune sees it as idle without the test having to wait.
fn backdate(path: &Path, age: Duration) {
    let when = SystemTime::now() - age;
    let handle = fs::File::open(path).unwrap();
    handle
        .set_times(fs::FileTimes::new().set_accessed(when).set_modified(when))
        .unwrap();
}

#[test]
fn audio_names_accept_common_extensions_case_insensitively() {
    assert!(is_audio_name("memo.m4a"));
    assert!(is_audio_name("MEMO.M4A"));
    assert!(is_audio_name("take.wav"));
}

#[test]
fn audio_names_reject_transcripts_hidden_files_and_icloud_placeholders() {
    assert!(!is_audio_name("memo.txt"));
    assert!(!is_audio_name(".memo.m4a"));
    assert!(!is_audio_name("memo.m4a.icloud"));
}

#[test]
fn ready_files_keeps_only_files_unchanged_across_both_scans() {
    let ident1 = (100, some_time());
    let ident2 = (200, some_time());
    let before = BTreeMap::from([
        (PathBuf::from("a"), ident1),
        (PathBuf::from("b"), ident1),
        (PathBuf::from("gone"), ident1),
    ]);
    let after = BTreeMap::from([
        (PathBuf::from("a"), ident1),
        (PathBuf::from("b"), ident2),
        (PathBuf::from("new"), ident1),
    ]);
    assert_eq!(ready_files(&before, &after), [PathBuf::from("a")]);
}

#[test]
fn encode_safe_flattens_slashes_injectively() {
    assert_eq!(encode_safe("a/b.m4a"), "a%2Fb.m4a");
    assert_eq!(encode_safe("50%.m4a"), "50%25.m4a");
    assert_ne!(encode_safe("a%2Fb"), encode_safe("a/b"));
}

#[test]
fn temp_name_binds_spool_to_path_size_and_mtime() {
    let name = temp_name("voice/memo.m4a", &(12_345, some_time()));
    assert!(!name.contains('/'));
    assert!(name.contains("12345"));
    assert_eq!(
        Path::new(&name).extension().and_then(|x| x.to_str()),
        Some("txt")
    );
}

#[test]
fn target_stems_starts_plain_then_counts_from_two() {
    assert_eq!(
        target_stems("memo").take(3).collect::<Vec<_>>(),
        ["memo", "memo-2", "memo-3"]
    );
}

#[test]
fn inside_detects_containment_and_identity() {
    assert!(is_inside(Path::new("/a/b"), Path::new("/a/b/c")));
    assert!(is_inside(Path::new("/a/b"), Path::new("/a/b")));
    assert!(!is_inside(Path::new("/a/b"), Path::new("/a/bc")));
    assert!(!is_inside(Path::new("/a/b/c"), Path::new("/a/b")));
}

#[test]
fn excerpt_keeps_short_messages_whole_and_both_ends_of_long_ones() {
    assert_eq!(excerpt_ends(10, "short"), "short");
    let long = "a".repeat(50) + "CAUSE";
    let excerpt = excerpt_ends(10, &long);
    assert!(excerpt.contains("CAUSE"));
    assert!(excerpt.starts_with("aaaa"));
}

#[test]
fn join_url_ignores_trailing_slash() {
    assert_eq!(
        join_url("http://h/v1", "chat/completions"),
        "http://h/v1/chat/completions"
    );
    assert_eq!(
        join_url("http://h/v1/", "chat/completions"),
        "http://h/v1/chat/completions"
    );
}

#[test]
fn cleanup_request_frames_the_transcript_as_dictation() {
    let said = "Remind Sano that he should also get a good recording app.";
    let request = chat_request_value("m", "rules", said);
    assert_eq!(request["messages"][0]["content"], "rules");
    let user = request["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("not a request to you"), "{user}");
    assert!(
        user.ends_with(&format!("<transcript>\n{said}\n</transcript>")),
        "{user}"
    );
}

#[test]
fn unwrap_cleanup_keeps_only_the_last_tagged_span() {
    assert_eq!(
        unwrap_cleanup("<transcript>\nHello.\n</transcript>\n"),
        "Hello."
    );
    assert_eq!(unwrap_cleanup("  Hello.  "), "Hello.");
    assert_eq!(unwrap_cleanup("<Transcript>Hello.</TRANSCRIPT>"), "Hello.");
    assert_eq!(
        unwrap_cleanup("Hello.</transcript>\nLet me know if you need more."),
        "Hello."
    );
    // A model that echoes the request keeps only the transcript's span.
    let request = chat_request_value("m", "rules", "Hello.");
    let echoed = format!(
        "{}\n<transcript>\nHello again.\n</transcript>",
        request["messages"][1]["content"].as_str().unwrap()
    );
    assert_eq!(unwrap_cleanup(&echoed), "Hello again.");
}

#[test]
fn prompt_placeholders_left_unfilled_are_reported() {
    let dir = tempdir().unwrap();
    let prompt = dir.path().join("prompt.md");
    assert!(
        prompt_file_problem(&prompt).is_some_and(|problem| problem.contains("missing")),
        "a missing prompt"
    );
    fs::write(&prompt, "Clean it.\n\nTranscript:\n${output}\n").unwrap();
    assert!(
        prompt_file_problem(&prompt).is_some_and(|problem| problem.contains("${output}")),
        "an unfilled placeholder"
    );
    fs::write(&prompt, "Clean it.\n").unwrap();
    assert_eq!(prompt_file_problem(&prompt), None);
}

/// Cleanups by the rules of the deployed prompt, each of which must stand.
const FAITHFUL_CLEANUPS: &[(&str, &str)] = &[
    (
        "Remind Sano that he should also get a good recording app.",
        "Remind Sano that he should also get a good recording app.",
    ),
    (
        "um so I think uh we should we should ship it period",
        "So I think we should ship it.",
    ),
    ("Call Bob at five thirty", "Call Bob at 5:30."),
    (
        "Meeting at three thirty pm tomorrow",
        "Meeting at 3:30 PM tomorrow.",
    ),
    ("Gate code is four two seven one", "Gate code is 4271."),
    (
        "Call Sano at four one five five five five one two three four",
        "Call Sano at 4155551234.",
    ),
    (
        "Pay the rent of twenty four hundred dollars",
        "Pay the rent of $2,400.",
    ),
    (
        "Try Qwen three point five on the M four max",
        "Try Qwen 3.5 on the M4 Max.",
    ),
    (
        "Upgrade to python three point twelve",
        "Upgrade to Python 3.12.",
    ),
    (
        "The M three ultra has five hundred twelve gigabytes of unified memory and eight \
         hundred gigabytes per second of bandwidth",
        "The M3 Ultra has 512 GB of unified memory and 800 GB per second of bandwidth.",
    ),
    (
        "Connect to one twenty seven dot zero dot zero dot one",
        "Connect to 127.0.0.1.",
    ),
    (
        "call foo underscore bar open paren x comma y close paren semicolon",
        "call foo_bar(x, y);",
    ),
    (
        "x equals foo open paren bar comma baz close paren",
        "x = foo(bar, baz)",
    ),
    (
        "use std double colon collections double colon hash map semicolon",
        "use std::collections::HashMap;",
    ),
    (
        "I tried pie torch and tensor flow on the M three max with llama see PP and V LLM",
        "I tried PyTorch and TensorFlow on the M3 Max with llama.cpp and vLLM.",
    ),
    (
        "I love see plus plus and pie torch",
        "I love C++ and PyTorch.",
    ),
    (
        "I want to, I need to, actually let me, let's just go with the second option",
        "Let's just go with the second option.",
    ),
    (
        "I, um, can't make the meeting tomorrow",
        "I can't make the meeting tomorrow.",
    ),
    (
        "Tell her I'm, uh, sorry I missed the call",
        "Tell her I'm sorry I missed the call.",
    ),
    (
        "Im sorry I missed your call",
        "I'm sorry I missed your call.",
    ),
    (
        "We dont have access to the staging server",
        "We don't have access to the staging server.",
    ),
    (
        "The UI can not scroll past the fold",
        "The UI cannot scroll past the fold.",
    ),
    (
        "The A.P.I. can't handle that many requests",
        "The API can't handle that many requests.",
    ),
    (
        "As an A.I. engineer I care about latency",
        "As an AI engineer, I care about latency.",
    ),
    ("the API, uh, can't handle it", "The API can't handle it."),
    (
        "and then I\ncannot reproduce the crash",
        "And then I cannot reproduce the crash.",
    ),
    ("I can not believe it works", "I cannot believe it works."),
    ("I'll call you when I land", "I'll call you when I land."),
    ("Note to self: buy milk", "Note to self: buy milk."),
    ("我们明天开会讨论预算", "我们明天开会，讨论预算。"),
    ("Fifteen", "15"),
];

#[test]
fn faithful_cleanups_are_accepted() {
    for (raw, cleaned) in FAITHFUL_CLEANUPS {
        assert_eq!(cleanup_rejection(raw, cleaned), None, "{cleaned}");
    }
}

#[test]
fn refusals_answers_commentary_and_summaries_are_rejected() {
    let sano = "Remind Sano that he should also get a good recording app.";
    let plan = "So the plan for tomorrow is to finish the migration of the build cache, \
                then look at why the nightly job keeps timing out on the larger runners";
    for (raw, cleaned, reason) in [
        // The reply published on 2026-11-07.
        (
            sano,
            "I cannot fulfill this request as I do not have access to external communication \
             channels or the ability to send messages to individuals.",
            "\"i cannot\"",
        ),
        // A refusal that repeats the memo passes both shares.
        (
            "Remind Sano that he should also get a good recording app",
            "I'm sorry, but I can't remind Sano that he should also get a good recording app.",
            "\"i cant\"",
        ),
        (sano, &format!("Reminder set: {sano}"), "\"reminder set\""),
        (
            plan,
            &format!("Here is the cleaned transcript: {plan}."),
            "\"here is\"",
        ),
        (
            plan,
            &format!("{plan}.\n\nNote: I removed the filler words."),
            "\"note\"",
        ),
        (
            plan,
            &format!("{plan}. (Sorry, but sending reminders is not something I'm able to do.)"),
            "\"not something im able\"",
        ),
        // An answer that uses none of those phrases.
        (
            "What's the weather tomorrow",
            "Tomorrow will be sunny with a high of 72 degrees.",
            "were spoken",
        ),
        // A summary built only from the speaker's words.
        (
            "we met with the vendor today and talked through the pricing the support terms \
             the migration plan and the timeline for the first release",
            "Vendor meeting about pricing.",
            "speaker's words",
        ),
    ] {
        let rejection = cleanup_rejection(raw, cleaned);
        assert!(
            rejection.as_deref().is_some_and(|why| why.contains(reason)),
            "{cleaned}: {rejection:?}"
        );
    }
}

#[test]
fn a_completion_cut_short_is_recognized() {
    assert!(chat_hit_length_limit(
        br#"{"choices":[{"message":{"content":"Remind"},"finish_reason":"length"}]}"#
    ));
    assert!(!chat_hit_length_limit(
        br#"{"choices":[{"message":{"content":"Remind."},"finish_reason":"stop"}]}"#
    ));
    assert!(!chat_hit_length_limit(
        br#"{"choices":[{"message":{"content":"Remind."}}]}"#
    ));
}

#[test]
fn max_tokens_clamps_between_2048_and_32768() {
    assert_eq!(max_tokens_for("short"), 2_048);
    assert_eq!(max_tokens_for(&"x".repeat(100_000)), 32_768);
    assert_eq!(max_tokens_for(&"x".repeat(10_000)), 5_000);
}

#[test]
fn chat_request_disables_streaming_and_thinking() {
    let body = to_vec(&chat_request_value("glm", "fix it", "raw words")).unwrap();
    assert!(
        body.windows(b"\"stream\":false".len())
            .any(|x| x == b"\"stream\":false")
    );
    assert!(
        body.windows(b"\"enable_thinking\":false".len())
            .any(|x| x == b"\"enable_thinking\":false")
    );
}

#[test]
fn chat_request_carries_prompt_as_system_and_transcript_as_user() {
    let body =
        String::from_utf8(to_vec(&chat_request_value("glm", "fix it", "raw words")).unwrap())
            .unwrap();
    assert!(body.contains("\"system\""));
    assert!(body.contains("\"fix it\""));
    assert!(body.contains("<transcript>\\nraw words\\n</transcript>"));
}

#[test]
fn decode_asr_extracts_text_field() {
    assert_eq!(
        decode_asr_text(br#"{"text":"hello","language":"en"}"#).unwrap(),
        "hello"
    );
}

#[test]
fn decode_asr_fails_on_missing_text() {
    assert!(decode_asr_text(b"{}").is_err());
}

#[test]
fn decode_chat_extracts_first_choice_content() {
    assert_eq!(
        decode_chat_text(br#"{"choices":[{"message":{"role":"assistant","content":"clean"}}]}"#)
            .unwrap(),
        "clean"
    );
}

#[test]
fn decode_chat_fails_on_empty_choices() {
    assert!(decode_chat_text(br#"{"choices":[]}"#).is_err());
}

#[test]
fn afinfo_duration_reads_estimated_duration_line() {
    // `afinfo -r`'s report on an eleven-minute AAC recording, abridged.
    let output = "\
File:           memo.m4a
File type ID:   m4af
Num Tracks:     1
----
Data format:     1 ch,  16000 Hz, aac  (0x00000000) 0 bits/channel, 0 bytes/packet, 1024 frames/packet, 0 bytes/frame
                no channel layout.
audio bytes: 41260
audio packets: 10315
estimated duration: 660.000 sec
bit rate: 500 bits per second
";
    assert_eq!(afinfo_duration(output), Some(Duration::from_secs(660)));
    // Fractions of a second survive.
    assert_eq!(
        afinfo_duration("estimated duration: 600.500 sec\n"),
        Some(Duration::from_millis(600_500))
    );
}

#[test]
fn afinfo_duration_fails_on_missing_or_impossible_duration() {
    // What afinfo prints to stdout for a file it cannot open.
    assert_eq!(afinfo_duration("File:           memo.m4a\n"), None);
    assert_eq!(afinfo_duration("estimated duration: nan sec\n"), None);
    assert_eq!(afinfo_duration("estimated duration: -1.000000 sec\n"), None);
}

#[test]
fn durable_write_is_atomic_and_leaves_no_temp_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("out.txt");
    write_file_durable(&path, "content\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "content\n");
    assert!(!dir.path().join("out.txt.tmp").is_file());
}

#[test]
fn move_path_moves_content_and_removes_source() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("a.m4a");
    let dst = dir.path().join("b.m4a");
    fs::write(&src, b"audio-bytes").unwrap();
    move_path(&src, &dst).unwrap();
    assert_eq!(fs::read(&dst).unwrap(), b"audio-bytes");
    assert!(!src.is_file());
}

#[test]
fn fresh_targets_uses_plain_stem_when_free() {
    let dir = tempdir().unwrap();
    let (txt, archive) =
        fresh_targets(dir.path(), Some(dir.path()), &dir.path().join("memo.m4a")).unwrap();
    assert_eq!(txt, dir.path().join("memo.txt"));
    assert_eq!(archive, Some(dir.path().join("memo.m4a")));
}

#[test]
fn fresh_targets_without_archive_only_reserves_transcript_name() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("memo.m4a"), "audio").unwrap();
    let (txt, archive) = fresh_targets(dir.path(), None, &dir.path().join("memo.m4a")).unwrap();
    assert_eq!(txt, dir.path().join("memo.txt"));
    assert_eq!(archive, None);
}

#[test]
fn fresh_targets_moves_to_two_when_either_destination_is_taken() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("memo.txt"), "occupied").unwrap();
    let (txt, archive) =
        fresh_targets(dir.path(), Some(dir.path()), &dir.path().join("memo.m4a")).unwrap();
    assert_eq!(txt, dir.path().join("memo-2.txt"));
    assert_eq!(archive, Some(dir.path().join("memo-2.m4a")));
}

#[test]
fn audio_checksum_is_the_first_256_bits_of_sha512() {
    let dir = tempdir().unwrap();
    let audio = dir.path().join("memo.m4a");
    fs::write(&audio, "abc").unwrap();
    // FIPS 180-2 SHA-512("abc"), truncated; SHA-512/256 would give 53048e26....
    assert_eq!(
        audio_checksum(&audio).unwrap(),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a"
    );
}

#[test]
fn audio_checksum_digests_files_longer_than_one_read() {
    let dir = tempdir().unwrap();
    let audio = dir.path().join("long.wav");
    fs::write(&audio, vec![b'a'; 1_000_000]).unwrap();
    // FIPS 180-2 SHA-512 of one million 'a' bytes, truncated.
    assert_eq!(
        audio_checksum(&audio).unwrap(),
        "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973eb"
    );
}

#[test]
fn provenance_drawer_links_audio_under_home_with_tilde() {
    let checksum = "ab".repeat(32);
    assert_eq!(
        provenance_drawer(
            Path::new("/Users/me/Documents/Recordings/06-50-28.m4a"),
            &checksum,
            Some(Path::new("/Users/me")),
        ),
        format!(
            ":PROPERTIES:\n\
             :RECORDING_AUDIO: [[file:~/Documents/Recordings/06-50-28.m4a]]\n\
             :RECORDING_AUDIO_SHA512_256: {checksum}\n\
             :END:\n"
        )
    );
}

/// The `:RECORDING_AUDIO:` line of a provenance drawer.
fn audio_line(audio: &str, home: Option<&str>) -> String {
    provenance_drawer(Path::new(audio), "00", home.map(Path::new))
        .lines()
        .nth(1)
        .unwrap()
        .to_owned()
}

#[test]
fn provenance_drawer_keeps_audio_outside_home_absolute() {
    // A sibling whose name merely extends home's is not inside it.
    assert_eq!(
        audio_line("/Users/me2/x.m4a", Some("/Users/me")),
        ":RECORDING_AUDIO: [[file:/Users/me2/x.m4a]]"
    );
    assert_eq!(
        audio_line("/Users/me/x.m4a", None),
        ":RECORDING_AUDIO: [[file:/Users/me/x.m4a]]"
    );
    // A root home would turn every path into `~/...`, which no other reader
    // could resolve; Emacs's `abbreviate-file-name` declines it too.
    assert_eq!(
        audio_line("/Users/me/x.m4a", Some("/")),
        ":RECORDING_AUDIO: [[file:/Users/me/x.m4a]]"
    );
}

#[test]
fn provenance_drawer_escapes_links_as_org_does() {
    // Expected values are Emacs's own `org-link-make-string` output.
    assert_eq!(
        audio_line("/rec/Interview [raw].m4a", None),
        r":RECORDING_AUDIO: [[file:/rec/Interview \[raw\].m4a]]"
    );
    assert_eq!(
        audio_line(r"/rec/a\[b].m4a", None),
        r":RECORDING_AUDIO: [[file:/rec/a\\\[b\].m4a]]"
    );
    assert_eq!(
        audio_line(r"/rec/odd\", None),
        r":RECORDING_AUDIO: [[file:/rec/odd\\]]"
    );
    assert_eq!(
        audio_line(r"/rec/x\y.m4a", None),
        r":RECORDING_AUDIO: [[file:/rec/x\y.m4a]]"
    );
}

#[test]
fn prune_removes_empty_and_ds_store_only_subdirs_but_not_root() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("empty1/empty2")).unwrap();
    fs::create_dir_all(dir.path().join("junk")).unwrap();
    fs::create_dir_all(dir.path().join("keep")).unwrap();
    fs::write(dir.path().join("junk/.DS_Store"), "").unwrap();
    fs::write(dir.path().join("keep/file.m4a"), "x").unwrap();
    prune_empty_dirs(dir.path(), Duration::ZERO);
    assert!(!dir.path().join("empty1").is_dir());
    assert!(!dir.path().join("junk").is_dir());
    assert!(dir.path().join("keep").is_dir());
    assert!(dir.path().is_dir());
}

#[test]
fn prune_spares_empty_dirs_until_they_have_been_idle_long_enough() {
    let dir = tempdir().unwrap();
    let fresh = dir.path().join("2026-09-06");
    let stale = dir.path().join("2026-09-01");
    fs::create_dir(&fresh).unwrap();
    fs::create_dir(&stale).unwrap();
    backdate(&stale, PRUNE_MIN_IDLE + Duration::from_secs(60));

    prune_empty_dirs(dir.path(), PRUNE_MIN_IDLE);

    // A date directory another device may still be uploading into survives,
    // which is the whole point of the delay.
    assert!(fresh.is_dir());
    // Yesterday's leftovers are still collected, so they cannot pile up.
    assert!(!stale.is_dir());
}

#[test]
fn prune_leaves_ds_store_alone_while_the_directory_is_too_young() {
    let dir = tempdir().unwrap();
    let fresh = dir.path().join("2026-09-06");
    fs::create_dir(&fresh).unwrap();
    fs::write(fresh.join(".DS_Store"), "").unwrap();

    prune_empty_dirs(dir.path(), PRUNE_MIN_IDLE);

    // Deleting the .DS_Store is part of pruning, so it must wait too.
    assert!(fresh.join(".DS_Store").is_file());
}

#[test]
fn prune_keeps_a_directory_whose_child_was_just_removed() {
    let dir = tempdir().unwrap();
    let outer = dir.path().join("outer");
    let inner = outer.join("inner");
    fs::create_dir_all(&inner).unwrap();
    // Innermost first: creating a child bumps the parent's mtime.
    backdate(&inner, PRUNE_MIN_IDLE + Duration::from_secs(60));
    backdate(&outer, PRUNE_MIN_IDLE + Duration::from_secs(60));

    prune_empty_dirs(dir.path(), PRUNE_MIN_IDLE);

    // `inner` was old enough to go; `outer` is judged on the mtime it had
    // before that removal bumped it, so one pass collects the whole chain.
    assert!(!inner.is_dir());
    assert!(!outer.is_dir());
}

#[test]
fn decode_fixtures_accept_byte_json() {
    assert_eq!(decode_asr_text(b"{\"text\":\"ok\"}").unwrap(), "ok");
}
