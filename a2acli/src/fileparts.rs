// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! `--save-fileparts <dir>`: write the raw file parts of agent responses to disk.
//!
//! Naming and overwrite rules match the Go `a2a-cli`, so both leave the same
//! files behind for the same response.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use a2a::{Artifact, Message, Part, PartContent, SendMessageResponse, StreamResponse, Task};

/// A file part that could not be written.
#[derive(Debug)]
pub(crate) struct SaveError {
    pub(crate) path: PathBuf,
    pub(crate) source: std::io::Error,
}

/// The path handed to one message or artifact.
struct Reserved {
    owner: String,
    path: PathBuf,
}

/// Writes raw file parts into one directory for the length of one invocation.
///
/// - Parts of one message or artifact that share a name are joined into one file.
/// - A file that already exists is overwritten the first time it is written.
/// - An artifact update with `append` extends the file its artifact wrote.
/// - A different artifact with the same name gets a ` (n)` suffix instead of
///   replacing the earlier file.
pub(crate) struct FilePartSaver {
    dir: PathBuf,
    reserved: HashMap<String, Vec<Reserved>>,
    created: Vec<PathBuf>,
}

impl FilePartSaver {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            reserved: HashMap::new(),
            created: Vec::new(),
        }
    }

    /// The files first written since the last call, for the caller to report.
    pub(crate) fn take_created(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.created)
    }

    pub(crate) fn save_task(&mut self, task: &Task) -> Result<(), SaveError> {
        for artifact in task.artifacts.iter().flatten() {
            self.save_artifact(artifact, false)?;
        }
        match &task.status.message {
            Some(message) => self.save_message(message),
            None => Ok(()),
        }
    }

    pub(crate) fn save_message(&mut self, message: &Message) -> Result<(), SaveError> {
        let owner = format!("msg:{}", message.message_id);
        self.save(&owner, "status-msg", &message.parts, false)
    }

    pub(crate) fn save_artifact(
        &mut self,
        artifact: &Artifact,
        append: bool,
    ) -> Result<(), SaveError> {
        let owner = format!("art:{}", artifact.artifact_id);
        let fallback = artifact
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&artifact.artifact_id);
        self.save(&owner, fallback, &artifact.parts, append)
    }

    fn save(
        &mut self,
        owner: &str,
        fallback: &str,
        parts: &[Part],
        append: bool,
    ) -> Result<(), SaveError> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for part in parts {
            let PartContent::Raw(bytes) = &part.content else {
                continue;
            };
            let name = resolve_name(part, fallback);
            match files.iter_mut().find(|(existing, _)| *existing == name) {
                Some((_, data)) => data.extend_from_slice(bytes),
                None => files.push((name, bytes.clone())),
            }
        }

        for (name, data) in files {
            let (path, is_new) = self.reserve(owner, &name);
            write_file(&path, &data, append && !is_new).map_err(|source| SaveError {
                path: path.clone(),
                source,
            })?;
            if is_new {
                self.created.push(path);
            }
        }
        Ok(())
    }

    /// The path for `name` under `owner`, and whether it was just reserved.
    fn reserve(&mut self, owner: &str, name: &str) -> (PathBuf, bool) {
        let taken = self.reserved.entry(name.to_string()).or_default();
        if let Some(existing) = taken.iter().find(|r| r.owner == owner) {
            return (existing.path.clone(), false);
        }
        let file_name = if taken.is_empty() {
            name.to_string()
        } else {
            numbered(name, taken.len())
        };
        let path = self.dir.join(file_name);
        taken.push(Reserved {
            owner: owner.to_string(),
            path: path.clone(),
        });
        (path, true)
    }
}

/// Something an agent sent back that may carry file parts.
pub(crate) trait FilePartSource {
    fn save_to(&self, saver: &mut FilePartSaver) -> Result<(), SaveError>;
}

impl FilePartSource for Task {
    fn save_to(&self, saver: &mut FilePartSaver) -> Result<(), SaveError> {
        saver.save_task(self)
    }
}

impl FilePartSource for Message {
    fn save_to(&self, saver: &mut FilePartSaver) -> Result<(), SaveError> {
        saver.save_message(self)
    }
}

