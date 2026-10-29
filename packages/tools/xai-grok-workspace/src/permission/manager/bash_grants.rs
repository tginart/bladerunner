//! Translation between what a permission prompt showed the user and what
//! becomes a persisted bash grant key. The "Always allow" label and glob come
//! back from the client (TUI, desktop, or a remote hub reply), so they are
//! verified against the prompted script before anything is persisted.

use crate::permission::bash_command_splitting::{
    is_setup_command, primary_command_from_script, try_parse_shell,
    try_parse_word_only_commands_sequence, unwrap_wrappers,
};
use crate::permission::policy::{bash_glob_is_catchall, bash_pattern_matches_command};
use crate::permission::state::PermissionState;

/// Whether `join(" ")` round-trips these words to the same single command.
/// A word with whitespace collapses to the join of different adjacent words
/// (`-m "fix stuff"` vs `-m fix stuff`), and a word with shell metacharacters
/// respells a different script (`-m "fix;git"` joins into a two-segment
/// chain), so a join serves as a grant key only if re-parsing it yields one
/// whole-script command with this exact argv.
fn words_join_unambiguously(words: &[String]) -> bool {
    let joined = words.join(" ");
    try_parse_shell(&joined)
        .and_then(|tree| try_parse_word_only_commands_sequence(&tree, &joined))
        .is_some_and(|segments| {
            matches!(segments.as_slice(),
                [only] if only.spans_whole_script(&joined) && only.words() == words)
        })
}

/// Persist an "Always allow: <words>" selection, verified against the row it
/// was built from: the label must re-derive as an argv-unambiguous prefix of
/// the primary command's words, at or above `minimum_always_allow_scope`, so a
/// forged/stale label (e.g. a remote hub reply) naming another segment — or
/// narrowing a pinned command to a bare `python3` / `sudo` — persists nothing.
/// The raw script is stored only when the label is the whole command modulo
/// quoting (empty prefix/suffix), so an unseen assignment or chained sibling
/// can't ride along on the raw key.
pub(super) fn persist_bash_always_allow(state: &mut PermissionState, cmd: &str, prefix: &str) {
    let Some(h) = primary_command_from_script(cmd) else {
        tracing::warn!(
            %prefix,
            "always-allow selected for a script with no primary command; no grant persisted"
        );
        return;
    };
    let words = &h.highlighted_words;
    let min_scope = super::minimum_always_allow_scope(words);
    let labeled_words_verified = (min_scope..=words.len())
        .any(|n| words_join_unambiguously(&words[..n]) && words[..n].join(" ") == prefix);
    if labeled_words_verified {
        state.allowed_bash_commands.insert(prefix.to_owned());
    } else if h.prefix.is_empty() && h.suffix.is_empty() && words.join(" ") == prefix {
        state.allowed_bash_commands.insert(cmd.to_owned());
    } else {
        tracing::warn!(
            %prefix,
            "always-allow label did not verify against the primary command; no grant persisted"
        );
    }
}

