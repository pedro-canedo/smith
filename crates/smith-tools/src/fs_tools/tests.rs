use super::*;
// The path jail moved to `fs_tools/jail.rs`; this reaches it by path.
use super::jail::lexical_normalize;
use tokio_util::sync::CancellationToken;

mod read;
mod readset;
mod write;

// Fixtures, and the path-jail tests — those exercise fs_tools/jail.rs,
// which every one of the tools below leans on.

fn ctx(dir: &tempfile::TempDir) -> ToolContext {
    ToolContext::new(dir.path(), "test-session")
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

/// The four file tools sharing one read-set, exactly as
/// `ToolRegistry::with_builtin_tools` wires them. A test that reads with
/// one set and writes with another is testing nothing, so anything about
/// the read gate holds on to a single `files()`.
struct Files {
    read: ReadFileTool,
    write: WriteFileTool,
    edit: EditFileTool,
    multi: MultiEditTool,
}

fn files() -> Files {
    let reads = Arc::new(ReadSet::new());
    Files {
        read: ReadFileTool::new(reads.clone()),
        write: WriteFileTool::new(reads.clone()),
        edit: EditFileTool::new(reads.clone()),
        multi: MultiEditTool::new(reads),
    }
}

/// `read_file` numbers lines by default; tests that care about the bytes
/// rather than the presentation turn it off.
async fn read_raw(ctx: &ToolContext, path: &str) -> String {
    files()
        .read
        .execute(
            serde_json::json!({"path": path, "line_numbers": false}),
            ctx,
            cancel(),
        )
        .await
        .content
}

/// The three write tools must agree on what counts as scratch-confined —
/// they share `scratch_confined`, and this pins the contract for all of
/// them at once.
#[test]
fn scratch_scoped_accepts_paths_inside_the_session_scratch_dir() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let f = files();

    let relative = serde_json::json!({"path": ".smith/scratch/test-session/probe.sh"});
    let absolute = serde_json::json!({
        "path": dir.path().join(".smith/scratch/test-session/data.json").to_string_lossy()
    });
    for input in [&relative, &absolute] {
        assert!(f.write.scratch_scoped(input, &ctx), "write: {input}");
        assert!(f.edit.scratch_scoped(input, &ctx), "edit: {input}");
        assert!(f.multi.scratch_scoped(input, &ctx), "multi: {input}");
    }
}

#[test]
fn scratch_scoped_refuses_everything_that_is_not_this_sessions_scratch() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let f = files();

    for path in [
        // An ordinary project file: the whole point of the exemption is
        // that it never applies here.
        "src/main.rs",
        // Another session's scratch is another session's business.
        ".smith/scratch/other-session/probe.sh",
        // A lexical escape back out of scratch must not keep the waiver.
        ".smith/scratch/test-session/../../../back-in-the-project.txt",
        // The scratch *root* is shared between sessions, not scratch.
        ".smith/scratch/loose-file.txt",
    ] {
        let input = serde_json::json!({"path": path});
        assert!(!f.write.scratch_scoped(&input, &ctx), "{path}");
    }
    // No path at all: nothing to vouch for.
    assert!(!f.write.scratch_scoped(&serde_json::json!({}), &ctx));
}

/// A symlink planted inside scratch pointing at the project must not turn
/// project writes prompt-free: resolution follows the link, the prefix
/// check fails, and the call falls back to an ordinary prompt.
#[cfg(unix)]
#[test]
fn scratch_scoped_refuses_a_symlink_escaping_the_scratch_dir() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let scratch = ctx.scratch_dir();
    std::fs::create_dir_all(&scratch).unwrap();
    std::os::unix::fs::symlink(dir.path(), scratch.join("escape")).unwrap();

    let input = serde_json::json!({"path": ".smith/scratch/test-session/escape/victim.txt"});
    assert!(!files().write.scratch_scoped(&input, &ctx));
}

// --- path jail -------------------------------------------------------
//
// Each of these used to succeed. They are the concrete escapes an agent
// (or a prompt-injected instruction inside a file it read) could use to
// reach outside the project it was pointed at.

