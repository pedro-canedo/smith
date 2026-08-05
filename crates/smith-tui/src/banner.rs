const RAW: &str = include_str!("../../../ASCII - smith.md");

/// The project's ASCII banner, with the surrounding markdown code fence stripped.
pub fn banner() -> String {
    RAW.lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}
