use super::*;

/// A scope confined to a temp dir: global at `<tmp>/global`, project at
/// `<tmp>/project`. Keeps tests off the real `~/.smith`.
struct Fixture {
    _tmp: tempfile::TempDir,
    global: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let root = tmp.path().join("project");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _tmp: tmp,
            global,
            root,
        }
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn write_global(&self, content: &str) {
        std::fs::write(self.global.join(MEMORY_FILE_NAME), content).unwrap();
    }

    fn scope(&self, rel_cwd: &str) -> MemoryScope {
        let cwd = if rel_cwd.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel_cwd)
        };
        std::fs::create_dir_all(&cwd).unwrap();
        MemoryScope::new(Some(self.global.clone()), self.root.clone(), cwd)
    }
}

// --- layering ---------------------------------------------------------

#[test]
fn no_memory_anywhere_renders_nothing() {
    let fx = Fixture::new();
    let memory = load(&fx.scope("crates/a"));
    assert!(memory.is_empty(), "unexpected: {}", memory.text);
    assert!(memory.omitted.is_empty());
    // The candidates are still watched, so a file created later is seen.
    assert!(memory.watched.iter().any(|p| p.ends_with(MEMORY_FILE_NAME)));
}

#[test]
fn a_missing_layer_between_two_present_ones_is_fine() {
    let fx = Fixture::new();
    fx.write_global("global rule");
    // no SMITH.md at the project root
    fx.write("crates/a/SMITH.md", "crate rule");

    let text = load(&fx.scope("crates/a")).text;
    assert!(text.contains("global rule"));
    assert!(text.contains("crate rule"));
}

#[test]
fn all_three_layers_contribute_least_specific_first() {
    let fx = Fixture::new();
    fx.write_global("prefer terse commit messages");
    fx.write("SMITH.md", "this repo uses 4-space indent");
    fx.write("crates/a/SMITH.md", "this crate uses 2-space indent");

    let text = load(&fx.scope("crates/a")).text;

    let global = text.find("terse commit messages").expect("global layer");
    let root = text.find("4-space indent").expect("root layer");
    let nested = text.find("2-space indent").expect("nested layer");

    // Concatenation, not override: the repo-wide rule survives a crate
    // that overrides only the indent.
    assert!(global < root && root < nested, "wrong order in:\n{text}");
    assert!(text.contains("the later one wins"));
}

#[test]
fn every_directory_between_root_and_cwd_contributes() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "root rule");
    fx.write("crates/SMITH.md", "middle rule");
    fx.write("crates/a/src/SMITH.md", "leaf rule");

    let text = load(&fx.scope("crates/a/src")).text;
    let root = text.find("root rule").unwrap();
    let middle = text.find("middle rule").unwrap();
    let leaf = text.find("leaf rule").unwrap();
    assert!(root < middle && middle < leaf, "wrong order in:\n{text}");
}

#[test]
fn nothing_above_the_project_root_is_read() {
    let fx = Fixture::new();
    std::fs::write(fx.root.parent().unwrap().join(MEMORY_FILE_NAME), "outsider").unwrap();
    fx.write("SMITH.md", "root rule");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("root rule"));
    assert!(
        !text.contains("outsider"),
        "reached above the root:\n{text}"
    );
}

#[test]
fn an_empty_memory_file_contributes_nothing() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "   \n\n");
    assert!(load(&fx.scope("")).is_empty());
}

#[test]
fn discover_finds_the_root_by_marker() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.root.join(".git")).unwrap();
    std::fs::create_dir_all(fx.root.join("crates/a")).unwrap();

    let scope = MemoryScope::discover(fx.root.join("crates/a"));
    assert_eq!(scope.root, lexical_normalize(&fx.root));
}

#[test]
fn discover_without_a_marker_treats_the_cwd_as_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let deep = tmp.path().join("a/b");
    std::fs::create_dir_all(&deep).unwrap();
    let scope = MemoryScope::discover(&deep);
    assert_eq!(scope.root, lexical_normalize(&deep));
}

