//! Identity checks shared by the benchmark runner, judge, and report.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::{Arm, GradeRecord, Manifest, QuestionSet, SessionRecord, load_json};

const RUN_META: &str = "run-meta.json";

/// Hash the JSON representation of a benchmark input.
///
/// # Errors
///
/// Returns a serialization error if the input has no JSON representation.
pub fn fingerprint(input: &impl Serialize) -> anyhow::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(input)?)
        .to_hex()
        .to_string())
}

/// Read and verify the same question bytes that the caller will parse.
///
/// # Errors
///
/// Returns an error if the file does not match the manifest or is invalid JSON.
pub fn load_questions(path: &Path, manifest: &Manifest) -> anyhow::Result<QuestionSet> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let hash = blake3::hash(&bytes).to_hex();
    if hash.as_str() != manifest.question_set_hash {
        bail!(
            "question-set hash drift: {} hashes to {hash}, but the manifest pins {}",
            path.display(),
            manifest.question_set_hash
        );
    }
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Immutable identity of the measurement inputs for a session ledger.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub manifest_hash: String,
    pub measurement_hash: String,
    pub agent_hash: String,
}

impl RunMeta {
    /// Load the identity for a session ledger and check its manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, legacy, or incompatible metadata.
    pub fn verify(sessions: &Path, manifest: &Manifest) -> anyhow::Result<Self> {
        let path = sessions.with_file_name(RUN_META);
        let meta: Self = load_json(&path)
            .context("sessions lack valid measurement provenance; use a fresh results directory")?;
        if meta.manifest_hash != fingerprint(manifest)? {
            bail!("session manifest drift; use a fresh results directory");
        }
        Ok(meta)
    }

    /// Check a results directory without changing it. Return true if initialized.
    ///
    /// # Errors
    ///
    /// Returns an error if a previous measurement does not match these inputs.
    pub fn check(&self, sessions: &Path) -> anyhow::Result<bool> {
        let path = sessions.with_file_name(RUN_META);
        if !path.try_exists()? && !sessions.try_exists()? {
            return Ok(false);
        }
        let previous: Self = load_json(&path)
            .context("sessions lack valid measurement provenance; use a fresh results directory")?;
        if *self != previous {
            bail!(
                "measurement fingerprint drift; use a fresh results directory (even with --force)"
            );
        }
        Ok(true)
    }

    /// Publish the identity exactly once, before writing sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata cannot be written or already exists.
    pub fn initialize(&self, sessions: &Path) -> anyhow::Result<()> {
        let path = sessions.with_file_name(RUN_META);
        let mut temp = tempfile::NamedTempFile::new_in(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
        serde_json::to_writer(&mut temp, self)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(())
    }
}

/// Locate a standalone executable without depending on a sandbox's PATH.
///
/// # Errors
///
/// Returns an error if no file can be found or resolved.
pub fn resolve_executable(env: &str, default: &str) -> anyhow::Result<PathBuf> {
    let name = std::env::var_os(env).unwrap_or_else(|| default.into());
    let path = Path::new(&name);
    if path.components().count() > 1 {
        return path
            .canonicalize()
            .with_context(|| format!("resolving {env}"));
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(path);
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("resolving {env}"));
        }
    }
    bail!("cannot locate {name:?}; set {env} to the standalone executable");
}

/// One grade with the identity of its exact session and grading inputs.
#[derive(Debug, Serialize, Deserialize)]
pub struct BoundGrade {
    #[serde(flatten)]
    pub grade: GradeRecord,
    pub session_hash: String,
    pub grading_hash: String,
}

/// Key of a trial within a ledger.
pub type TrialKey<'a> = (&'a str, Arm, u32);

/// Return the key of a session in the ledger.
pub fn session_key(session: &SessionRecord) -> TrialKey<'_> {
    (&session.question_id, session.arm, session.run_index)
}

/// Return the key of a grade in the ledger.
pub fn grade_key(grade: &GradeRecord) -> TrialKey<'_> {
    (&grade.question_id, grade.arm, grade.run_index)
}
