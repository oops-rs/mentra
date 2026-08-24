//! A credential wrapper shared by the HTTP MCP transports.
//!
//! Both HTTP transports authenticate with operator-configured headers, so the
//! redaction rule lives here rather than in either transport's configuration
//! module: a type that cannot print itself is inherited by every struct that
//! derives `Debug`, with no rule for a contributor to remember.

#[cfg(test)]
mod tests;

use serde::Deserialize;

/// A header value that is never rendered by `Debug` or `Display`.
///
/// Redaction is a property of this type rather than of each container, so every
/// struct that derives `Debug` inherits it without a rule for contributors to
/// remember.
///
/// This type deliberately does **not** implement [`serde::Serialize`]. Adding
/// `#[derive(Serialize)]` to any struct holding one is therefore a compile
/// error rather than a silent credential leak into a config dump, a state
/// snapshot, or a session-persistence layer.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a value that must not be logged.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the wrapped value.
    ///
    /// This is the single grep-able point at which a secret becomes visible.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([redacted])")
    }
}

impl<T: Into<String>> From<T> for SecretString {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}