/// The same boundary, reached through a directory spelled differently.
///
/// Windows hands out both an 8.3 short name and a long one for the same
/// directory, and the two sources disagree about which: `temp_dir()`
/// answers short, `home_dir()` long. A lexical comparison therefore found
/// no boundary at all and walked straight past it — which is what CI kept
/// catching after the first fix looked right.
#[test]
fn the_boundary_is_a_directory_not_a_spelling() {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return;
    };
    // The temp directory is inside the profile on Windows and outside it
    // elsewhere; either way it must not resolve to a project root above.
    let deep = std::env::temp_dir().join("smith-boundary-probe/a/b");
    let scope = MemoryScope::discover(&deep);
    assert_ne!(
        scope.root,
        lexical_normalize(&home),
        "the walk reached the home directory"
    );
}

/// `~/.smith` is smith's own global directory and exists on every machine
/// that has run `smith setup`. Since `.smith` is a root marker, the walk
/// used to hand back `$HOME` as the project root for any project without a
/// `.git` — loading `~/SMITH.md` twice and widening the `@import` jail to
/// the whole home directory.
#[test]
fn the_home_directory_is_never_treated_as_a_project_root() {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return;
    };
    // A directory under the home that carries no marker of its own. Not
    // created on disk: `find_project_root` only asks whether markers
    // exist, and nothing here should match, which is the point.
    let deep = home.join("some-project-without-a-marker/src");
    let scope = MemoryScope::discover(&deep);
    assert_eq!(
        scope.root,
        lexical_normalize(&deep),
        "the walk escaped into the home directory"
    );
}

/// …but running smith *in* the home directory still works. The stop
/// applies to ancestors, not to the directory actually asked about.
#[test]
fn asking_about_the_home_directory_itself_still_answers() {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return;
    };
    let scope = MemoryScope::discover(&home);
    assert_eq!(scope.root, lexical_normalize(&home));
}

#[test]
fn the_chain_is_capped_but_never_drops_the_root() {
    let fx = Fixture::new();
    let deep: PathBuf = (0..40).fold(PathBuf::new(), |acc, i| acc.join(format!("d{i}")));
    let scope = fx.scope(deep.to_str().unwrap());
    assert!(scope.chain().len() <= MAX_CHAIN_DEPTH);
    assert_eq!(scope.chain()[0], lexical_normalize(&fx.root));
}

// --- @import ----------------------------------------------------------

#[test]
fn import_pulls_a_file_in() {
    let fx = Fixture::new();
    fx.write("docs/style.md", "always use tabs");
    fx.write("SMITH.md", "see the style guide\n@import docs/style.md\n");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("see the style guide"));
    assert!(
        text.contains("always use tabs"),
        "import not expanded:\n{text}"
    );
    assert!(text.contains("begin imported file"));
}

#[test]
fn import_accepts_quoted_paths_and_ignores_mid_sentence_mentions() {
    let fx = Fixture::new();
    fx.write("docs/a b.md", "quoted body");
    fx.write(
        "SMITH.md",
        "@import \"docs/a b.md\"\nyou can write @import somewhere.md inline\n",
    );

    let text = load(&fx.scope("")).text;
    assert!(text.contains("quoted body"));
    assert!(text.contains("you can write @import somewhere.md inline"));
}

#[test]
fn a_cycle_terminates_and_is_reported() {
    let fx = Fixture::new();
    fx.write("a.md", "alpha\n@import b.md\n");
    fx.write("b.md", "beta\n@import a.md\n");
    fx.write("SMITH.md", "@import a.md\n");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("alpha"));
    assert!(text.contains("beta"));
    assert!(
        text.contains("already included"),
        "cycle not reported:\n{text}"
    );
    // Included once each, not repeatedly.
    assert_eq!(text.matches("alpha").count(), 1);
    assert_eq!(text.matches("beta").count(), 1);
}

#[test]
fn a_self_import_terminates() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "solo\n@import SMITH.md\n");
    let text = load(&fx.scope("")).text;
    assert!(text.contains("already included"));
    assert_eq!(text.matches("solo").count(), 1);
}

#[test]
fn import_depth_is_bounded() {
    let fx = Fixture::new();
    // depth 0: SMITH.md -> 1: l1 -> 2: l2 -> 3: l3 -> refused
    fx.write("SMITH.md", "@import l1.md\n");
    fx.write("l1.md", "level one\n@import l2.md\n");
    fx.write("l2.md", "level two\n@import l3.md\n");
    fx.write("l3.md", "level three\n@import l4.md\n");
    fx.write("l4.md", "level four");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("level one"));
    assert!(text.contains("level two"));
    assert!(text.contains("level three"));
    assert!(
        !text.contains("level four"),
        "depth limit not enforced:\n{text}"
    );
    assert!(text.contains("import depth limit"));
}

