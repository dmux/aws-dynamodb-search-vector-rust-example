//! Best-effort GitHub attribution label.
//!
//! # What this is, and what it deliberately is not
//!
//! This reads the local `gh` login purely so stored memories carry a
//! human-readable name. It has **no part in authentication or authorization**.
//!
//! The obvious alternative — send the local `gh` token to the API and have the
//! server verify it — was rejected on purpose. A token from `gh auth login`
//! typically carries `repo` and `workflow` scopes, so forwarding it would hand
//! the API (and its CloudWatch logs, and anyone who compromised the account)
//! read/write access to every one of the user's repositories, in exchange for
//! answering "who are you". SigV4 already answers that question with
//! credentials the machine has anyway, and at no extra cost.
//!
//! Note this calls `gh api user` rather than `gh auth token`: the token itself
//! is never read, let alone transmitted.

use std::process::Stdio;

use tokio::process::Command;

/// The `login` of the locally authenticated GitHub user, if there is one.
///
/// Every failure path returns `None`. A machine without `gh`, or with `gh`
/// logged out, must still get a fully working memory server.
pub async fn detect_login() -> Option<String> {
    let output = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        tracing::debug!("gh is unavailable or logged out; continuing without a GitHub label");
        return None;
    }

    let login = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if login.is_empty() { None } else { Some(login) }
}

/// The name of the locally active GitHub repository, if there is one.
pub async fn detect_repo() -> Option<String> {
    let output = Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let repo = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if repo.is_empty() { None } else { Some(repo) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detection_never_fails_the_caller() {
        // Whether or not `gh` exists on this machine, this must return rather
        // than error: the label is a nicety, not a requirement.
        let _: Option<String> = detect_login().await;
    }
}
