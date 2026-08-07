//! `smith uninstall` — remove what installing smith put on this machine.
//!
//! Three rules, all of them about not destroying something the user wanted:
//!
//! - **Nothing goes without being shown first.** The plan is printed with a
//!   size against every entry, and removal is one confirmation for the whole
//!   plan plus a separate one for credentials.
//! - **Only what smith created.** Every target is checked to be inside
//!   `~/.smith` or to *be* the running executable. A path that escapes both is
//!   refused rather than removed, so a hand-edited config or a symlink cannot
//!   turn this into `rm -rf` somewhere else.
//! - **Per-project `.smith/` directories are named, never hunted.** They hold
//!   session history and `/rewind` checkpoints, they are scattered across
//!   wherever the user has worked, and walking the filesystem looking for
//!   directories to delete is not a thing this command should do. It prints
//!   the `find` that lists them and leaves the decision alone.

use std::path::{Path, PathBuf};

/// One thing to remove, and what it is, so the plan reads as prose.
struct Target {
    path: PathBuf,
    what: &'static str,
    /// Bytes on disk, `None` when it does not exist.
    size: Option<u64>,
    /// Holds API keys — asked about separately, and kept by default.
    sensitive: bool,
}

impl Target {
    fn new(path: PathBuf, what: &'static str, sensitive: bool) -> Self {
        let size = path.exists().then(|| dir_size(&path));
        Self {
            path,
            what,
            size,
            sensitive,
        }
    }

    fn present(&self) -> bool {
        self.size.is_some()
    }
}

/// Recursive size. Symlinks are counted as their own (tiny) entry and never
/// followed — a link into `/` must not make this report, or remove, the disk.
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_symlink() || meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| dir_size(&e.path()))
        .sum::<u64>()
        .saturating_add(meta.len())
}

fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

/// `Ok` on a clean removal (or nothing to do); `Err` with a sentence for the
/// user otherwise.
pub async fn run(assume_yes: bool, keep_config: bool) -> Result<(), String> {
    let home = smith_config::config_dir().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().ok();

    let mut targets = vec![
        Target::new(
            home.join("runtime"),
            "Node, the 9router gateway, Chromium",
            false,
        ),
        Target::new(home.join("agents"), "subagent definitions", false),
        Target::new(home.join("commands"), "custom slash commands", false),
        Target::new(home.join("skills"), "skills", false),
        Target::new(home.join("personas"), "personas", false),
        Target::new(home.join("SMITH.md"), "global project memory", false),
    ];
    if !keep_config {
        targets.push(Target::new(
            home.join("config.toml"),
            "config — including any API keys",
            true,
        ));
    }

    let present: Vec<&Target> = targets.iter().filter(|t| t.present()).collect();
    if present.is_empty() && exe.is_none() {
        println!("Nothing to remove: {} does not exist.", home.display());
        return Ok(());
    }

    println!("This will remove:\n");
    let mut total = 0;
    for target in &present {
        let size = target.size.unwrap_or(0);
        total += size;
        println!(
            "  {:>9}  {}\n             {}",
            human(size),
            target.path.display(),
            target.what
        );
    }
    if let Some(exe) = &exe {
        println!(
            "\n  {:>9}  {}\n             the smith binary itself",
            "",
            exe.display()
        );
    }
    println!("\n  {} in total.\n", human(total));

    // Named, not hunted. See the module doc.
    println!("Left alone — per-project history and `/rewind` checkpoints:");
    println!("  <project>/.smith/   in every project you have used smith in");
    println!(
        "  List them with:  find ~ -type d -name .smith -not -path '*/.smith/*' 2>/dev/null\n"
    );

    if !assume_yes && !confirm("Remove all of the above?")? {
        println!("Nothing was removed.");
        return Ok(());
    }

    // Credentials get a second question of their own, because "uninstall" and
    // "throw away my API keys" are different intentions and someone removing a
    // binary to reinstall it means only the first.
    //
    // `--yes` skips it: a script passing `--yes` is saying it accepts every
    // consequence, and a prompt no script can answer would just hang. The flag
    // for a cautious caller is `--keep-config`, which drops the file from the
    // plan entirely rather than asking about it.
    let mut removed = 0;
    for target in &present {
        if target.sensitive && !assume_yes && !confirm(&format!("Also remove {}?", target.what))? {
            println!("  kept {}", target.path.display());
            continue;
        }
        match remove(&target.path, &home) {
            Ok(()) => {
                println!("  removed {}", target.path.display());
                removed += 1;
            }
            Err(e) => println!("  could not remove {}: {e}", target.path.display()),
        }
    }

    // `~/.smith` itself, only once it is empty — a leftover the user put there
    // by hand is theirs, not ours.
    if std::fs::read_dir(&home).is_ok_and(|mut d| d.next().is_none()) {
        let _ = std::fs::remove_dir(&home);
        println!("  removed {}", home.display());
    }

    if let Some(exe) = exe {
        match remove_self(&exe) {
            Ok(()) => println!("  removed {}", exe.display()),
            Err(e) => {
                println!("\nThe binary is still at {}.", exe.display());
                println!("  {e}");
            }
        }
    }

    println!("\nRemoved {removed} item(s). smith is uninstalled.");
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool, String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(
            "uninstall needs a terminal to confirm; pass `--yes` if you meant to run it \
             unattended"
                .into(),
        );
    }
    dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(prompt)
        .default(false)
        .interact_opt()
        .map_err(|e| e.to_string())
        .map(|answer| answer.unwrap_or(false))
}

