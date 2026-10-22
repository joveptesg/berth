use std::{fs, io::Write, path::Path};

use anyhow::Context;

const FILE_NAME: &str = "secret.key";
const ENV_VAR: &str = "BERTH_SECRET_KEY";

/// 256-bit server key, kept outside the database: back it up separately.
pub struct SecretKey([u8; 32]);

impl SecretKey {
    /// Reads the key from `BERTH_SECRET_KEY` or `<data_dir>/secret.key`, generating the file on first start.
    pub(crate) fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        if let Ok(hex) = std::env::var(ENV_VAR) {
            return Self::from_hex(&hex).with_context(|| format!("invalid {ENV_VAR}"));
        }
        let path = data_dir.join(FILE_NAME);
        if path.exists() {
            let hex = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            return Self::from_hex(&hex).with_context(|| format!("invalid {}", path.display()));
        }

        let mut key = [0; 32];
        getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&path)
            .and_then(|mut file| file.write_all(hex::encode(key).as_bytes()))
            .with_context(|| format!("failed to write {}", path.display()))?;
        tracing::info!(path = %path.display(), "generated secret key");
        Ok(Self(key))
    }

    /// Raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn from_hex(hex: &str) -> anyhow::Result<Self> {
        let mut key = [0; 32];
        hex::decode_to_slice(hex.trim(), &mut key)?;
        Ok(Self(key))
    }
}
