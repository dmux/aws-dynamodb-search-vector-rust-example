//! Deriving the memory namespace from the IAM caller.
//!
//! The namespace is never read from the request body. It comes from the
//! principal API Gateway verified when it checked the SigV4 signature, which is
//! what makes it a real boundary rather than a value a client can choose.
//!
//! # Why the ARN needs normalising
//!
//! An IAM user ARN is stable:
//!
//! ```text
//! arn:aws:iam::123456789012:user/rafael
//! ```
//!
//! An assumed role ARN is not — the last segment is the **session name**, and
//! it changes on every login:
//!
//! ```text
//! arn:aws:sts::123456789012:assumed-role/Developer/rafael-2026-08-10-091500
//! arn:aws:sts::123456789012:assumed-role/Developer/rafael-2026-08-11-084200
//! ```
//!
//! Using the raw ARN as a partition key would therefore give every SSO login a
//! brand-new namespace, and yesterday's memories would simply stop existing
//! from the user's point of view. Collapsing the session segment is what keeps
//! one human mapped to one namespace.

use agent_memory_core::UserId;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrincipalError {
    #[error("the request carried no IAM caller identity")]
    Missing,

    #[error("could not parse the caller ARN: {0}")]
    Malformed(String),
}

/// Map a verified IAM caller ARN to a stable memory namespace.
///
/// ```text
/// arn:aws:iam::123:user/rafael                     -> aws:123:user/rafael
/// arn:aws:sts::123:assumed-role/Dev/session-abc    -> aws:123:assumed-role/Dev
/// arn:aws:iam::123:role/Service                    -> aws:123:role/Service
/// ```
pub fn principal_to_namespace(arn: &str) -> Result<UserId, PrincipalError> {
    let arn = arn.trim();
    if arn.is_empty() {
        return Err(PrincipalError::Missing);
    }

    // arn:partition:service:region:account-id:resource
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" {
        return Err(PrincipalError::Malformed(arn.to_string()));
    }

    // The partition is kept as the namespace prefix. Account IDs are only
    // guaranteed unique within a partition, and for the ordinary `aws`
    // partition this is exactly the "aws:" prefix the format already had.
    let partition = parts[1];
    let account_id = parts[4];
    let resource = parts[5];
    if partition.is_empty() || account_id.is_empty() || resource.is_empty() {
        return Err(PrincipalError::Malformed(arn.to_string()));
    }

    let mut segments = resource.split('/');
    let resource_type = segments.next().unwrap_or_default();
    let resource_name = segments.next().unwrap_or_default();
    if resource_type.is_empty() || resource_name.is_empty() {
        return Err(PrincipalError::Malformed(arn.to_string()));
    }

    // Everything after the role name is the session, and it is deliberately
    // discarded. For `user/` and `role/` there is nothing after the name, so
    // the same code path handles both.
    let normalised = format!("{partition}:{account_id}:{resource_type}/{resource_name}");

    UserId::new(normalised).map_err(|_| PrincipalError::Malformed(arn.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_iam_user_maps_to_a_stable_namespace() {
        let namespace =
            principal_to_namespace("arn:aws:iam::123456789012:user/rafael").expect("valid arn");
        assert_eq!(namespace.as_str(), "aws:123456789012:user/rafael");
    }

    #[test]
    fn two_sessions_of_the_same_role_share_one_namespace() {
        // This is the whole reason the function exists. Without collapsing the
        // session segment, every SSO login would look like a different user and
        // the previous day's memories would vanish.
        let monday = principal_to_namespace(
            "arn:aws:sts::123456789012:assumed-role/Developer/rafael-2026-08-10-091500",
        )
        .expect("valid arn");
        let tuesday = principal_to_namespace(
            "arn:aws:sts::123456789012:assumed-role/Developer/rafael-2026-08-11-084200",
        )
        .expect("valid arn");

        assert_eq!(monday, tuesday);
        assert_eq!(monday.as_str(), "aws:123456789012:assumed-role/Developer");
    }

    #[test]
    fn different_roles_stay_in_different_namespaces() {
        let developer =
            principal_to_namespace("arn:aws:sts::123456789012:assumed-role/Developer/session")
                .expect("valid arn");
        let admin = principal_to_namespace("arn:aws:sts::123456789012:assumed-role/Admin/session")
            .expect("valid arn");
        assert_ne!(developer, admin);
    }

    #[test]
    fn different_accounts_stay_in_different_namespaces() {
        let first =
            principal_to_namespace("arn:aws:iam::111111111111:user/rafael").expect("valid arn");
        let second =
            principal_to_namespace("arn:aws:iam::222222222222:user/rafael").expect("valid arn");
        assert_ne!(
            first, second,
            "the same user name in two accounts is two different people"
        );
    }

    #[test]
    fn a_session_name_containing_slashes_is_still_collapsed() {
        let namespace =
            principal_to_namespace("arn:aws:sts::123456789012:assumed-role/Developer/a/b/c")
                .expect("valid arn");
        assert_eq!(
            namespace.as_str(),
            "aws:123456789012:assumed-role/Developer"
        );
    }

    #[test]
    fn a_role_arn_without_a_session_is_accepted() {
        let namespace = principal_to_namespace("arn:aws:iam::123456789012:role/ServiceRole")
            .expect("valid arn");
        assert_eq!(namespace.as_str(), "aws:123456789012:role/ServiceRole");
    }

    #[test]
    fn the_partition_is_part_of_the_namespace() {
        // Account IDs are only unique within a partition, so an account in
        // aws-cn must not collide with the same number in aws.
        let commercial =
            principal_to_namespace("arn:aws:iam::123456789012:user/rafael").expect("valid arn");
        let china =
            principal_to_namespace("arn:aws-cn:iam::123456789012:user/rafael").expect("valid arn");

        assert_eq!(commercial.as_str(), "aws:123456789012:user/rafael");
        assert_eq!(china.as_str(), "aws-cn:123456789012:user/rafael");
        assert_ne!(commercial, china);
    }

    #[test]
    fn blank_input_is_reported_as_missing_rather_than_malformed() {
        assert_eq!(principal_to_namespace("   "), Err(PrincipalError::Missing));
    }

    #[test]
    fn malformed_arns_are_rejected_instead_of_producing_a_junk_namespace() {
        for bad in [
            "not-an-arn",
            "arn:aws:iam::123456789012",
            "arn:aws:iam::123456789012:",
            "arn:aws:iam:::user/rafael",
            "arn:aws:iam::123456789012:user",
            "arn:aws:iam::123456789012:user/",
        ] {
            assert!(
                matches!(
                    principal_to_namespace(bad),
                    Err(PrincipalError::Malformed(_))
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