#[test]
fn total_imported_files_are_bounded() {
    let fx = Fixture::new();
    let mut root = String::new();
    for i in 0..(MAX_IMPORTED_FILES + 5) {
        fx.write(&format!("n{i}.md"), &format!("body {i}"));
        root.push_str(&format!("@import n{i}.md\n"));
    }
    fx.write("SMITH.md", &root);

    let text = load(&fx.scope("")).text;
    assert!(
        text.contains("import limit"),
        "fan-out not bounded:\n{text}"
    );
    assert_eq!(
        text.matches("begin imported file").count(),
        MAX_IMPORTED_FILES
    );
}

#[test]
fn an_import_escaping_the_project_is_refused() {
    let fx = Fixture::new();
    let secret = fx.root.parent().unwrap().join("secret.md");
    std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
    fx.write("SMITH.md", "@import ../secret.md\n");

    let text = load(&fx.scope("")).text;
    assert!(!text.contains("SSH KEY MATERIAL"), "jail escaped:\n{text}");
    assert!(text.contains("resolves outside"));
}

#[test]
fn an_absolute_import_outside_the_project_is_refused() {
    let fx = Fixture::new();
    let secret = fx.root.parent().unwrap().join("secret.md");
    std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
    fx.write("SMITH.md", &format!("@import {}\n", secret.display()));

    let text = load(&fx.scope("")).text;
    assert!(!text.contains("SSH KEY MATERIAL"), "jail escaped:\n{text}");
}

#[test]
fn a_dotdot_import_that_lands_back_inside_the_project_is_allowed() {
    let fx = Fixture::new();
    fx.write("shared.md", "shared body");
    fx.write("crates/a/SMITH.md", "@import ../../shared.md\n");

    let text = load(&fx.scope("crates/a")).text;
    assert!(
        text.contains("shared body"),
        "legitimate import refused:\n{text}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_project_is_refused() {
    let fx = Fixture::new();
    let secret = fx.root.parent().unwrap().join("secret.md");
    std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
    std::os::unix::fs::symlink(&secret, fx.root.join("link.md")).unwrap();
    fx.write("SMITH.md", "@import link.md\n");

    let text = load(&fx.scope("")).text;
    assert!(
        !text.contains("SSH KEY MATERIAL"),
        "symlink escaped the jail:\n{text}"
    );
}

#[test]
fn the_global_layer_may_import_from_the_smith_directory_only() {
    let fx = Fixture::new();
    std::fs::write(fx.global.join("notes.md"), "global notes body").unwrap();
    std::fs::write(fx.root.join("project-secret.md"), "project body").unwrap();
    fx.write_global("@import notes.md\n@import ../project/project-secret.md\n");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("global notes body"));
    assert!(
        !text.contains("project body"),
        "global jail escaped:\n{text}"
    );
}

#[test]
fn a_missing_import_is_reported_not_silently_dropped() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "@import nope.md\n");
    let text = load(&fx.scope("")).text;
    assert!(text.contains("no such file"), "{text}");
}

#[test]
fn a_tilde_import_is_refused() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "@import ~/.ssh/id_rsa\n");
    let text = load(&fx.scope("")).text;
    assert!(text.contains("`~` paths are not supported"), "{text}");
}

// --- size cap ---------------------------------------------------------

#[test]
fn under_the_cap_nothing_is_dropped() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "short");
    let memory = load(&fx.scope(""));
    assert!(memory.omitted.is_empty());
    assert!(!memory.truncated);
}

#[test]
fn the_cap_drops_distant_layers_before_close_ones_and_says_so() {
    let fx = Fixture::new();
    let big = "x".repeat(MAX_MEMORY_BYTES);
    fx.write_global("global rule");
    fx.write("SMITH.md", &format!("root rule\n{big}"));
    fx.write("crates/a/SMITH.md", "crate rule");

    let memory = load(&fx.scope("crates/a"));

    // Most specific first: the crate layer fits, the huge root layer does
    // not, and the small global one still slips into what is left.
    assert!(memory.text.contains("crate rule"));
    assert!(memory.text.contains("global rule"));
    assert!(!memory.text.contains("root rule"), "{}", memory.text);
    assert_eq!(memory.omitted, vec![fx.root.join(MEMORY_FILE_NAME)]);
    assert!(!memory.truncated);
    // The drop is announced in the prompt: standing instructions going
    // missing without a word is the hazard this exists to avoid.
    assert!(memory.text.contains("were NOT loaded"), "{}", memory.text);
    assert!(memory
        .text
        .contains(&fx.root.join(MEMORY_FILE_NAME).display().to_string()));
}

