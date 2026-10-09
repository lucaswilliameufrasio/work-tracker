use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddress {
    pub address: String,
    pub key: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmailAddressError {
    #[error("email address is empty")]
    Empty,
    #[error("email address must contain one non-empty local part and domain")]
    Malformed,
    #[error("email address contains whitespace or control characters")]
    WhitespaceOrControl,
}

pub fn normalize_email_address(value: &str) -> Result<EmailAddress, EmailAddressError> {
    let address = value.trim();
    if address.is_empty() {
        return Err(EmailAddressError::Empty);
    }
    if address.chars().any(char::is_whitespace) || address.chars().any(char::is_control) {
        return Err(EmailAddressError::WhitespaceOrControl);
    }
    let mut parts = address.split('@');
    let Some(local_part) = parts.next() else {
        return Err(EmailAddressError::Malformed);
    };
    let Some(domain) = parts.next() else {
        return Err(EmailAddressError::Malformed);
    };
    if local_part.is_empty() || domain.is_empty() || parts.next().is_some() {
        return Err(EmailAddressError::Malformed);
    }

    Ok(EmailAddress {
        address: address.to_owned(),
        key: address.to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_outer_whitespace_and_folds_ascii_case_without_aliasing_dots_or_plus_tags() {
        let normalized = normalize_email_address(" Alice.Example+tag@EXAMPLE.COM ").unwrap();
        assert_eq!(normalized.address, "Alice.Example+tag@EXAMPLE.COM");
        assert_eq!(normalized.key, "alice.example+tag@example.com");

        let dotted = normalize_email_address("aliceexample+tag@example.com").unwrap();
        let untagged = normalize_email_address("alice.example@example.com").unwrap();
        assert_ne!(normalized.key, dotted.key);
        assert_ne!(normalized.key, untagged.key);
    }

    #[test]
    fn rejects_empty_malformed_and_embedded_whitespace_addresses() {
        assert_eq!(
            normalize_email_address("  ").unwrap_err(),
            EmailAddressError::Empty
        );
        assert_eq!(
            normalize_email_address("local-only").unwrap_err(),
            EmailAddressError::Malformed
        );
        assert_eq!(
            normalize_email_address("a@@example.com").unwrap_err(),
            EmailAddressError::Malformed
        );
        assert_eq!(
            normalize_email_address("a b@example.com").unwrap_err(),
            EmailAddressError::WhitespaceOrControl
        );
    }
}