/// Whether a client-supplied glob grant is scoped to `cmd`: it must match at
/// least one non-setup segment under the same matcher enforcement uses (the
/// raw text when the script is unparseable), and it must not be universally
/// broad. A pattern like `*` / `**` / `* *` "matches the prompted script" only
/// by matching everything, so a forged reply could mint a blanket grant;
/// [`bash_glob_is_catchall`] refuses those. Scoped patterns the pattern editor
/// produces (`gh api repos/owner/*`) still pass.
pub(super) fn bash_glob_covers_script(cmd: &str, pattern: &str) -> bool {
    if bash_glob_is_catchall(pattern) {
        return false;
    }
    let segments =
        try_parse_shell(cmd).and_then(|tree| try_parse_word_only_commands_sequence(&tree, cmd));
    let Some(segments) = segments else {
        return bash_pattern_matches_command(pattern, cmd);
    };
    segments.iter().any(|seg| {
        let words = unwrap_wrappers(seg.words());
        !words.is_empty()
            && !is_setup_command(words)
            && bash_pattern_matches_command(pattern, &words.join(" "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_allow_persists_exactly_the_labeled_selection() {
        // Ordinary command: any argv-unambiguous prefix the row offers persists.
        let mut plain = PermissionState::default();
        persist_bash_always_allow(&mut plain, "cargo test --lib", "cargo test");
        assert!(plain.allowed_bash_commands.contains("cargo test"));

        // Plain full-scope dangerous command: label == raw, exact grant works.
        let mut dangerous = PermissionState::default();
        persist_bash_always_allow(
            &mut dangerous,
            "git push origin main",
            "git push origin main",
        );
        assert!(
            dangerous
                .allowed_bash_commands
                .contains("git push origin main")
        );

        // Chained script: only a primary-command prefix persists; a label
        // naming another segment (forgeable via remote replies) is refused.
        let mut chained = PermissionState::default();
        const CHAIN: &str = "cargo build && git push --force";
        persist_bash_always_allow(&mut chained, CHAIN, "cargo build");
        assert!(chained.allowed_bash_commands.contains("cargo build"));
        persist_bash_always_allow(&mut chained, CHAIN, "git push --force");
        assert!(!chained.allowed_bash_commands.contains("git push --force"));
        assert!(!chained.allowed_bash_commands.contains(CHAIN));

        // Quoted single command: the full-scope label is argv-ambiguous, so
        // the raw text is stored instead — it matches only its own spelling.
        let mut quoted = PermissionState::default();
        const QUOTED: &str = r#"git commit -m "fix stuff""#;
        persist_bash_always_allow(&mut quoted, QUOTED, "git commit -m fix stuff");
        assert!(quoted.allowed_bash_commands.contains(QUOTED));
        assert!(
            !quoted
                .allowed_bash_commands
                .contains("git commit -m fix stuff")
        );

        // A label the primary command cannot derive persists nothing.
        let mut forged = PermissionState::default();
        persist_bash_always_allow(&mut forged, "ls -la", "rm -rf /");
        assert!(forged.allowed_bash_commands.is_empty());

        // A narrowed pinned label persists nothing: dangerous verbs, exec
        // vehicles, and `sed` only save the exact full command. The bare-`git`
        // shape matters — `git` alone is not a dangerous verb, so a per-prefix
        // dangerous check would let it through and cover `git reset --hard`.
        for (cmd, label) in [
            ("git push origin main", "git push"),
            ("git push origin main", "git"),
            ("python3 -u foo.py arg", "python3"),
            ("python3 -u foo.py arg", "python3 -u"),
            ("sudo git status", "sudo git"),
            ("sed -n 1,5p a.rs", "sed -n"),
        ] {
            let mut narrowed = PermissionState::default();
            persist_bash_always_allow(&mut narrowed, cmd, label);
            assert!(
                narrowed.allowed_bash_commands.is_empty(),
                "narrowed pinned label {label:?} for {cmd:?} must persist nothing"
            );
        }
    }

    #[test]
    fn metacharacter_words_never_mint_chain_colliding_grants() {
        // The dequoted join respells a dangerous two-segment chain, so only
        // the raw text may persist.
        const CMD: &str = r#"git commit -m "fix;git" push --force"#;
        const COLLIDING_CHAIN: &str = "git commit -m fix;git push --force";

        let mut state = PermissionState::default();
        persist_bash_always_allow(&mut state, CMD, COLLIDING_CHAIN);
        assert!(state.allowed_bash_commands.contains(CMD));
        assert!(!state.allowed_bash_commands.contains(COLLIDING_CHAIN));
    }

    #[test]
    fn glob_grants_must_cover_the_prompted_script() {
        // The editor's legitimate output: a glob widening the primary's args.
        assert!(bash_glob_covers_script(
            "gh api repos/owner/xai/pulls",
            "gh api repos/owner/*"
        ));
        // Chains verify against any non-setup segment.
        assert!(bash_glob_covers_script(
            "cd /tmp && gh api repos/owner/xai",
            "gh api *"
        ));
        // A forged scope naming commands the prompt never showed is refused.
        assert!(!bash_glob_covers_script("ls -la", "gh api *"));
        assert!(!bash_glob_covers_script("ls -la", "*push*"));
        // Catch-all patterns "match the prompted script" only by matching
        // everything, so a forged reply cannot mint a blanket grant.
        for catchall in ["*", "**", "* *", "?*"] {
            assert!(
                !bash_glob_covers_script("gh api repos/owner/xai", catchall),
                "catch-all {catchall:?} must not cover the script",
            );
        }
        // Unparseable scripts verify against the raw text.
        assert!(bash_glob_covers_script("deploy $(rev)", "deploy *"));
        assert!(!bash_glob_covers_script("deploy $(rev)", "rm *"));
        assert!(!bash_glob_covers_script("deploy $(rev)", "*"));
    }
}
