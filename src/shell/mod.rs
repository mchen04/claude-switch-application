//! Shell wrapper injection for zsh and bash. Manages the `cs` block in rc files
//! and provides snippet generation for `cs setup` / `cs uninstall`.

use std::path::PathBuf;

use crate::cli::ShellChoice;
use crate::error::{Error, Result};

pub mod bash;
pub mod zsh;

pub const BEGIN_MARKER: &str = "# >>> cs (claude-switch) >>>";
pub const END_MARKER: &str = "# <<< cs (claude-switch) <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
}

impl Shell {
    pub fn detect(choice: ShellChoice) -> Result<Self> {
        match choice {
            ShellChoice::Zsh => Ok(Self::Zsh),
            ShellChoice::Bash => Ok(Self::Bash),
            ShellChoice::Auto => {
                let env_shell = std::env::var("SHELL").unwrap_or_default();
                if env_shell.ends_with("/zsh") || env_shell == "zsh" {
                    Ok(Self::Zsh)
                } else if env_shell.ends_with("/bash") || env_shell == "bash" {
                    Ok(Self::Bash)
                } else {
                    Err(Error::Config(format!(
                        "could not detect shell from $SHELL=`{env_shell}`; pass --shell zsh|bash"
                    )))
                }
            }
        }
    }

    pub fn rc_path(self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        match self {
            Shell::Zsh => Some(home.join(".zshrc")),
            Shell::Bash => Some(home.join(".bashrc")),
        }
    }

    pub fn snippet(self) -> &'static str {
        match self {
            Shell::Zsh => zsh::SNIPPET,
            Shell::Bash => bash::SNIPPET,
        }
    }
}

/// Replace any existing `begin ... end` block with `body`, or append it if absent.
/// Errors if the rc file contains a malformed (half-present or out-of-order) marker pair.
pub fn upsert_block(existing: &str, body: &str) -> Result<String> {
    upsert_block_named(existing, BEGIN_MARKER, END_MARKER, body)
}

/// Replace a well-formed `begin ... end` block with `body`; append a fresh block when
/// neither marker is present. If exactly one marker is present, or they appear out of
/// order, the existing block is malformed: refuse rather than append a second `begin`,
/// which a later run would treat as the block start and use to clobber user content
/// sitting between the orphaned markers.
pub fn upsert_block_named(existing: &str, begin: &str, end: &str, body: &str) -> Result<String> {
    match (existing.find(begin), existing.find(end)) {
        (Some(start), Some(stop)) if start < stop => {
            let end_line = existing[stop..]
                .find('\n')
                .map(|n| stop + n + 1)
                .unwrap_or(existing.len());
            let mut out = String::with_capacity(existing.len());
            out.push_str(&existing[..start]);
            push_block(&mut out, begin, end, body);
            out.push_str(&existing[end_line..]);
            Ok(out)
        }
        (None, None) => {
            let mut out = String::with_capacity(existing.len() + body.len() + 64);
            out.push_str(existing);
            if !existing.is_empty() && !existing.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            push_block(&mut out, begin, end, body);
            Ok(out)
        }
        _ => Err(Error::Config(format!(
            "the cs wrapper markers in the rc file are malformed (one marker missing or out \
             of order); fix or remove the `{begin}` / `{end}` block manually, then re-run"
        ))),
    }
}

fn push_block(out: &mut String, begin: &str, end: &str, body: &str) {
    out.push_str(begin);
    out.push('\n');
    out.push_str(body.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(end);
    out.push('\n');
}

/// Remove the `# >>> cs ... # <<< cs` block if present. Returns the new file contents.
#[allow(dead_code)] // used by `cs uninstall` (Phase D)
pub fn remove_block(existing: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        if start < end {
            let end_line = existing[end..]
                .find('\n')
                .map(|n| end + n + 1)
                .unwrap_or(existing.len());
            let mut out = String::with_capacity(existing.len());
            // Trim a trailing blank line we may have inserted before the block.
            let head = existing[..start].trim_end_matches('\n');
            out.push_str(head);
            if !head.is_empty() {
                out.push('\n');
            }
            out.push_str(&existing[end_line..]);
            return out;
        }
    }
    existing.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_when_missing() {
        let s = upsert_block("export FOO=1\n", "alias x=y").unwrap();
        assert!(s.contains(BEGIN_MARKER));
        assert!(s.contains("alias x=y"));
        assert!(s.contains(END_MARKER));
    }

    #[test]
    fn upsert_replaces_existing() {
        let initial = upsert_block("export A=1\n", "alias x=y").unwrap();
        let updated = upsert_block(&initial, "alias x=z").unwrap();
        assert!(updated.contains("alias x=z"));
        assert!(!updated.contains("alias x=y"));
        assert_eq!(updated.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn upsert_refuses_malformed_half_present_block() {
        // BEGIN marker present but END truncated away: refuse rather than append a second
        // block (which a later run would treat as the start and clobber user content).
        let corrupted = format!("user line\n{BEGIN_MARKER}\nclaude() {{ :; }}\n");
        assert!(upsert_block(&corrupted, "alias x=y").is_err());
    }

    #[test]
    fn remove_drops_block_idempotent() {
        let with = upsert_block("export A=1\n", "alias x=y").unwrap();
        let without = remove_block(&with);
        assert!(!without.contains(BEGIN_MARKER));
        assert_eq!(remove_block(&without), without);
    }
}
