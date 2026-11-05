//! The PIN that locks the app window. Kept as a salted, slow hash: the PIN
//! itself is never stored. This guards the window against someone at the
//! computer, not against someone who can edit the settings file.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hash rounds: a few hundredths of a second per check, so guessing a
/// 4-digit PIN from a copied settings file still takes a while.
const ROUNDS: u32 = 100_000;

pub const MIN_LEN: usize = 4;
pub const MAX_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinLock {
    /// Hex.
    pub salt: String,
    /// Hex.
    pub hash: String,
    /// Digits: the lock screen shows as many dots and checks on the last one.
    pub len: usize,
}

impl PinLock {
    pub fn new(pin: &str) -> Self {
        let salt: [u8; 16] = std::array::from_fn(|_| fastrand::u8(..));
        let salt = hex(&salt);
        PinLock { hash: hash(&salt, pin), salt, len: pin.chars().count() }
    }

    pub fn matches(&self, pin: &str) -> bool {
        hash(&self.salt, pin) == self.hash
    }
}

/// A PIN as the lock takes it: MIN_LEN to MAX_LEN digits.
pub fn valid(pin: &str) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&pin.chars().count()) && pin.chars().all(|c| c.is_ascii_digit())
}

fn hash(salt: &str, pin: &str) -> String {
    let mut digest = Sha256::new().chain_update(salt).chain_update(pin).finalize();
    for _ in 0..ROUNDS {
        digest = Sha256::new().chain_update(digest).chain_update(salt).finalize();
    }
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_pin_and_keeps_no_trace_of_it() {
        let lock = PinLock::new("2580");
        assert!(lock.matches("2580"));
        assert!(!lock.matches("2581") && !lock.matches(""));
        assert_eq!(lock.len, 4);
        let json = serde_json::to_string(&lock).unwrap();
        assert!(!json.contains("2580"));
        // Another salt, another hash.
        assert_ne!(PinLock::new("2580").hash, lock.hash);
    }

    #[test]
    fn only_four_to_eight_digits() {
        assert!(valid("1234") && valid("12345678"));
        assert!(!valid("123") && !valid("123456789") && !valid("12a4") && !valid(""));
    }
}