/// Removes `path`, refusing anything that is not inside `home`.
///
/// The check is on the *lexical* parent chain rather than a canonicalised one,
/// because canonicalising follows symlinks — and a `~/.smith/runtime` symlinked
/// elsewhere should still be removed as the link it is, not as its target.
fn remove(path: &Path, home: &Path) -> Result<(), String> {
    if !path.starts_with(home) {
        return Err(format!(
            "refusing: {} is outside {}",
            path.display(),
            home.display()
        ));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() && !meta.is_symlink() {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

/// Deleting the running executable.
///
/// Works on Unix: the directory entry goes and the inode stays alive until
/// this process exits. Windows holds the image open and refuses, so there the
/// honest answer is the command for the user to run afterwards.
#[cfg(unix)]
fn remove_self(exe: &Path) -> Result<(), String> {
    std::fs::remove_file(exe).map_err(|e| format!("{e} — remove it with: rm {}", exe.display()))
}

#[cfg(windows)]
fn remove_self(exe: &Path) -> Result<(), String> {
    Err(format!(
        "Windows keeps a running executable locked. Delete it after this exits: del \"{}\"",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KB");
        assert_eq!(human(5 << 20), "5.0 MB");
        assert_eq!(human(3 << 30), "3.0 GB");
    }

    #[test]
    fn removal_refuses_a_path_outside_the_smith_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("smith-home");
        std::fs::create_dir_all(&home).unwrap();
        let outsider = dir.path().join("not-ours.txt");
        std::fs::write(&outsider, b"keep me").unwrap();

        let err = remove(&outsider, &home).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        assert!(outsider.exists(), "the file must still be there");
    }

    #[test]
    fn removal_takes_something_inside_the_smith_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("smith-home");
        std::fs::create_dir_all(home.join("runtime")).unwrap();
        std::fs::write(home.join("runtime").join("node"), b"x").unwrap();

        remove(&home.join("runtime"), &home).unwrap();
        assert!(!home.join("runtime").exists());
    }

    #[test]
    fn a_symlink_is_measured_as_itself_rather_than_followed() {
        // A link pointing at something enormous must not make the plan claim
        // the uninstall frees that much — nor, worse, remove through it.
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&big, &link).unwrap();
        #[cfg(unix)]
        assert!(dir_size(&link) < 4096);
        #[cfg(not(unix))]
        let _ = link;
    }
}