#[tokio::test]
async fn read_file_refuses_an_absolute_path_outside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let result = files()
        .read
        .execute(
            serde_json::json!({"path": "/etc/passwd"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
    assert!(result.content.contains("outside the project directory"));
}

#[tokio::test]
async fn read_file_refuses_a_parent_traversal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inside.txt"), "ok").unwrap();
    let result = files()
        .read
        .execute(
            serde_json::json!({"path": "../../../../etc/passwd"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
}

/// `starts_with` is component-wise, so a traversal that dips back inside
/// the root would pass a naive prefix check: `<root>/a/../../etc/passwd`
/// literally begins with `<root>`.
#[tokio::test]
async fn read_file_refuses_a_traversal_that_re_enters_the_root_textually() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    let result = files()
        .read
        .execute(
            serde_json::json!({"path": "a/../../etc/passwd"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
}

#[tokio::test]
async fn write_file_refuses_to_escape_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let result = files()
        .write
        .execute(
            serde_json::json!({"path": "../escaped.txt", "content": "nope"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
    assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
}

#[tokio::test]
async fn glob_refuses_a_pattern_that_escapes_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let result = GlobTool
        .execute(
            serde_json::json!({"pattern": "../../*.rs"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
    assert!(result.content.contains("outside the project directory"));
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_refuses_a_symlink_pointing_out_of_the_project() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "classified").unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("link.txt"),
    )
    .unwrap();

    let result = files()
        .read
        .execute(
            serde_json::json!({"path": "link.txt"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "got: {}", result.content);
    assert!(!result.content.contains("classified"));
}

#[cfg(unix)]
#[tokio::test]
async fn glob_skips_matches_that_escape_through_a_symlink() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "classified").unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mine.rs"), "ok").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("elsewhere")).unwrap();

    let result = GlobTool
        .execute(
            serde_json::json!({"pattern": "**/*.rs"}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(!result.is_error, "got: {}", result.content);
    assert!(result.content.contains("mine.rs"));
    assert!(
        !result.content.contains("secret.rs"),
        "glob leaked outside the project: {}",
        result.content
    );
}

#[tokio::test]
async fn normal_paths_inside_the_project_still_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

    // Relative, nested-relative, `.`-prefixed and absolute-inside all
    // have to keep working — the jail must not cost normal usage.
    for path in [
        "src/main.rs",
        "./src/main.rs",
        "src/../src/main.rs",
        dir.path().join("src/main.rs").to_str().unwrap(),
    ] {
        let result = files()
            .read
            .execute(serde_json::json!({"path": path}), &ctx(&dir), cancel())
            .await;
        assert!(!result.is_error, "{path} was rejected: {}", result.content);
        assert!(result.content.contains("fn main"));
    }
}

#[test]
fn lexical_normalize_resolves_parent_segments() {
    assert_eq!(
        lexical_normalize(Path::new("/a/b/../c")),
        PathBuf::from("/a/c")
    );
    assert_eq!(
        lexical_normalize(Path::new("/a/./b")),
        PathBuf::from("/a/b")
    );
    // Climbing past the root stays at the root rather than going negative.
    assert_eq!(lexical_normalize(Path::new("/../..")), PathBuf::from("/"));
}

#[test]
fn clip_line_cuts_on_a_char_boundary() {
    // `€` is three bytes wide: a byte-indexed cut lands mid-character and
    // panics — the same bug `shell_tool::truncate_tail` was written for.
    let (out, clipped) = clip_line(&"€".repeat(10), 4);
    assert!(clipped);
    assert_eq!(out.matches('€').count(), 4);
    assert!(out.contains("line clipped"));

    let (out, clipped) = clip_line("short", 400);
    assert!(!clipped);
    assert_eq!(out, "short");
}

// --- write_file's read gate ------------------------------------------
//
// The failure this exists for: a model working from a stale summary (or
// its own imagination) replaces a file it has never seen, and the loss is
// only noticed hours later. `/rewind` can undo it; not doing it is
// better.

async fn write(files: &Files, ctx: &ToolContext, path: &str, content: &str) -> ToolResult {
    files
        .write
        .execute(
            serde_json::json!({"path": path, "content": content}),
            ctx,
            cancel(),
        )
        .await
}

async fn read(files: &Files, ctx: &ToolContext, extra: serde_json::Value) -> ToolResult {
    files.read.execute(extra, ctx, cancel()).await
}

/// The judgement call, pinned: a clipped line means the model was shown
/// characters the file does not contain.
#[tokio::test]
async fn a_clipped_line_does_not_count_as_having_read_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("min.js"), "x".repeat(MAX_LINE_CHARS * 3)).unwrap();

    let files = files();
    let shown = read(&files, &ctx, serde_json::json!({"path": "min.js"})).await;
    assert!(shown.content.contains("line clipped"), "{}", shown.content);

    let refused = write(&files, &ctx, "min.js", "replaced").await;
    assert!(refused.is_error, "{}", refused.content);
}

/// `ReadOnly` tools run concurrently now (`Agent::run_concurrent_group`),
/// so the read-set really is written from several tasks at once. Each
/// task reads a different slice of the same file: if two of them raced,
/// the merged coverage would come out short and the write below would be
/// refused as partial.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reads_of_one_file_add_up_without_racing() {
    const CHUNK: usize = 25;
    const CHUNKS: usize = 32;

    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let body: String = (1..=CHUNK * CHUNKS)
        .map(|n| format!("line {n}\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), &body).unwrap();

    let reads = Arc::new(ReadSet::new());
    let read = Arc::new(ReadFileTool::new(reads.clone()));
    let write_tool = WriteFileTool::new(reads);

    let mut tasks = Vec::new();
    for chunk in 0..CHUNKS {
        let read = read.clone();
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            read.execute(
                serde_json::json!({
                    "path": "big.txt",
                    "offset": chunk * CHUNK + 1,
                    "limit": CHUNK
                }),
                &ctx,
                cancel(),
            )
            .await
        }));
    }
    for task in tasks {
        let result = task.await.unwrap();
        assert!(!result.is_error, "{}", result.content);
    }

    let result = write_tool
        .execute(
            serde_json::json!({"path": "big.txt", "content": "replaced"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(
        !result.is_error,
        "concurrent reads lost coverage: {}",
        result.content
    );
}

/// The same race across distinct files, which is the shape a real turn
/// produces: one round of parallel `read_file`s, then writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reads_of_many_files_all_land() {
    const FILES: usize = 48;

    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    for n in 0..FILES {
        std::fs::write(dir.path().join(format!("f{n}.txt")), format!("body {n}\n")).unwrap();
    }

    let reads = Arc::new(ReadSet::new());
    let read = Arc::new(ReadFileTool::new(reads.clone()));
    let write_tool = WriteFileTool::new(reads);

    let mut tasks = Vec::new();
    for n in 0..FILES {
        let read = read.clone();
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            read.execute(
                serde_json::json!({"path": format!("f{n}.txt")}),
                &ctx,
                cancel(),
            )
            .await
        }));
    }
    for task in tasks {
        assert!(!task.await.unwrap().is_error);
    }

    for n in 0..FILES {
        let result = write_tool
            .execute(
                serde_json::json!({"path": format!("f{n}.txt"), "content": "replaced"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!result.is_error, "f{n}.txt: {}", result.content);
    }
}

#[test]
fn coverage_ranges_merge_no_matter_what_order_they_arrive_in() {
    let set = ReadSet::new();
    let path = Path::new("/p/a.rs");
    for range in [(4, 6), (0, 2), (2, 4)] {
        set.record_read("s", path, "hash", 6, range);
    }
    assert_eq!(set.knowledge("s", path, "hash"), Knowledge::Whole);

    // A gap in the middle is a gap, however many ranges surround it.
    let set = ReadSet::new();
    set.record_read("s", path, "hash", 6, (0, 2));
    set.record_read("s", path, "hash", 6, (4, 6));
    assert_eq!(
        set.knowledge("s", path, "hash"),
        Knowledge::Partial {
            read_to: 2,
            total: 6
        }
    );
}

/// Coverage belongs to the bytes it was read from: ranges recorded
/// against content that has since changed describe lines that may not
/// exist any more, so they are dropped rather than merged.
#[test]
fn a_new_hash_discards_the_coverage_recorded_against_the_old_one() {
    let set = ReadSet::new();
    let path = Path::new("/p/a.rs");
    set.record_read("s", path, "before", 4, (0, 4));
    set.record_read("s", path, "after", 4, (2, 4));

    assert_eq!(set.knowledge("s", path, "before"), Knowledge::Stale);
    assert_eq!(set.knowledge("s", path, "after"), Knowledge::Unread);
}
