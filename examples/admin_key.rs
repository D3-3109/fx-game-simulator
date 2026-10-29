//! Administrator PSK provisioning for container starts. Not a production entry point.
//!
//! Writes the `{salt, hash}` administrator config format, using the
//! exact PBKDF2 call `App::admin_login` verifies against. Never overwrites an
//! existing file and never echoes the PSK, so callers may log its stdout safely.
//!
//!   FX_ADMIN_PSK=secret cargo run --example admin_key -- .pi/admin-key.json
//!   cargo run --example admin_key -- .pi/admin-key.json   # generates a random PSK
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: admin_key <output.json>")?;
    let (psk, generated) = match std::env::var("FX_ADMIN_PSK") {
        Ok(value) if !value.is_empty() => (value, false),
        _ => {
            let bytes: [u8; 18] = rand::random();
            (hex::encode(bytes), true)
        }
    };
    if !(1..=256).contains(&psk.chars().count()) {
        return Err("FX_ADMIN_PSK must contain 1-256 characters".into());
    }
    let salt: [u8; 16] = rand::random();
    let mut hash = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(psk.as_bytes(), &salt, 200_000, &mut hash);
    let config = serde_json::json!({"salt": hex::encode(salt), "hash": hex::encode(hash)});
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|e| format!("refusing to overwrite {path}: {e}"))?;
    file.write_all(config.to_string().as_bytes())?;
    if generated {
        // The only time the PSK is ever printed: nobody else knows it yet.
        println!("{psk}");
    }
    Ok(())
}
