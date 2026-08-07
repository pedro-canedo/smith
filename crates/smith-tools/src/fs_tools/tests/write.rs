//! `write_file`, `edit_file` and `multi_edit`, and the staging they use.

use super::*;

#[tokio::test]
async fn write_file_leaves_no_staging_residue_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);

    let write = files()
        .write
        .execute(
            serde_json::json!({"path": "nested/out.txt", "content": "data"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!write.is_error, "{}", write.content);
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("nested/out.txt"))
            .await
            .unwrap(),
        "data"
    );
    let staged = dir
        .path()
        .join(".smith/staging/test-session/nested/out.txt");
    assert!(
        !staged.is_file(),
        "staging file should be removed after apply"
    );
}

#[tokio::test]
async fn write_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);

    let write = files()
        .write
        .execute(
            serde_json::json!({"path": "a.txt", "content": "hello\nworld"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!write.is_error, "{}", write.content);
    assert_eq!(read_raw(&ctx, "a.txt").await, "hello\nworld");
}

#[tokio::test]
async fn edit_file_requires_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "foo foo").unwrap();

    let ambiguous = files()
        .edit
        .execute(
            serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(ambiguous.is_error);
    assert!(ambiguous.content.contains("replace_all"));

    std::fs::write(dir.path().join("a.txt"), "foo baz").unwrap();
    let edited = files()
        .edit
        .execute(
            serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);
    assert_eq!(read_raw(&ctx, "a.txt").await, "bar baz");
}

#[tokio::test]
async fn edit_file_replace_all_changes_every_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "foo foo foo").unwrap();

    let edited = files()
        .edit
        .execute(
            serde_json::json!({
                "path": "a.txt", "old_str": "foo", "new_str": "bar", "replace_all": true
            }),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);
    assert!(
        edited.content.contains("3 occurrences"),
        "{}",
        edited.content
    );
    assert_eq!(read_raw(&ctx, "a.txt").await, "bar bar bar");
}

#[tokio::test]
async fn edit_file_errors_when_old_str_missing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

    let result = files()
        .edit
        .execute(
            serde_json::json!({"path": "a.txt", "old_str": "nope", "new_str": "x"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(result.is_error);
}

// --- multi_edit ------------------------------------------------------

#[tokio::test]
async fn multi_edit_applies_every_edit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.rs"), "let a = 1;\nlet b = 2;\n").unwrap();

    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": "a.rs", "edits": [
                {"old_str": "let a = 1;", "new_str": "let a = 10;"},
                {"old_str": "let b = 2;", "new_str": "let b = 20;"}
            ]}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(read_raw(&ctx, "a.rs").await, "let a = 10;\nlet b = 20;");
}

/// The semantics decision, pinned: edit 2 sees edit 1's output. Applying
/// both against the original would either lose one of these or splice
/// them, and would do it silently.
#[tokio::test]
async fn multi_edit_applies_edits_sequentially_not_against_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();

    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": "a.txt", "edits": [
                {"old_str": "alpha", "new_str": "beta"},
                {"old_str": "beta", "new_str": "gamma"}
            ]}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(read_raw(&ctx, "a.txt").await, "gamma");
}

#[tokio::test]
async fn a_failing_edit_leaves_the_file_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let before = "one\ntwo\nthree\n";
    std::fs::write(dir.path().join("a.txt"), before).unwrap();

    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": "a.txt", "edits": [
                {"old_str": "one", "new_str": "1"},
                {"old_str": "not present", "new_str": "x"},
                {"old_str": "three", "new_str": "3"}
            ]}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("edit 2"), "{}", result.content);
    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).unwrap(),
        before.as_bytes(),
        "a failed batch modified the file"
    );
}

#[tokio::test]
async fn multi_edit_reports_which_edit_became_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    // After edit 1 there are two `x`s, so edit 2 is ambiguous — a case
    // that only exists because edits compose sequentially, and one that
    // has to fail loudly rather than pick one.
    let before = "x\ny\n";
    std::fs::write(dir.path().join("a.txt"), before).unwrap();

    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": "a.txt", "edits": [
                {"old_str": "y", "new_str": "x"},
                {"old_str": "x", "new_str": "z"}
            ]}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("not unique"), "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        before
    );
}

#[tokio::test]
async fn multi_edit_rejects_an_empty_batch() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": "a.txt", "edits": []}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(result.is_error);
}

#[tokio::test]
async fn multi_edit_refuses_to_escape_the_project() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("target.txt"), "untouched").unwrap();
    let dir = tempfile::tempdir().unwrap();

    let escape = format!("../{}/target.txt", {
        let name = outside.path().file_name().unwrap();
        name.to_string_lossy().into_owned()
    });
    let result = files()
        .multi
        .execute(
            serde_json::json!({"path": escape, "edits": [
                {"old_str": "untouched", "new_str": "touched"}
            ]}),
            &ctx(&dir),
            cancel(),
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
        "untouched"
    );
}

#[test]
fn substitute_refuses_a_no_op_edit() {
    assert!(substitute("abc", "a", "a", false, "").is_err());
    assert!(substitute("abc", "", "x", false, "").is_err());
}

#[tokio::test]
async fn write_file_refuses_to_overwrite_a_file_that_was_never_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    let before = "the user's actual work\n";
    std::fs::write(dir.path().join("a.txt"), before).unwrap();

    let files = files();
    let result = write(&files, &ctx, "a.txt", "hallucinated replacement").await;

    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "a.txt already exists and has not been read this session — call read_file on it \
             first, then write_file, or use edit_file to change part of it without replacing \
             the whole file"
    );
    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).unwrap(),
        before.as_bytes(),
        "a refused write still changed the file"
    );
}

/// An `edit_file` that matched `old_str` proves the model knew that
/// snippet — the same three lines out of a thousand that grep gives it —
/// so it does not by itself license replacing the whole file.
#[tokio::test]
async fn a_successful_edit_does_not_by_itself_license_an_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();

    let files = files();
    let edited = files
        .edit
        .execute(
            serde_json::json!({"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"}),
            &ctx,
            cancel(),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);

    let refused = write(&files, &ctx, "a.txt", "replaced").await;
    assert!(refused.is_error, "{}", refused.content);
}

/// But an edit on top of a real read keeps the file writable: the model
/// read those bytes and authored the change, so it knows the result. The
/// hash moved, and knowledge moves with it.
#[tokio::test]
async fn an_edit_carries_a_read_across_the_change_it_makes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();

    let files = files();
    assert!(
        !read(&files, &ctx, serde_json::json!({"path": "a.txt"}))
            .await
            .is_error
    );
    for tool in ["edit", "multi"] {
        let call = if tool == "edit" {
            files.edit.execute(
                serde_json::json!({"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"}),
                &ctx,
                cancel(),
            )
        } else {
            files.multi.execute(
                serde_json::json!({"path": "a.txt", "edits": [
                    {"old_str": "beta", "new_str": "BETA"}
                ]}),
                &ctx,
                cancel(),
            )
        };
        let edited = call.await;
        assert!(!edited.is_error, "{tool}: {}", edited.content);
    }

    let result = write(&files, &ctx, "a.txt", "replaced").await;
    assert!(!result.is_error, "{}", result.content);
}

/// A file smith itself just created is one the model has seen — refusing
/// the second write of a turn would be an obstacle with no safety in it.
#[tokio::test]
async fn write_file_can_replace_what_it_just_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(&dir);

    let files = files();
    assert!(!write(&files, &ctx, "a.txt", "first").await.is_error);
    let again = write(&files, &ctx, "a.txt", "second").await;
    assert!(!again.is_error, "{}", again.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "second"
    );
}
