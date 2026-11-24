//! Single-use second-factor recovery codes (`docs/adr/notifications.md`
//! decision 8).
//!
//! A patron gets [`RECOVERY_CODE_COUNT`] codes when they set up their first
//! second factor, and can make a new set at any time. A code finishes a
//! password reset in place of the authenticator code or passkey the
//! account would otherwise ask for.
//!
//! A code is 16 characters of Crockford base32, shown in four groups of
//! four (`7K3M-Q9TB-2XWD-HF6P`). The first group is the code's
//! *selector*: it is stored as typed and names which of the user's codes
//! to check, so a check runs Argon2id once rather than once per code. The
//! other twelve characters (60 bits) are the secret, stored only as an
//! Argon2id hash.

use rand::RngCore;

use crate::error::{LibraryError, Result};

/// How many codes a set holds.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Characters in a code's selector.
pub const SELECTOR_CHARS: usize = 4;

/// Characters in a code's secret.
pub const SECRET_CHARS: usize = 12;

/// Crockford's base32 alphabet: digits and capitals without I, L, O and U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Longest text accepted as a code before it is normalized: 16 symbols
/// plus room for separators and spaces a patron might type or paste.
pub const MAX_RECOVERY_CODE_INPUT: usize = 40;

/// One recovery code, as typed or as just made. Its `Debug` never shows
/// the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryCode {
    /// The selector: [`SELECTOR_CHARS`] alphabet characters.
    selector: String,
    /// The secret: [`SECRET_CHARS`] alphabet characters.
    secret: zeroize::Zeroizing<String>,
}

impl std::fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RecoveryCode({}-<redacted>)", self.selector)
    }
}

impl RecoveryCode {
    /// Makes a fresh code from the operating system's random source.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; SELECTOR_CHARS + SECRET_CHARS];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        // 256 is a multiple of 32, so masking keeps every symbol equally
        // likely.
        let symbols: String = bytes
            .iter()
            .map(|b| char::from(ALPHABET[usize::from(b & 31)]))
            .collect();
        let (selector, secret) = symbols.split_at(SELECTOR_CHARS);
        Self {
            selector: selector.to_string(),
            secret: zeroize::Zeroizing::new(secret.to_string()),
        }
    }

    /// Makes a full set of [`RECOVERY_CODE_COUNT`] codes whose selectors
    /// all differ.
    #[must_use]
    pub fn generate_set() -> Vec<Self> {
        let mut codes: Vec<Self> = Vec::with_capacity(RECOVERY_CODE_COUNT);
        while codes.len() < RECOVERY_CODE_COUNT {
            let code = Self::generate();
            if codes.iter().all(|c| c.selector != code.selector) {
                codes.push(code);
            }
        }
        codes
    }

    /// Reads a code as a patron typed it: case does not matter, spaces
    /// and hyphens are ignored, and the look-alikes Crockford allows are
    /// read as digits (`I` and `L` as `1`, `O` as `0`).
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] when the text is longer than
    /// [`MAX_RECOVERY_CODE_INPUT`] bytes or does not come to exactly 16
    /// alphabet characters.
    pub fn parse(input: &str) -> Result<Self> {
        let invalid =
            || LibraryError::Invalid("recovery_code must be 16 letters and digits".into());
        if input.len() > MAX_RECOVERY_CODE_INPUT {
            return Err(invalid());
        }
        let mut symbols = zeroize::Zeroizing::new(String::with_capacity(16));
        for c in input.chars() {
            let c = match c.to_ascii_uppercase() {
                ' ' | '-' => continue,
                'I' | 'L' => '1',
                'O' => '0',
                other => other,
            };
            if !u8::try_from(c).is_ok_and(|b| ALPHABET.contains(&b)) {
                return Err(invalid());
            }
            symbols.push(c);
        }
        if symbols.len() != SELECTOR_CHARS + SECRET_CHARS {
            return Err(invalid());
        }
        let (selector, secret) = symbols.split_at(SELECTOR_CHARS);
        Ok(Self {
            selector: selector.to_string(),
            secret: zeroize::Zeroizing::new(secret.to_string()),
        })
    }

    /// The selector, which names the stored code to check. It is not
    /// secret on its own.
    #[must_use]
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// The code as shown to the patron: four groups of four, hyphenated.
    #[must_use]
    pub fn display(&self) -> zeroize::Zeroizing<String> {
        let secret = self.secret.as_str();
        zeroize::Zeroizing::new(format!(
            "{}-{}-{}-{}",
            self.selector,
            &secret[..4],
            &secret[4..8],
            &secret[8..]
        ))
    }

    /// Hashes the secret with Argon2id, for storage.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Other`] when hashing fails.
    pub fn hash(&self) -> Result<String> {
        crate::password::hash_password(&self.secret)
    }

    /// Whether the secret matches a stored hash from [`Self::hash`].
    ///
    /// # Errors
    ///
    /// [`LibraryError::Other`] when the stored hash cannot be read.
    pub fn verify(&self, stored_hash: &str) -> Result<bool> {
        crate::password::verify_password(&self.secret, stored_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_reads_back_from_its_display_form() {
        let code = RecoveryCode::generate();
        let shown = code.display();
        assert_eq!(shown.len(), 19);
        assert_eq!(RecoveryCode::parse(&shown).unwrap(), code);
        assert_eq!(
            RecoveryCode::parse(&shown.to_lowercase().replace('-', " ")).unwrap(),
            code
        );
    }

    #[test]
    fn a_set_has_ten_codes_with_distinct_selectors() {
        let set = RecoveryCode::generate_set();
        assert_eq!(set.len(), RECOVERY_CODE_COUNT);
        let mut selectors: Vec<&str> = set.iter().map(RecoveryCode::selector).collect();
        selectors.sort_unstable();
        selectors.dedup();
        assert_eq!(selectors.len(), RECOVERY_CODE_COUNT);
    }

    #[test]
    fn look_alike_letters_read_as_digits() {
        let code = RecoveryCode::parse("ilo0-1111-0000-ABCD").unwrap();
        assert_eq!(code.selector(), "1100");
        assert_eq!(code.display().as_str(), "1100-1111-0000-ABCD");
    }

    #[test]
    fn malformed_codes_are_refused() {
        for input in [
            "",
            "ABCD-EFGH-JKMN",
            "ABCD-EFGH-JKMN-PQRST",
            "ABCD-EFGH-JKMN-PQRU",
            "ABCD_EFGH_JKMN_PQRS",
            "ÄBCD-EFGH-JKMN-PQRS",
            &"A".repeat(MAX_RECOVERY_CODE_INPUT + 1),
        ] {
            assert!(RecoveryCode::parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn the_secret_verifies_against_its_hash_and_debug_hides_it() {
        let code = RecoveryCode::generate();
        let hash = code.hash().unwrap();
        assert!(code.verify(&hash).unwrap());
        assert!(!RecoveryCode::generate().verify(&hash).unwrap());
        let debug = format!("{code:?}");
        assert!(!debug.contains(&code.display()[5..]));
        assert!(debug.contains(code.selector()));
    }
}
