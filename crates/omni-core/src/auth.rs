use anyhow::Result;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use rand::Rng;

/// Argon2id parameters (plan P2.2): 64 MiB memory, 3 passes, 1 lane.
///
/// `Argon2::default()` is 19 MiB / t=2, the OWASP *minimum*. These are the
/// values the plan fixes, and they are a deliberate choice for a machine that
/// hashes a password a handful of times a day and never in the ingest path.
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_PARALLELISM: u32 = 1;

fn hasher() -> Result<Argon2<'static>> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        None,
    )
    .map_err(|e| anyhow::anyhow!("Invalid Argon2 parameters: {}", e))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = hasher()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Argon2 hashing error: {}", e))?
        .to_string();
    Ok(hash)
}

/// Verify against a stored PHC string.
///
/// Verification uses `Argon2::default()` deliberately: the PHC string carries
/// its own `m`, `t` and `p`, and the verifier takes them from there. Hashes
/// written before the parameters above still validate — which is the whole
/// point of storing PHC rather than a bare digest.
pub fn verify_password(password: &str, password_hash: &str) -> bool {
    let parsed_hash = match PasswordHash::new(password_hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

pub fn generate_session_token() -> String {
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 32] = rng.gen();
    hex::encode(random_bytes)
}

mod hex {
    pub fn encode(bytes: [u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hashing_and_verification() {
        let raw = "SuperSecretMcrPass123!";
        let hashed = hash_password(raw).expect("Hashing should succeed");
        assert!(verify_password(raw, &hashed));
        assert!(!verify_password("WrongPassword!", &hashed));
    }

    #[test]
    fn new_hashes_carry_the_configured_argon2id_parameters() {
        // The PHC string is what a later verify reads its cost from, so the
        // parameters have to actually reach it -- a hasher built with the right
        // params but discarded would still verify fine and cost 19 MiB.
        let hashed = hash_password("whatever").unwrap();
        assert!(hashed.starts_with("$argon2id$"), "got {hashed}");
        assert!(hashed.contains("m=65536"), "got {hashed}");
        assert!(hashed.contains("t=3"), "got {hashed}");
        assert!(hashed.contains("p=1"), "got {hashed}");
    }

    #[test]
    fn hashes_written_with_the_old_weaker_parameters_still_verify() {
        // Existing admin accounts were hashed with Argon2::default() (m=19456,
        // t=2). Raising the parameters must not lock those operators out.
        use argon2::password_hash::{PasswordHasher, SaltString};
        let salt = SaltString::from_b64("c29tZXNhbHR2YWx1ZQ").unwrap();
        let legacy = Argon2::default()
            .hash_password(b"LegacyPass1!", &salt)
            .unwrap()
            .to_string();
        assert!(legacy.contains("m=19456"), "precondition: {legacy}");
        assert!(verify_password("LegacyPass1!", &legacy));
        assert!(!verify_password("wrong", &legacy));
    }

    #[test]
    fn test_session_token_uniqueness() {
        let t1 = generate_session_token();
        let t2 = generate_session_token();
        assert_eq!(t1.len(), 64);
        assert_eq!(t2.len(), 64);
        assert_ne!(t1, t2);
    }
}

