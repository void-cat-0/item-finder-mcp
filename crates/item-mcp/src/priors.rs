//! Startup-only, immutable priors loaded from the exact bytes whose hash is published.
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use item_query::affordance::{BUILTIN_PRIORS_REF, Priors};
use rmcp::schemars;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const MAX_PRIORS_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PriorsProfile {
    pub profile: String,
    pub builtin_version: String,
    pub content_sha256: Option<String>,
    pub priors_ref: String,
}

#[derive(Debug, Clone)]
pub struct PriorsSnapshot {
    priors: Arc<Priors>,
    profile: PriorsProfile,
}

impl Default for PriorsSnapshot {
    fn default() -> Self {
        Self {
            priors: Arc::new(Priors::builtin().clone()),
            profile: PriorsProfile {
                profile: "builtin".into(),
                builtin_version: "g4-priors-1".into(),
                content_sha256: None,
                priors_ref: BUILTIN_PRIORS_REF.into(),
            },
        }
    }
}

impl PriorsSnapshot {
    /// The path is resolved once at startup. Errors deliberately omit file contents,
    /// paths and parser diagnostics (which can include sensitive configuration).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !path.as_os_str().is_empty(),
            "The configured priors path is empty."
        );
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|_| anyhow::anyhow!("The priors startup directory is unavailable."))?
                .join(path)
        };
        let file = std::fs::File::open(path)
            .map_err(|_| anyhow::anyhow!("The configured priors file could not be read."))?;
        let mut bytes = Vec::new();
        file.take(MAX_PRIORS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("The configured priors file could not be read."))?;
        Self::from_bytes(&bytes)
    }

    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_PRIORS_BYTES,
            "The configured priors file exceeds 1 MiB."
        );
        let text = std::str::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("The configured priors file must be UTF-8."))?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        let reference = format!("{BUILTIN_PRIORS_REF}+configured-default:sha256:{hash}");
        let priors = Priors::from_toml_str(text, reference.clone())
            .map_err(|_| anyhow::anyhow!("The configured priors file is invalid."))?;
        Ok(Self {
            priors: Arc::new(priors),
            profile: PriorsProfile {
                profile: "configured-default".into(),
                builtin_version: "g4-priors-1".into(),
                content_sha256: Some(hash),
                priors_ref: reference,
            },
        })
    }

    pub(crate) fn priors(&self) -> &Priors {
        &self.priors
    }

    pub(crate) fn profile(&self) -> PriorsProfile {
        self.profile.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_hash_and_rules_use_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.toml");
        let bytes = b"[[object]]\nlabel='keys'\nlength_m=[0.01,0.02]\n";
        std::fs::write(&path, bytes).unwrap();
        let snapshot = PriorsSnapshot::load(&path).unwrap();
        std::fs::write(&path, "not valid TOML").unwrap();
        assert_eq!(
            snapshot.profile.content_sha256,
            Some(format!("{:x}", Sha256::digest(bytes)))
        );
        assert_eq!(
            snapshot.priors().object("keys").unwrap().extents[0]
                .unwrap()
                .max,
            0.02
        );
        assert_eq!(snapshot.priors().reference(), snapshot.profile.priors_ref);
        assert!(
            !serde_json::to_string(&snapshot.profile)
                .unwrap()
                .contains("private.toml")
        );
    }

    #[test]
    fn configured_failure_never_falls_back_or_leaks() {
        for bytes in [
            vec![0xff],
            vec![b'a'; MAX_PRIORS_BYTES as usize + 1],
            b"token='secret'\n[[object]]\nlabel='secret'\nlength_m=[-1,2]".to_vec(),
        ] {
            let error = PriorsSnapshot::from_bytes(&bytes).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        let builtin = PriorsSnapshot::default();
        assert_eq!(builtin.profile.priors_ref, BUILTIN_PRIORS_REF);
        assert_eq!(builtin.profile.content_sha256, None);
    }
}
