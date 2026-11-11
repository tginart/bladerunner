//! Compare marketplace source spellings.
//!
//! An install record keeps the source as the user typed it at install time.
//! Later the same source can come back with a `.git` suffix added or removed,
//! or with a different scheme or host case. Without folding those,
//! auto-update cannot find the installed plugin and installs a second copy.

/// Purpose: Tell if two marketplace sources name the same repository.
/// Input: Two source strings: git URLs or local paths.
/// Output: `true` when both name the same source.
/// Method:
///     1. Equal strings always match.
///     2. Git URLs match after [`normalize_git_url`].
///     3. Local paths match only when the strings are equal.
pub fn is_same_source_identity(a: &str, b: &str) -> bool {
    a == b || (canonical_remote(a).is_some() && normalize_git_url(a) == normalize_git_url(b))
}

/// Purpose: Give a git URL a canonical spelling for comparison.
/// Input: A git URL (scheme form or scp form) or a local path.
/// Output: The URL with one `.git` suffix removed and scheme and host in lower
/// case. A local path keeps its spelling, except for the `.git` suffix.
/// Note: The user info and the path part keep their case. Some hosts treat a
/// user or a path with a different case as a different account or repository.
pub fn normalize_git_url(url: &str) -> String {
    let url = url.strip_suffix(".git").unwrap_or(url);
    canonical_remote(url).unwrap_or_else(|| url.to_owned())
}

/// Lower-case the scheme and host of a remote URL, or `None` for a local path.
fn canonical_remote(url: &str) -> Option<String> {
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        return Some(format!(
            "{}://{}{path}",
            scheme.to_ascii_lowercase(),
            lowercase_host(authority)
        ));
    }
    // Git reads a `/` before the first `:` as a local path. Requiring `@`
    // also keeps a Windows drive path such as `C:/mkt` local.
    let (user_host, path) = url.split_once(':')?;
    (user_host.contains('@') && !user_host.contains('/'))
        .then(|| format!("{}:{path}", lowercase_host(user_host)))
}

/// Lower-case the host of `[userinfo@]host[:port]`. The user info keeps its
/// case because user names can be case sensitive.
fn lowercase_host(authority: &str) -> String {
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => format!("{userinfo}@{}", host.to_ascii_lowercase()),
        None => authority.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[rustfmt::skip]
    fn normalize_git_url_folds_scheme_host_and_one_git_suffix() {
        let rows: [(&str, &str, &str); 7] = [
            ("HTTPS://Git.Corp.com/Team/Tools.git", "https://git.corp.com/Team/Tools", "scheme and host fold"),
            ("https://h.example/repo.git.git", "https://h.example/repo.git", "only one .git strips"),
            ("git@GitHub.com:Org/Repo.git", "git@github.com:Org/Repo", "scp host folds, path does not"),
            ("Alice@GitHub.com:Org/Repo", "Alice@github.com:Org/Repo", "scp user keeps its case"),
            ("SSH://Alice:Tok@Host.Example/r", "ssh://Alice:Tok@host.example/r", "URL userinfo keeps its case"),
            ("/tmp/Marketplace", "/tmp/Marketplace", "local path keeps its spelling"),
            ("/tmp/A@b:c/mkt", "/tmp/A@b:c/mkt", "a `/` before `:` is a local path"),
        ];
        for (input, want, label) in rows {
            assert_eq!(want, normalize_git_url(input), "{label}");
        }
    }

    #[test]
    #[rustfmt::skip]
    fn source_identity_ignores_git_suffix_and_host_case_only() {
        let rows: [(&str, &str, bool, &str); 16] = [
            ("https://github.com/org/repo.git", "https://github.com/org/repo", true, ".git suffix"),
            ("HTTPS://GitHub.com/org/repo", "https://github.com/org/repo.git", true, "scheme and host case"),
            ("git@GitHub.com:org/repo.git", "git@github.com:org/repo", true, "scp host case and .git"),
            ("ssh://git@GitHub.com/org/repo.git", "ssh://git@github.com/org/repo", true, "ssh URL"),
            ("https://github.com/org/repo", "https://github.com/org/other", false, "different repo"),
            ("https://github.com/org/repo", "https://gitlab.com/org/repo", false, "different host"),
            ("https://github.com/Org/Repo", "https://github.com/org/repo", false, "path case is identity"),
            ("Alice@host:market.git", "alice@host:market.git", false, "scp user case is identity"),
            ("ssh://Alice@host/market", "ssh://alice@host/market", false, "URL user case is identity"),
            ("git@github.com:org/repo.git", "https://github.com/org/repo.git", false, "scp and https differ"),
            ("/tmp/mkt.git", "/tmp/mkt", false, "local paths with and without .git differ"),
            ("/tmp/Mkt", "/tmp/mkt", false, "local path case is kept"),
            ("/tmp/mkt", "/tmp/mkt", true, "same local path"),
            ("/tmp/github.com/org/repo", "https://github.com/org/repo", false, "path never aliases a URL"),
            ("github.com:org/repo.git", "github.com:org/repo", false, "user-less scp compares exactly"),
            ("C:/mkt.git", "C:/mkt", false, "Windows drive path is local"),
        ];
        for (a, b, want, label) in rows {
            assert_eq!(want, is_same_source_identity(a, b), "{label}: {a} vs {b}");
            assert_eq!(want, is_same_source_identity(b, a), "{label} (reversed): {b} vs {a}");
        }
    }
}
