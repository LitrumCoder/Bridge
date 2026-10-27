//! Network profiles on disk (`profiles.toml` next to the trust store, ADR-0016).

use std::path::Path;

use bridge_core::state::NetworkProfile;
use bridge_protocol::{base32_decode, base32_encode};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default, rename = "profile")]
    profiles: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    #[serde(default)]
    auto_join: bool,
    /// Network fingerprints, base32.
    #[serde(default)]
    networks: Vec<String>,
}

pub fn load(path: &Path) -> Vec<NetworkProfile> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let file: File = toml::from_str(&text).unwrap_or_default();
    file.profiles
        .into_iter()
        .map(|e| NetworkProfile {
            name: e.name,
            auto_join: e.auto_join,
            fingerprints: e
                .networks
                .iter()
                .filter_map(|n| base32_decode(n)?.try_into().ok())
                .collect(),
        })
        .collect()
}

pub fn save(path: &Path, profiles: &[NetworkProfile]) -> std::io::Result<()> {
    let file = File {
        profiles: profiles
            .iter()
            .map(|p| Entry {
                name: p.name.clone(),
                auto_join: p.auto_join,
                networks: p.fingerprints.iter().map(|f| base32_encode(f)).collect(),
            })
            .collect(),
    };
    let text = toml::to_string_pretty(&file).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_round_trip() {
        let dir = std::env::temp_dir().join(format!("bridge-profiles-{}", std::process::id()));
        let path = dir.join("profiles.toml");
        let p = vec![NetworkProfile {
            name: "Дом".into(),
            fingerprints: vec![[7; 32], [9; 32]],
            auto_join: true,
        }];
        save(&path, &p).expect("save");
        assert_eq!(load(&path), p);
        assert!(load(&dir.join("missing.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
