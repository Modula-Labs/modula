//! Input normalisation shared by handlers.

use crate::error::{AppError, AppResult};

pub fn email(raw: &str) -> AppResult<String> {
    let e = raw.trim().to_lowercase();
    let valid = e.len() <= 254
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
        && matches!(e.split_once('@'), Some((local, domain))
            if !local.is_empty() && domain.contains('.') && !domain.contains('@')
               && !domain.starts_with('.') && !domain.ends_with('.'));
    if !valid {
        return Err(AppError::bad_request("invalid email address"));
    }
    Ok(e)
}

/// Human-facing names (users, workspaces, projects): 1–64 chars, no control chars.
pub fn display_name(raw: &str, what: &str) -> AppResult<String> {
    let n = raw.trim();
    let len = n.chars().count();
    if len == 0 || len > 64 || n.chars().any(char::is_control) {
        return Err(AppError::bad_request(format!(
            "{what} must be 1–64 characters"
        )));
    }
    Ok(n.to_owned())
}

pub fn password(raw: &str) -> AppResult<()> {
    modula_crypto::password::validate_policy(raw).map_err(AppError::bad_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert_eq!(email("  Ada@Example.COM ").unwrap(), "ada@example.com");
        for bad in [
            "",
            "ada",
            "ada@",
            "@x.com",
            "a b@x.com",
            "a@x",
            "a@@x.com",
            "a@.x.com",
            "a@x.com.",
        ] {
            assert!(email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(display_name("  Acme ", "name").unwrap(), "Acme");
        assert!(display_name("", "name").is_err());
        assert!(display_name("a\u{0}b", "name").is_err());
        assert!(display_name(&"x".repeat(65), "name").is_err());
    }
}