#[test]
fn a_single_oversized_closest_layer_is_truncated_rather_than_dropped() {
    let fx = Fixture::new();
    let body = format!(
        "first line\n{}\nLAST LINE",
        "y".repeat(MAX_MEMORY_BYTES * 2)
    );
    fx.write("SMITH.md", &body);

    let memory = load(&fx.scope(""));
    assert!(memory.truncated);
    assert!(memory.text.contains("first line"), "everything was lost");
    assert!(!memory.text.contains("LAST LINE"));
    assert!(memory.text.contains("this file was cut off here"));
}

#[test]
fn truncate_at_boundary_prefers_a_newline_and_never_splits_a_char() {
    assert_eq!(truncate_at_boundary("abc\ndef", 5), 3);
    assert_eq!(truncate_at_boundary("abc", 10), 3);
    // 'é' occupies bytes 1..3 — cutting at 2 would be invalid.
    let s = "aéb";
    let n = truncate_at_boundary(s, 2);
    assert!(s.is_char_boundary(n));
}

// --- caching ----------------------------------------------------------

#[test]
fn the_cache_returns_the_same_text_until_a_file_changes() {
    let fx = Fixture::new();
    let path = fx.write("SMITH.md", "first version");
    let cache = MemoryCache::new(fx.scope(""));

    assert!(cache.render().contains("first version"));
    assert!(cache.render().contains("first version"));

    // Length differs too, so this is caught regardless of mtime
    // resolution on the test machine.
    std::fs::write(&path, "second version, longer than the first").unwrap();
    assert!(cache.render().contains("second version"));
}

#[test]
fn the_cache_notices_a_memory_file_created_mid_session() {
    let fx = Fixture::new();
    let cache = MemoryCache::new(fx.scope("crates/a"));
    assert_eq!(cache.render(), "");

    fx.write("crates/a/SMITH.md", "appeared later");
    assert!(cache.render().contains("appeared later"));
}

#[test]
fn the_cache_notices_a_memory_file_deleted_mid_session() {
    let fx = Fixture::new();
    let path = fx.write("SMITH.md", "here for now");
    let cache = MemoryCache::new(fx.scope(""));
    assert!(cache.render().contains("here for now"));

    std::fs::remove_file(&path).unwrap();
    assert_eq!(cache.render(), "");
}

#[test]
fn the_cache_notices_an_imported_file_changing() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "@import docs/style.md\n");
    let imported = fx.write("docs/style.md", "original guidance");
    let cache = MemoryCache::new(fx.scope(""));
    assert!(cache.render().contains("original guidance"));

    std::fs::write(&imported, "revised guidance, materially longer").unwrap();
    assert!(cache.render().contains("revised guidance"));
}

#[test]
fn invalidate_forces_a_reread() {
    let fx = Fixture::new();
    let cache = MemoryCache::new(fx.scope(""));
    assert_eq!(cache.render(), "");
    fx.write("SMITH.md", "new");
    cache.invalidate();
    assert!(cache.render().contains("new"));
}

// --- remember ---------------------------------------------------------

#[test]
fn remember_creates_the_file_with_a_header() {
    let fx = Fixture::new();
    let path = memory_path(&fx.root);
    remember(&path, "always run cargo fmt before committing").unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# Project memory"));
    assert!(text.contains("- always run cargo fmt before committing"));
}

#[test]
fn remember_appends_without_disturbing_existing_content() {
    let fx = Fixture::new();
    let path = fx.write("SMITH.md", "# Hand written\n\nkeep me");
    remember(&path, "and this").unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# Hand written"));
    assert!(text.contains("keep me"));
    assert!(text.trim_end().ends_with("- and this"));
}

#[test]
fn remember_keeps_a_multiline_note_as_one_item() {
    let fx = Fixture::new();
    let path = memory_path(&fx.root);
    remember(&path, "first line\nsecond line").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("- first line\n  second line\n"), "{text}");
}

#[test]
fn remember_refuses_an_empty_note() {
    let fx = Fixture::new();
    let path = memory_path(&fx.root);
    assert!(matches!(
        remember(&path, "   \n "),
        Err(ConfigError::EmptyNote)
    ));
    assert!(!path.exists());
}