impl FilePartSource for SendMessageResponse {
    fn save_to(&self, saver: &mut FilePartSaver) -> Result<(), SaveError> {
        match self {
            SendMessageResponse::Task(task) => saver.save_task(task),
            SendMessageResponse::Message(message) => saver.save_message(message),
        }
    }
}

impl FilePartSource for StreamResponse {
    fn save_to(&self, saver: &mut FilePartSaver) -> Result<(), SaveError> {
        match self {
            StreamResponse::Task(task) => saver.save_task(task),
            StreamResponse::Message(message) => saver.save_message(message),
            StreamResponse::StatusUpdate(event) => match &event.status.message {
                Some(message) => saver.save_message(message),
                None => Ok(()),
            },
            StreamResponse::ArtifactUpdate(event) => {
                saver.save_artifact(&event.artifact, event.append.unwrap_or(false))
            }
        }
    }
}

fn write_file(path: &Path, data: &[u8], append: bool) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options.open(path)?.write_all(data)
}

/// `report.pdf` as the `n`th file of that name becomes `report (n).pdf`.
fn numbered(name: &str, n: usize) -> String {
    let path = Path::new(name);
    match (
        path.file_stem().and_then(|stem| stem.to_str()),
        path.extension().and_then(|ext| ext.to_str()),
    ) {
        (Some(stem), Some(ext)) => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// The file name for a part: its own filename, else `fallback`, else
/// `filepart`, with an extension from the media type when it has none.
fn resolve_name(part: &Part, fallback: &str) -> String {
    let mut name = part
        .filename
        .as_deref()
        .and_then(clean_name)
        .or_else(|| clean_name(fallback))
        .unwrap_or_else(|| "filepart".to_string());
    if Path::new(&name).extension().is_none() {
        if let Some(extension) = part.media_type.as_deref().and_then(extension_for) {
            name.push_str(extension);
        }
    }
    name
}

/// One safe file name out of an agent-supplied one. A name never picks the
/// directory: only the last component under either separator is kept.
fn clean_name(raw: &str) -> Option<String> {
    clean_name_for(raw, cfg!(windows))
}

fn clean_name_for(raw: &str, windows: bool) -> Option<String> {
    let base = raw
        .rsplit(['/', '\\'])
        .map(str::trim)
        .find(|component| !component.is_empty())?;
    if base == "." || base == ".." {
        return None;
    }
    let name: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let name = if windows { windows_safe(&name) } else { name };
    (!name.is_empty()).then_some(name)
}

/// Replace what Windows refuses in a file name: reserved characters, trailing
/// dots and spaces, and the device names (`CON`, `NUL`, `COM1`, ...).
fn windows_safe(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    safe.truncate(safe.trim_end_matches(['.', ' ']).len());
    let stem = safe.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(stem.as_bytes(), [_, _, _, b'1'..=b'9']);
    if numbered_device || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        safe.insert(0, '_');
    }
    safe
}

/// The usual extension for the media types agents commonly send.
fn extension_for(media_type: &str) -> Option<&'static str> {
    const EXTENSIONS: &[(&str, &str)] = &[
        ("text/plain", ".txt"),
        ("text/html", ".html"),
        ("text/csv", ".csv"),
        ("text/markdown", ".md"),
        ("application/json", ".json"),
        ("application/pdf", ".pdf"),
        ("application/xml", ".xml"),
        ("application/zip", ".zip"),
        ("application/gzip", ".gz"),
        ("image/png", ".png"),
        ("image/jpeg", ".jpg"),
        ("image/gif", ".gif"),
        ("image/webp", ".webp"),
        ("image/svg+xml", ".svg"),
        ("audio/mpeg", ".mp3"),
        ("audio/wav", ".wav"),
        ("audio/ogg", ".ogg"),
        ("video/mp4", ".mp4"),
        ("video/webm", ".webm"),
    ];
    let essence = media_type.split(';').next()?.trim().to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == essence)
        .map(|(_, extension)| *extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::event::{TaskArtifactUpdateEvent, TaskStatusUpdateEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh directory per test: runs in parallel, and across processes.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "a2acli-fileparts-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn saver(&self) -> FilePartSaver {
            FilePartSaver::new(self.0.clone())
        }

        fn read(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.0.join(name)).unwrap()
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn raw(bytes: &[u8], filename: Option<&str>, media_type: Option<&str>) -> Part {
        Part {
            content: PartContent::Raw(bytes.to_vec()),
            filename: filename.map(str::to_string),
            media_type: media_type.map(str::to_string),
            metadata: None,
        }
    }

    fn artifact(id: &str, name: Option<&str>, parts: Vec<Part>) -> Artifact {
        Artifact {
            artifact_id: id.to_string(),
            name: name.map(str::to_string),
            description: None,
            parts,
            metadata: None,
            extensions: None,
        }
    }

    fn message(id: &str, parts: Vec<Part>) -> Message {
        Message {
            message_id: id.to_string(),
            context_id: None,
            task_id: None,
            role: a2a::Role::Agent,
            parts,
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        }
    }

    #[test]
    fn test_a_message_part_is_saved_under_its_filename() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        saver
            .save_message(&message("m1", vec![raw(b"abc", Some("a.bin"), None)]))
            .unwrap();

        assert_eq!(scratch.read("a.bin"), b"abc");
        assert_eq!(saver.take_created(), vec![scratch.0.join("a.bin")]);
        assert!(saver.take_created().is_empty());
    }

    #[test]
    fn test_only_raw_parts_are_saved() {
        let scratch = Scratch::new();
        let parts = vec![
            Part::text("hello"),
            Part {
                content: PartContent::Url("https://example.com/x.pdf".to_string()),
                filename: Some("x.pdf".to_string()),
                media_type: None,
                metadata: None,
            },
            Part {
                content: PartContent::Data(serde_json::json!({"a": 1})),
                filename: Some("d.json".to_string()),
                media_type: None,
                metadata: None,
            },
        ];
        scratch.saver().save_message(&message("m1", parts)).unwrap();

        assert!(scratch.files().is_empty());
    }

    #[test]
    fn test_an_artifact_name_is_the_fallback_and_the_media_type_gives_the_extension() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        saver
            .save_artifact(
                &artifact(
                    "a1",
                    Some("report"),
                    vec![raw(b"%PDF", None, Some("application/pdf; charset=binary"))],
                ),
                false,
            )
            .unwrap();
        saver
            .save_artifact(
                &artifact("a2", None, vec![raw(b"x", None, Some("IMAGE/PNG"))]),
                false,
            )
            .unwrap();

        assert_eq!(scratch.files(), vec!["a2.png", "report.pdf"]);
    }

    #[test]
    fn test_a_filename_of_only_dots_falls_back() {
        let scratch = Scratch::new();
        scratch
            .saver()
            .save_message(&message("m1", vec![raw(b"x", Some(".."), None)]))
            .unwrap();

        assert_eq!(scratch.files(), vec!["status-msg"]);
        assert_eq!(resolve_name(&raw(b"", Some("."), None), ""), "filepart");
    }

    #[test]
    fn test_parts_sharing_a_name_in_one_message_are_joined() {
        let scratch = Scratch::new();
        let parts = vec![
            raw(b"one-", Some("a.txt"), None),
            raw(b"other", Some("b.txt"), None),
            raw(b"two", Some("a.txt"), None),
        ];
        scratch.saver().save_message(&message("m1", parts)).unwrap();

        assert_eq!(scratch.read("a.txt"), b"one-two");
        assert_eq!(scratch.read("b.txt"), b"other");
    }

    #[test]
    fn test_chunks_of_one_artifact_are_appended_only_when_asked() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        let chunk = |bytes: &[u8]| artifact("a1", Some("log.txt"), vec![raw(bytes, None, None)]);

        saver.save_artifact(&chunk(b"one "), false).unwrap();
        saver.save_artifact(&chunk(b"two "), true).unwrap();
        saver.save_artifact(&chunk(b"three"), true).unwrap();
        assert_eq!(scratch.read("log.txt"), b"one two three");

        // Without append the same artifact replaces what it wrote.
        saver.save_artifact(&chunk(b"fresh"), false).unwrap();
        assert_eq!(scratch.read("log.txt"), b"fresh");
        assert_eq!(scratch.files(), vec!["log.txt"]);
    }

    #[test]
    fn test_a_first_chunk_marked_append_creates_the_file() {
        let scratch = Scratch::new();
        scratch
            .saver()
            .save_artifact(
                &artifact("a1", Some("log.txt"), vec![raw(b"x", None, None)]),
                true,
            )
            .unwrap();

        assert_eq!(scratch.read("log.txt"), b"x");
    }

    #[test]
    fn test_another_artifact_with_the_same_name_is_kept_beside_the_first() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        for (id, body) in [("a1", b"v1"), ("a2", b"v2"), ("a3", b"v3")] {
            saver
                .save_artifact(
                    &artifact(id, Some("doc.txt"), vec![raw(body, None, None)]),
                    false,
                )
                .unwrap();
        }

        assert_eq!(
            scratch.files(),
            vec!["doc (1).txt", "doc (2).txt", "doc.txt"]
        );
        assert_eq!(scratch.read("doc.txt"), b"v1");
        assert_eq!(scratch.read("doc (1).txt"), b"v2");
        assert_eq!(scratch.read("doc (2).txt"), b"v3");
    }

    #[test]
    fn test_a_file_from_an_earlier_run_is_overwritten_once() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("a.txt"), b"older and longer").unwrap();
        let mut saver = scratch.saver();
        saver
            .save_message(&message("m1", vec![raw(b"new", Some("a.txt"), None)]))
            .unwrap();

        assert_eq!(scratch.read("a.txt"), b"new");
        assert_eq!(scratch.files(), vec!["a.txt"]);
    }

    #[test]
    fn test_a_task_saves_its_artifacts_and_its_status_message() {
        let scratch = Scratch::new();
        let task = Task {
            id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: a2a::TaskStatus {
                state: a2a::TaskState::Completed,
                message: Some(message("m1", vec![raw(b"said", Some("note.txt"), None)])),
                timestamp: None,
            },
            artifacts: Some(vec![artifact(
                "a1",
                Some("out.bin"),
                vec![raw(b"made", None, None)],
            )]),
            history: None,
            metadata: None,
        };
        let mut saver = scratch.saver();
        task.save_to(&mut saver).unwrap();

        assert_eq!(scratch.read("out.bin"), b"made");
        assert_eq!(scratch.read("note.txt"), b"said");
    }

    #[test]
    fn test_the_same_task_seen_again_rewrites_rather_than_duplicates() {
        let scratch = Scratch::new();
        let make = |body: &[u8]| Task {
            id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: a2a::TaskStatus {
                state: a2a::TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: Some(vec![artifact(
                "a1",
                Some("out.bin"),
                vec![raw(body, None, None)],
            )]),
            history: None,
            metadata: None,
        };
        let mut saver = scratch.saver();
        make(b"partial").save_to(&mut saver).unwrap();
        make(b"complete").save_to(&mut saver).unwrap();

        assert_eq!(scratch.read("out.bin"), b"complete");
        assert_eq!(scratch.files(), vec!["out.bin"]);
    }

    #[test]
    fn test_a_directory_in_the_filename_never_leaves_the_target_directory() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        for (index, name) in [
            "../../escape.txt",
            "/etc/passwd",
            "a/b/c.txt",
            "..\\..\\win.txt",
            "C:\\Users\\x\\drive.txt",
        ]
        .into_iter()
        .enumerate()
        {
            saver
                .save_message(&message(
                    &format!("m{index}"),
                    vec![raw(b"x", Some(name), None)],
                ))
                .unwrap();
        }

        let files = scratch.files();
        for expected in ["escape.txt", "passwd", "c.txt", "win.txt", "drive.txt"] {
            assert!(
                files.iter().any(|f| f == expected),
                "{expected} in {files:?}"
            );
        }
        // Nothing was written above the target directory.
        let parent = scratch.0.parent().unwrap();
        assert!(!parent.join("escape.txt").exists());
    }

    #[test]
    fn test_clean_name() {
        assert_eq!(clean_name("plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(clean_name("  spaced.txt  ").as_deref(), Some("spaced.txt"));
        assert_eq!(clean_name("dir/").as_deref(), Some("dir"));
        assert_eq!(clean_name("a/b/c.txt").as_deref(), Some("c.txt"));
        assert_eq!(clean_name("a\\b\\c.txt").as_deref(), Some("c.txt"));
        assert_eq!(clean_name("x/..").as_deref(), None);
        assert_eq!(clean_name(".").as_deref(), None);
        assert_eq!(clean_name("   ").as_deref(), None);
        assert_eq!(clean_name("").as_deref(), None);
        assert_eq!(clean_name("bad\0name").as_deref(), Some("bad_name"));
    }

    #[test]
    fn test_clean_name_for_windows_also_replaces_what_windows_refuses() {
        assert_eq!(clean_name_for("a:b.txt", true).as_deref(), Some("a_b.txt"));
        assert_eq!(clean_name_for("a:b.txt", false).as_deref(), Some("a:b.txt"));
        assert_eq!(clean_name_for("NUL", true).as_deref(), Some("_NUL"));
        assert_eq!(clean_name_for("...", true).as_deref(), None);
    }

    #[test]
    fn test_every_kind_of_response_is_saved() {
        let scratch = Scratch::new();
        let mut saver = scratch.saver();
        let part = |body: &[u8], name: &str| vec![raw(body, Some(name), None)];
        let status_update = |message: Option<Message>| {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "t1".to_string(),
                context_id: "c1".to_string(),
                status: a2a::TaskStatus {
                    state: a2a::TaskState::Working,
                    message,
                    timestamp: None,
                },
                metadata: None,
            })
        };

        message("m1", part(b"1", "message.txt"))
            .save_to(&mut saver)
            .unwrap();
        SendMessageResponse::Message(message("m2", part(b"2", "reply.txt")))
            .save_to(&mut saver)
            .unwrap();
        StreamResponse::Message(message("m3", part(b"3", "streamed.txt")))
            .save_to(&mut saver)
            .unwrap();
        status_update(Some(message("m4", part(b"4", "status.txt"))))
            .save_to(&mut saver)
            .unwrap();
        status_update(None).save_to(&mut saver).unwrap();
        StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: "t1".to_string(),
            context_id: "c1".to_string(),
            artifact: artifact("a1", Some("chunk.txt"), vec![raw(b"5", None, None)]),
            append: None,
            last_chunk: None,
            metadata: None,
        })
        .save_to(&mut saver)
        .unwrap();

        assert_eq!(
            scratch.files(),
            vec![
                "chunk.txt",
                "message.txt",
                "reply.txt",
                "status.txt",
                "streamed.txt"
            ]
        );
        assert_eq!(scratch.read("reply.txt"), b"2");
    }

    #[test]
    fn test_windows_safe() {
        assert_eq!(windows_safe("a:b?.txt"), "a_b_.txt");
        assert_eq!(windows_safe("report. "), "report");
        assert_eq!(windows_safe("NUL"), "_NUL");
        assert_eq!(windows_safe("nul.txt"), "_nul.txt");
        assert_eq!(windows_safe("com1"), "_com1");
        assert_eq!(windows_safe("LPT9.log"), "_LPT9.log");
        assert_eq!(windows_safe("com0"), "com0");
        assert_eq!(windows_safe("console.txt"), "console.txt");
        assert_eq!(windows_safe("..."), "");
    }

    #[test]
    fn test_numbered() {
        assert_eq!(numbered("doc.txt", 1), "doc (1).txt");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered("README", 3), "README (3)");
    }

    #[test]
    fn test_extension_for_ignores_case_and_parameters() {
        assert_eq!(
            extension_for("Application/JSON; charset=utf-8"),
            Some(".json")
        );
        assert_eq!(extension_for("image/jpeg"), Some(".jpg"));
        assert_eq!(extension_for("application/x-unknown"), None);
    }

    #[test]
    fn test_an_unwritable_target_reports_the_path() {
        let scratch = Scratch::new();
        // A regular file where the directory should be.
        let blocker = scratch.0.join("blocked");
        std::fs::write(&blocker, b"").unwrap();
        let mut saver = FilePartSaver::new(blocker.clone());
        let error = saver
            .save_message(&message("m1", vec![raw(b"x", Some("a.txt"), None)]))
            .unwrap_err();

        assert_eq!(error.path, blocker.join("a.txt"));
        assert!(saver.take_created().is_empty());
    }
}
