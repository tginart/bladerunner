//! Fail-closed `git` arm of the routine-bash heuristic. A shape that this module
//! does not recognize goes to the classifier.
//!
//! Callers pass wrapper-peeled words with `words[0]` literally `git`. A
//! case-variant or path-qualified spelling may resolve to a different binary,
//! so it is not routine.
//!
//! Ported from xai-org/grok-build 2bdd1d6, with one change: `git stash pop`
//! drops the stash entry, so it goes to the classifier here.

use crate::permission::exec_risk::{
    git_words_are_read_only_query, git_words_have_unsafe_query_option,
};

/// Purpose: This function tells if a `git` command is routine local work that
/// auto mode can allow without the classifier.
/// Input: The wrapper-peeled command words.
/// Output: `true` for read-only queries and non-destructive workflow verbs.
/// Method:
///     1. Accept the read-only queries that `exec_risk` recognizes.
///     2. Refuse a command with an unsafe query option.
///     3. Accept `add`, `commit`, `pull`, `fetch`, and `worktree list`.
///     4. Accept `checkout` and `switch` only as a plain branch switch.
///     5. Accept `stash` only for subcommands that keep the stash entries.
///     6. Refuse all other shapes.
pub(super) fn git_words_are_routine(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("git") {
        return false;
    }
    if git_words_are_read_only_query(words) {
        return true;
    }
    if git_words_have_unsafe_query_option(words) {
        return false;
    }
    let Some(verb) = words.get(1).map(String::as_str) else {
        return false;
    };
    let args = words.get(2..).unwrap_or_default();
    match verb {
        "add" | "commit" | "pull" | "fetch" => true,
        "worktree" => args.first().map(String::as_str) == Some("list"),
        "checkout" | "switch" => is_routine_branch_switch(args),
        "stash" => is_routine_stash(args),
        _ => false,
    }
}

/// Exact spellings only, so clusters (`-qf`) and abbreviations (`--fo`) fail closed.
const BRANCH_SWITCH_BENIGN_FLAGS: &[&str] = &[
    "-q",
    "--quiet",
    "-d",
    "--detach",
    "-t",
    "--track",
    "--no-track",
    "--guess",
    "--no-guess",
    "--progress",
    "--no-progress",
    "--recurse-submodules",
    "--no-recurse-submodules",
];

/// An unlisted option (`--`, `-f`, `--discard-changes`, `-p`, ...) or a second
/// operand means path mode or a discard, so both fail closed.
fn is_routine_branch_switch(args: &[String]) -> bool {
    let mut operands = 0;
    let mut it = args.iter().map(String::as_str);
    while let Some(word) = it.next() {
        match word {
            "-b" | "-B" | "-c" | "-C" | "--orphan" => {
                it.next();
            }
            // Bare `-` is the previous branch.
            "-" => operands += 1,
            _ if BRANCH_SWITCH_BENIGN_FLAGS.contains(&word) => {}
            _ if word.starts_with('-') => return false,
            _ => {
                if operand_reads_as_path(word) {
                    return false;
                }
                operands += 1;
            }
        }
    }
    operands <= 1
}

/// `drop` and `clear` throw away a stash, and `pop` drops the entry after it
/// applies it. Only subcommands that keep every stash entry are listed.
fn is_routine_stash(args: &[String]) -> bool {
    let mut it = args.iter().map(String::as_str);
    let subcommand = loop {
        match it.next() {
            Some("-m" | "--message") => {
                it.next();
            }
            Some("-q" | "--quiet") => {}
            Some(word) if word.starts_with('-') => return false,
            other => break other,
        }
    };
    matches!(
        subcommand,
        None | Some("push" | "save" | "apply" | "list" | "show" | "branch")
    )
}

/// Purpose: This function tells if a `checkout`/`switch` operand looks like a
/// pathspec or a revision expression instead of a branch name.
/// Input: One operand word.
/// Output: `true` if the operand is not a plain branch name.
/// Note: A dotless bare name (`Makefile`, `src`) looks the same as a branch
/// without a repository lookup, so it stays routine.
fn operand_reads_as_path(op: &str) -> bool {
    // git-check-ref-format rejects these in a branch name.
    if op
        .split('/')
        .any(|c| c.starts_with('.') || c.ends_with(".lock"))
        || op.ends_with('/')
        || op.ends_with('.')
        || op.contains("..")
        || op.contains("@{")
        || op.contains(['~', '^', ':', '\\'])
        || op.contains(char::is_whitespace)
    {
        return true;
    }
    if op.starts_with('/') || op.contains(['*', '?', '[']) {
        return true;
    }
    let Some((stem, ext)) = op.rsplit_once('.') else {
        return false;
    };
    // `2.x` and `release/1.2.x` are maintenance branches, not a file extension.
    let stem_leaf = stem.rsplit('/').next().unwrap_or(stem);
    if ext == "x"
        && stem_leaf.bytes().any(|b| b.is_ascii_digit())
        && stem_leaf.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        return false;
    }
    (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
#[path = "routine_git_tests.rs"]
mod tests;
