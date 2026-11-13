use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};

/// Canonicalise an Ed25519 OpenSSH public key to `ssh-ed25519 <base64>`, dropping the comment.
pub fn key_identity(key: &str) -> Result<String> {
    let fields: Vec<_> = key.split_whitespace().collect();
    ensure!(
        fields.len() >= 2 && fields[0] == "ssh-ed25519",
        "expected an Ed25519 public key"
    );
    ensure!(
        fields[1].len() == 68 && fields[1].starts_with("AAAAC3NzaC1lZDI1NTE5AAAAI"),
        "invalid Ed25519 key encoding"
    );
    ensure!(
        fields[1]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte)),
        "invalid public key characters"
    );
    ensure!(
        !key.contains(['\n', '\r']),
        "public key must be a single line"
    );
    let decoded = STANDARD.decode(fields[1])?;
    ensure!(
        decoded.len() == 51
            && decoded[..4] == [0, 0, 0, 11]
            && decoded[4..15] == *b"ssh-ed25519"
            && decoded[15..19] == [0, 0, 0, 32],
        "invalid SSH public key structure"
    );
    Ok(format!("{} {}", fields[0], fields[1]))
}
