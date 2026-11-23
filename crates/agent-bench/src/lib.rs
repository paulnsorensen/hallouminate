//! Shared model types and IO helpers for the wiki-grounding benchmark pilot.

pub mod model;
pub use model::*;

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Load and deserialize a JSON file, with the path attached to any error.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let value = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing JSON from {}", path.display()))?;
    Ok(value)
}

/// Load and deserialize a TOML file, with the path attached to any error.
pub fn load_toml<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value =
        toml::from_str(&text).with_context(|| format!("parsing TOML from {}", path.display()))?;
    Ok(value)
}

/// Stream-hash a file's contents with blake3, returning lowercase hex.
///
/// Depends on the `blake3` crate directly rather than
/// `hallouminate-domain::corpus::hasher::blake3_file` to avoid pulling that
/// crate's tokenizer/calamine/lancedb build weight into `agent-bench`.
pub fn blake3_file_hash(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Read a JSON-lines file into a `Vec<T>`, one deserialized value per line.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("parsing JSONL line from {}", path.display()))
        })
        .collect()
}

/// Append a single value as one JSON line to `path`, creating it if needed.
pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)
        .with_context(|| format!("writing JSONL to {}", path.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("writing JSONL to {}", path.display()))?;
    Ok(())
}

/// Atomically replace a JSON Lines ledger with the supplied records.
///
/// # Errors
///
/// Returns an error if serialization, flushing, syncing, or replacement fails.
pub fn replace_jsonl<T: Serialize>(path: &Path, records: &[T]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging JSONL for {}", path.display()))?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        for record in records {
            serde_json::to_writer(&mut writer, record)
                .with_context(|| format!("writing JSONL for {}", path.display()))?;
            writer
                .write_all(b"\n")
                .with_context(|| format!("writing JSONL for {}", path.display()))?;
        }
        writer
            .flush()
            .with_context(|| format!("flushing JSONL for {}", path.display()))?;
    }
    temp.as_file()
        .sync_all()
        .with_context(|| format!("syncing JSONL for {}", path.display()))?;
    temp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Verify the agent CLI at `bin` reports the version the manifest pins.
///
/// `manifest.claude_code_version` is recorded provenance; recording it
/// without checking it certifies a fact nothing verified. `claude --version`
/// prints `<version> (Claude Code)`, so the leading whitespace-delimited
/// token is the version. Callers run this once, before spawning any measured
/// session, so a drifted CLI aborts instead of producing numbers the
/// manifest misdescribes.
pub fn verify_agent_cli_version(bin: &str, expected_version: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running `{bin} --version`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`{bin} --version` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual = stdout.split_whitespace().next().unwrap_or("");
    if actual != expected_version {
        anyhow::bail!(
            "agent CLI version drift: manifest.claude_code_version = {expected_version}, \
             but `{bin} --version` reports {actual:?} \u{2014} refusing to start any \
             measured run under an unpinned agent CLI",
        );
    }
    Ok(())
}

/// Repository root, resolved at compile time from this crate's location
/// under `crates/agent-bench` — independent of the process cwd.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/agent-bench has a workspace root two levels up")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{read_jsonl, replace_jsonl};
    use serde::{Serialize, Serializer};
    use std::path::Path;

    enum CheckpointRecord<'a> {
        Complete(u8),
        Interrupted(&'a Path),
    }

    impl Serialize for CheckpointRecord<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Self::Complete(value) => serializer.serialize_u8(*value),
                Self::Interrupted(path) => {
                    assert_eq!(read_jsonl::<u8>(path).unwrap(), [1, 2]);
                    Err(serde::ser::Error::custom("interrupted serialization"))
                }
            }
        }
    }

    #[test]
    fn failed_checkpoint_leaves_the_last_complete_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        replace_jsonl(&ledger, &[1u8, 2]).unwrap();

        let records = [
            CheckpointRecord::Complete(3),
            CheckpointRecord::Interrupted(&ledger),
        ];
        let error = replace_jsonl(&ledger, &records).unwrap_err();
        assert_eq!(error.root_cause().to_string(), "interrupted serialization");
        assert_eq!(read_jsonl::<u8>(&ledger).unwrap(), [1, 2]);

        replace_jsonl(&ledger, &[3u8, 4]).unwrap();
        assert_eq!(read_jsonl::<u8>(&ledger).unwrap(), [3, 4]);
    }
}
