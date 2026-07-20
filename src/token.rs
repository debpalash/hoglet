//! Project API token validation.
//!
//! Owns the token invariant for the whole binary (compat-spec.md "Token
//! resolution"). Every endpoint that touches a token goes through
//! [`validate`]; no handler re-implements these rules.

/// Tokens longer than this are rejected outright.
pub const MAX_TOKEN_BYTES: usize = 64;

/// Personal API keys start with this prefix and must never be accepted as
/// project tokens.
const PERSONAL_KEY_PREFIX: &str = "phx_";

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    Empty,
    TooLong,
    NotAscii,
    NullByte,
    PersonalApiKey,
}

/// Validate a project token per the PostHog wire contract.
///
/// Rejects tokens that are empty, longer than [`MAX_TOKEN_BYTES`], non-ASCII,
/// contain a null byte, or start with `phx_` (a personal API key, which must
/// never appear where a project token belongs).
pub fn validate(token: &str) -> Result<(), TokenError> {
    if token.is_empty() {
        return Err(TokenError::Empty);
    }
    if token.len() > MAX_TOKEN_BYTES {
        return Err(TokenError::TooLong);
    }
    if !token.is_ascii() {
        return Err(TokenError::NotAscii);
    }
    if token.contains('\0') {
        return Err(TokenError::NullByte);
    }
    if token.starts_with(PERSONAL_KEY_PREFIX) {
        return Err(TokenError::PersonalApiKey);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_tokens() {
        assert_eq!(validate("phc_abc123"), Ok(()));
        assert_eq!(validate("my-project-token"), Ok(()));
        assert_eq!(validate(&"a".repeat(MAX_TOKEN_BYTES)), Ok(()));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate(""), Err(TokenError::Empty));
    }

    #[test]
    fn rejects_too_long() {
        assert_eq!(
            validate(&"a".repeat(MAX_TOKEN_BYTES + 1)),
            Err(TokenError::TooLong)
        );
    }

    #[test]
    fn rejects_non_ascii() {
        assert_eq!(validate("tokén"), Err(TokenError::NotAscii));
    }

    #[test]
    fn rejects_null_byte() {
        assert_eq!(validate("tok\0en"), Err(TokenError::NullByte));
    }

    #[test]
    fn rejects_personal_api_key() {
        assert_eq!(validate("phx_secret"), Err(TokenError::PersonalApiKey));
    }
}