#[test]
fn a_remembered_note_is_loaded_back_as_memory() {
    let fx = Fixture::new();
    remember(&memory_path(&fx.root), "the build needs nightly").unwrap();
    assert!(load(&fx.scope("")).text.contains("the build needs nightly"));
}

// --- AGENTS.md fallback -----------------------------------------------

#[test]
fn agents_md_is_loaded_when_smith_md_is_absent() {
    let fx = Fixture::new();
    std::fs::write(
        fx.global.join(FALLBACK_MEMORY_FILE_NAME),
        "global agents rule",
    )
    .unwrap();
    fx.write("AGENTS.md", "root agents rule");
    fx.write("crates/a/AGENTS.md", "crate agents rule");

    let text = load(&fx.scope("crates/a")).text;
    assert!(text.contains("global agents rule"), "{text}");
    assert!(text.contains("root agents rule"));
    assert!(text.contains("crate agents rule"));
}

#[test]
fn smith_md_wins_over_agents_md_in_the_same_directory() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "SMITH RULE");
    fx.write("AGENTS.md", "AGENTS RULE");

    let text = load(&fx.scope("")).text;
    assert!(text.contains("SMITH RULE"));
    assert!(
        !text.contains("AGENTS RULE"),
        "both files of one layer loaded: {text}"
    );
}

/// The fallback is per layer, not global: a nested dir with only an
/// AGENTS.md still contributes even when the root has a SMITH.md.
#[test]
fn the_fallback_is_decided_layer_by_layer() {
    let fx = Fixture::new();
    fx.write("SMITH.md", "root smith rule");
    fx.write("crates/a/AGENTS.md", "crate agents rule");

    let text = load(&fx.scope("crates/a")).text;
    assert!(text.contains("root smith rule"));
    assert!(text.contains("crate agents rule"));
}

#[test]
fn the_section_header_names_the_agents_file_that_was_loaded() {
    let fx = Fixture::new();
    fx.write("AGENTS.md", "agents rule");
    let text = load(&fx.scope("")).text;
    assert!(text.contains("AGENTS.md (project root)"), "{text}");
}

/// The fingerprint claim, asserted end to end: the cache watches the
/// SMITH.md candidate even while serving the AGENTS.md, so creating one
/// mid-session displaces the fallback on the next render.
#[test]
fn a_smith_md_created_mid_session_displaces_the_agents_md() {
    let fx = Fixture::new();
    fx.write("AGENTS.md", "AGENTS RULE");
    let cache = MemoryCache::new(fx.scope(""));
    assert!(cache.render().contains("AGENTS RULE"));

    fx.write("SMITH.md", "SMITH RULE");
    let text = cache.render();
    assert!(text.contains("SMITH RULE"), "{text}");
    assert!(!text.contains("AGENTS RULE"), "{text}");
}

#[cfg(unix)]
#[test]
fn an_import_inside_agents_md_stays_jailed() {
    let fx = Fixture::new();
    let secret = fx.root.parent().unwrap().join("secret.md");
    std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
    fx.write("AGENTS.md", "@import ../secret.md\n");

    let text = load(&fx.scope("")).text;
    assert!(!text.contains("SSH KEY MATERIAL"), "{text}");
    assert!(text.contains("skipped"), "{text}");
}

#[test]
fn remember_seeds_an_import_of_an_existing_agents_md() {
    let fx = Fixture::new();
    fx.write("AGENTS.md", "existing agents rule");
    remember(&memory_path(&fx.root), "new note").unwrap();

    let written = std::fs::read_to_string(memory_path(&fx.root)).unwrap();
    assert!(written.contains("@import AGENTS.md"), "{written}");
    // ...and the loaded memory carries both the note and the imported rules,
    // so /remember never silently deactivates the AGENTS.md it eclipsed.
    let text = load(&fx.scope("")).text;
    assert!(text.contains("new note"));
    assert!(text.contains("existing agents rule"), "{text}");
}

#[test]
fn remember_does_not_seed_an_import_when_there_is_no_agents_md() {
    let fx = Fixture::new();
    remember(&memory_path(&fx.root), "just a note").unwrap();
    let written = std::fs::read_to_string(memory_path(&fx.root)).unwrap();
    assert!(!written.contains("@import AGENTS.md"), "{written}");
}
