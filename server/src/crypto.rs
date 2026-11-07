use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

pub fn random_token() -> String {
    let mut bytes = [0u8;32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
pub fn digest(value: &str) -> String { hex::encode(Sha256::digest(value.as_bytes())) }
pub fn encrypt(key: &[u8;32], plaintext: &str) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "Invalid cipher key")?;
    let mut nonce = [0u8;12];
    OsRng.fill_bytes(&mut nonce);
    let encrypted = cipher.encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes()).map_err(|_| "Encryption failed")?;
    let mut data = nonce.to_vec();
    data.extend_from_slice(&encrypted);
    Ok(STANDARD.encode(data))
}
pub fn decrypt(key: &[u8;32], data: &str) -> Result<String, String> {
    let bytes = STANDARD.decode(data).map_err(|_| "Malformed ciphertext")?;
    if bytes.len() < 28 { return Err("Malformed ciphertext".into()); }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "Invalid cipher key")?;
    let plain = cipher.decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..]).map_err(|_| "Decryption failed; verify APP_ENCRYPTION_KEY")?;
    String::from_utf8(plain).map_err(|_| "Invalid plaintext".into())
}
#[cfg(test)]
mod tests {
    #[test]
    fn encrypt_roundtrip() {
        let k = [7u8;32];
        let cipher = super::encrypt(&k, "secret%123").unwrap();
        assert_ne!(cipher, "secret%123");
        assert_eq!(super::decrypt(&k, &cipher).unwrap(), "secret%123");
    }
}
