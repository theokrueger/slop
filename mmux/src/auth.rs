use std::ffi::{OsStr, OsString};

use anyhow::anyhow;
use nonstick::{AuthnFlags, ConversationAdapter, ErrorCode, Transaction, TransactionBuilder};
use thiserror::Error;

/// Result of verifying a Linux username/password pair.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError>;
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("authentication backend error: {0}")]
    Backend(#[source] anyhow::Error),
}

/// Authenticates against Linux-PAM. Only the Unix account mmux runs as is
/// accepted: panes always run with the service's own privileges.
pub struct PamAuthenticator {
    service: String,
    user: String,
}

impl PamAuthenticator {
    pub fn new(service: impl Into<String>, user: impl Into<String>) -> anyhow::Result<Self> {
        let service = service.into();
        if service.trim().is_empty() {
            return Err(anyhow!("PAM service name must not be empty"));
        }
        Ok(Self {
            service,
            user: user.into(),
        })
    }
}

impl Authenticator for PamAuthenticator {
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError> {
        if username != self.user {
            // The server can only run panes as its own user, so there is no
            // point in asking PAM about any other account.
            return Err(AuthError::InvalidCredentials);
        }

        let conversation = PasswordConversation {
            username: self.user.clone(),
            password: password.to_owned(),
        };
        let mut transaction = TransactionBuilder::new_with_service(&self.service)
            .username(&self.user)
            .build(conversation.into_conversation())
            .map_err(|err| AuthError::Backend(anyhow!("could not start PAM transaction: {err}")))?;

        transaction
            .authenticate(AuthnFlags::empty())
            .map_err(classify_pam_error)?;
        transaction
            .account_management(AuthnFlags::empty())
            .map_err(classify_pam_error)?;
        Ok(())
    }
}

fn classify_pam_error(code: ErrorCode) -> AuthError {
    use ErrorCode::*;
    let invalid = matches!(
        code,
        AuthenticationError
            | UserUnknown
            | MaxTries
            | CredentialsInsufficient
            | AccountExpired
            | CredentialsExpired
            | PermissionDenied
    );
    if invalid {
        AuthError::InvalidCredentials
    } else {
        AuthError::Backend(anyhow!("PAM error: {code}"))
    }
}

/// Answers PAM prompts from the submitted credentials.
struct PasswordConversation {
    username: String,
    password: String,
}

impl ConversationAdapter for PasswordConversation {
    fn prompt(&self, _request: impl AsRef<OsStr>) -> nonstick::Result<OsString> {
        Ok(OsString::from(&self.username))
    }

    fn masked_prompt(&self, _request: impl AsRef<OsStr>) -> nonstick::Result<OsString> {
        Ok(OsString::from(&self.password))
    }

    fn error_msg(&self, message: impl AsRef<OsStr>) {
        tracing::debug!(message = %message.as_ref().to_string_lossy(), "PAM error message");
    }

    fn info_msg(&self, message: impl AsRef<OsStr>) {
        tracing::debug!(message = %message.as_ref().to_string_lossy(), "PAM info message");
    }
}

/// The Unix account this process runs as.
pub fn current_username() -> anyhow::Result<String> {
    let uid = nix::unistd::geteuid();
    let user = nix::unistd::User::from_uid(uid)
        .map_err(|err| anyhow!("could not look up uid {uid}: {err}"))?
        .ok_or_else(|| anyhow!("no passwd entry for uid {uid}"))?;
    Ok(user.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_other_accounts_without_calling_pam() {
        let auth = PamAuthenticator::new("login", "alice").unwrap();
        let err = auth.authenticate("bob", "hunter2").unwrap_err();
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[test]
    fn rejects_empty_service() {
        assert!(PamAuthenticator::new("  ", "alice").is_err());
    }

    #[test]
    fn current_user_has_a_name() {
        assert!(!current_username().unwrap().is_empty());
    }

    /// Manual end-to-end PAM check:
    /// `MMUX_TEST_PAM_USER=me MMUX_TEST_PAM_PASSWORD=... cargo test -- --ignored`
    #[test]
    #[ignore = "requires MMUX_TEST_PAM_USER and MMUX_TEST_PAM_PASSWORD"]
    fn pam_accepts_a_real_password() {
        let user = std::env::var("MMUX_TEST_PAM_USER").expect("MMUX_TEST_PAM_USER");
        let password = std::env::var("MMUX_TEST_PAM_PASSWORD").expect("MMUX_TEST_PAM_PASSWORD");
        let service = std::env::var("MMUX_TEST_PAM_SERVICE").unwrap_or_else(|_| "login".into());
        let auth = PamAuthenticator::new(service, &user).unwrap();
        auth.authenticate(&user, &password).expect("valid password");
        assert!(matches!(
            auth.authenticate(&user, "definitely-wrong").unwrap_err(),
            AuthError::InvalidCredentials
        ));
    }
}
