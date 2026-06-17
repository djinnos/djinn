#![allow(dead_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{PlannedIndexerCommand, SupportedIndexer};

const SCHEMA_VERSION: &str = "v1";
const CACHE_DIR_ENV: &str = "DJINN_SCIP_CACHE_DIR";
const DEFAULT_CACHE_SUFFIX: &str = "djinn/scip-indexer";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolVersionRecord {
    pub binary_path: PathBuf,
    pub reported_version: String,
    pub override_env: Option<VersionOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VersionOverride {
    pub name: String,
    pub value: String,
}

impl ToolVersionRecord {
    pub(crate) fn new(
        indexer: SupportedIndexer,
        binary_path: impl Into<PathBuf>,
        reported_version: impl Into<String>,
        env: &BTreeMap<String, String>,
    ) -> Self {
        let override_name = version_override_env_name(indexer);
        let override_env = env.get(&override_name).map(|value| VersionOverride {
            name: override_name,
            value: value.clone(),
        });
        Self {
            binary_path: binary_path.into(),
            reported_version: reported_version.into(),
            override_env,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommandShape {
    pub binary_name: String,
    pub args: Vec<String>,
    pub working_directory: WorkspaceIdentity,
}

impl CommandShape {
    pub(crate) fn from_plan(plan: &PlannedIndexerCommand) -> Self {
        let output = plan.output_path.to_string_lossy();
        let args = plan
            .args
            .iter()
            .map(|arg| {
                if arg == output.as_ref() {
                    "$SCIP_OUTPUT".to_string()
                } else {
                    arg.clone()
                }
            })
            .collect();
        Self {
            binary_name: plan.indexer.binary_name().to_string(),
            args,
            working_directory: WorkspaceIdentity::from_plan(plan),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceIdentity {
    pub workspace_rel_root: PathBuf,
    pub workspace_slug: String,
}

impl WorkspaceIdentity {
    pub(crate) fn from_plan(plan: &PlannedIndexerCommand) -> Self {
        Self {
            workspace_rel_root: plan.workspace_rel_root.clone(),
            workspace_slug: plan.workspace_slug.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheKeyIngredients {
    pub schema_version: String,
    pub indexer: SupportedIndexer,
    pub tool_version: ToolVersionRecord,
    pub command: CommandShape,
    pub workspace: WorkspaceIdentity,
    pub source_hashes: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
    pub lockfile_hashes: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
}

impl CacheKeyIngredients {
    pub(crate) fn new(
        indexer: SupportedIndexer,
        tool_version: ToolVersionRecord,
        command: CommandShape,
        workspace: WorkspaceIdentity,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            indexer,
            tool_version,
            command,
            workspace,
            source_hashes: BTreeMap::new(),
            config_hashes: BTreeMap::new(),
            lockfile_hashes: BTreeMap::new(),
            environment: BTreeMap::new(),
        }
    }

    pub(crate) fn cache_key(&self) -> Result<ScipCacheKey> {
        let bytes = serde_json::to_vec(self).context("serialize SCIP cache key ingredients")?;
        Ok(ScipCacheKey(hex_sha256(&bytes)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ScipCacheKey(String);

impl ScipCacheKey {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScipCacheStore {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheManifest {
    schema_version: String,
    key: String,
    artifact_sha256: String,
    artifact_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheLookup {
    Hit,
    Miss,
}

impl ScipCacheStore {
    #[allow(dead_code)]
    pub(crate) fn from_environment() -> Self {
        Self::new(resolve_cache_root_from_env(|name| env::var(name).ok()))
    }

    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn lookup(&self, key: &ScipCacheKey, output_path: &Path) -> CacheLookup {
        match self.try_lookup(key, output_path) {
            Ok(true) => CacheLookup::Hit,
            Ok(false) | Err(_) => CacheLookup::Miss,
        }
    }

    pub(crate) fn store_artifact(&self, key: &ScipCacheKey, artifact_path: &Path) -> Result<()> {
        let bytes = fs::read(artifact_path)
            .with_context(|| format!("read SCIP artifact {}", artifact_path.display()))?;
        let entry = self.entry_dir(key);
        fs::create_dir_all(&entry)
            .with_context(|| format!("create SCIP cache entry dir {}", entry.display()))?;

        atomic_write_if_absent(&entry, "artifact.scip", &bytes)?;

        // Concurrent writers race only on the first artifact publication. Build
        // the manifest from the artifact that actually won that race so the
        // entry cannot end up with artifact A and manifest B.
        let stored_artifact = fs::read(entry.join("artifact.scip"))
            .with_context(|| format!("read published artifact for key {}", key.as_str()))?;
        let manifest = CacheManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            key: key.as_str().to_string(),
            artifact_sha256: hex_sha256(&stored_artifact),
            artifact_len: stored_artifact.len() as u64,
        };
        let manifest_bytes =
            serde_json::to_vec_pretty(&manifest).context("serialize cache manifest")?;

        atomic_write_if_absent(&entry, "manifest.json", &manifest_bytes)?;
        Ok(())
    }

    fn try_lookup(&self, key: &ScipCacheKey, output_path: &Path) -> Result<bool> {
        let entry = self.entry_dir(key);
        let manifest_path = entry.join("manifest.json");
        let artifact_path = entry.join("artifact.scip");
        let manifest_bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", manifest_path.display()));
            }
        };
        let manifest: CacheManifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("parse {}", manifest_path.display()))?;
        if manifest.schema_version != SCHEMA_VERSION || manifest.key != key.as_str() {
            return Ok(false);
        }

        let artifact = match fs::read(&artifact_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", artifact_path.display()));
            }
        };
        if artifact.len() as u64 != manifest.artifact_len
            || hex_sha256(&artifact) != manifest.artifact_sha256
        {
            return Ok(false);
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create SCIP output dir {}", parent.display()))?;
        }
        atomic_replace(output_path, &artifact)
            .with_context(|| format!("copy cached SCIP artifact to {}", output_path.display()))?;
        Ok(true)
    }

    fn entry_dir(&self, key: &ScipCacheKey) -> PathBuf {
        self.root
            .join(SCHEMA_VERSION)
            .join(&key.as_str()[..2])
            .join(key.as_str())
    }
}

fn resolve_cache_root_from_env(mut get_env: impl FnMut(&str) -> Option<String>) -> PathBuf {
    if let Some(path) = get_env(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = get_env("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path).join(DEFAULT_CACHE_SUFFIX);
    }
    if let Some(path) = get_env("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path)
            .join(".cache")
            .join(DEFAULT_CACHE_SUFFIX);
    }
    PathBuf::from(".cache").join(DEFAULT_CACHE_SUFFIX)
}

fn version_override_env_name(indexer: SupportedIndexer) -> String {
    let language = indexer.language();
    let mut name = String::from("SCIP_");
    for ch in language.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }
    name.push_str("_VERSION");
    name
}

fn atomic_write_if_absent(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<()> {
    let final_path = dir.join(file_name);
    if final_path.exists() {
        return Ok(());
    }
    let temp_path = unique_temp_path(dir, file_name);
    fs::write(&temp_path, bytes).with_context(|| format!("write {}", temp_path.display()))?;
    match fs::hard_link(&temp_path, &final_path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(error).with_context(|| format!("publish {}", final_path.display()))
        }
    }
}

fn atomic_replace(final_path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scip-output");
    let temp_path = unique_temp_path(parent, file_name);
    fs::write(&temp_path, bytes).with_context(|| format!("write {}", temp_path.display()))?;
    fs::rename(&temp_path, final_path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), final_path.display()))?;
    Ok(())
}

fn unique_temp_path(dir: &Path, file_name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let thread = format!("{:?}", std::thread::current().id());
    dir.join(format!(".{file_name}.{nanos}.{thread}.tmp"))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn tempdir_in_workspace() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-scip-cache-")
            .tempdir_in(".")
            .expect("create tempdir")
    }

    fn fake_plan(output_path: PathBuf) -> PlannedIndexerCommand {
        PlannedIndexerCommand {
            indexer: SupportedIndexer::TypeScript,
            binary_path: PathBuf::from("/usr/bin/scip-typescript"),
            args: vec![
                "index".to_string(),
                "--output".to_string(),
                output_path.to_string_lossy().into_owned(),
            ],
            working_directory: PathBuf::from("/abs/tmp/work/repo/ui"),
            workspace_root: PathBuf::from("/abs/tmp/work/repo/ui"),
            workspace_rel_root: PathBuf::from("ui"),
            workspace_slug: "ui".to_string(),
            output_path,
        }
    }

    fn base_ingredients() -> CacheKeyIngredients {
        let plan = fake_plan(PathBuf::from("/run/one/out/index.scip"));
        let mut env = BTreeMap::new();
        env.insert("NODE_ENV".to_string(), "production".to_string());
        let tool = ToolVersionRecord::new(
            plan.indexer,
            plan.binary_path.clone(),
            "scip-typescript 1.2.3",
            &env,
        );
        let mut ingredients = CacheKeyIngredients::new(
            plan.indexer,
            tool,
            CommandShape::from_plan(&plan),
            WorkspaceIdentity::from_plan(&plan),
        );
        ingredients
            .source_hashes
            .insert("src/main.ts".to_string(), "source-a".to_string());
        ingredients
            .config_hashes
            .insert("tsconfig.json".to_string(), "config-a".to_string());
        ingredients
            .lockfile_hashes
            .insert("pnpm-lock.yaml".to_string(), "lock-a".to_string());
        ingredients.environment = env;
        ingredients
    }

    fn key(ingredients: &CacheKeyIngredients) -> String {
        ingredients
            .cache_key()
            .expect("cache key")
            .as_str()
            .to_string()
    }

    #[test]
    fn cache_root_prefers_explicit_env_then_xdg_then_home() {
        assert_eq!(
            resolve_cache_root_from_env(|name| match name {
                CACHE_DIR_ENV => Some("/cache/scip".to_string()),
                _ => None,
            }),
            PathBuf::from("/cache/scip")
        );
        assert_eq!(
            resolve_cache_root_from_env(|name| match name {
                "XDG_CACHE_HOME" => Some("/xdg".to_string()),
                "HOME" => Some("/home/me".to_string()),
                _ => None,
            }),
            PathBuf::from("/xdg").join(DEFAULT_CACHE_SUFFIX)
        );
        assert_eq!(
            resolve_cache_root_from_env(|name| match name {
                "HOME" => Some("/home/me".to_string()),
                _ => None,
            }),
            PathBuf::from("/home/me/.cache").join(DEFAULT_CACHE_SUFFIX)
        );
    }

    #[test]
    fn cache_key_ignores_absolute_output_path() {
        let first = base_ingredients();
        let plan = fake_plan(PathBuf::from("/different/tmp/out/index.scip"));
        let mut second = first.clone();
        second.command = CommandShape::from_plan(&plan);
        assert_eq!(key(&first), key(&second));
    }

    #[test]
    fn cache_key_changes_on_tool_version_and_override_changes() {
        let base = base_ingredients();

        let mut version_changed = base.clone();
        version_changed.tool_version.reported_version = "scip-typescript 9.9.9".to_string();
        assert_ne!(key(&base), key(&version_changed));

        let mut env = BTreeMap::new();
        env.insert(
            "SCIP_TYPESCRIPT_VERSION".to_string(),
            "override-a".to_string(),
        );
        let mut override_a = base.clone();
        override_a.tool_version = ToolVersionRecord::new(
            SupportedIndexer::TypeScript,
            "/usr/bin/scip-typescript",
            "scip-typescript 1.2.3",
            &env,
        );

        env.insert(
            "SCIP_TYPESCRIPT_VERSION".to_string(),
            "override-b".to_string(),
        );
        let mut override_b = base.clone();
        override_b.tool_version = ToolVersionRecord::new(
            SupportedIndexer::TypeScript,
            "/usr/bin/scip-typescript",
            "scip-typescript 1.2.3",
            &env,
        );
        assert_ne!(key(&base), key(&override_a));
        assert_ne!(key(&override_a), key(&override_b));
    }

    #[test]
    fn cache_key_changes_on_source_config_lock_and_environment_hashes() {
        let base = base_ingredients();

        let mut source_changed = base.clone();
        source_changed
            .source_hashes
            .insert("src/main.ts".to_string(), "source-b".to_string());
        assert_ne!(key(&base), key(&source_changed));

        let mut config_changed = base.clone();
        config_changed
            .config_hashes
            .insert("tsconfig.json".to_string(), "config-b".to_string());
        assert_ne!(key(&base), key(&config_changed));

        let mut lock_changed = base.clone();
        lock_changed
            .lockfile_hashes
            .insert("pnpm-lock.yaml".to_string(), "lock-b".to_string());
        assert_ne!(key(&base), key(&lock_changed));

        let mut env_changed = base.clone();
        env_changed
            .environment
            .insert("NODE_ENV".to_string(), "development".to_string());
        assert_ne!(key(&base), key(&env_changed));
    }

    #[test]
    fn cache_key_changes_on_relevant_command_shape_changes() {
        let base = base_ingredients();
        let mut changed = base.clone();
        changed.command.args.push("--infer-tsconfig".to_string());
        assert_ne!(key(&base), key(&changed));

        let mut workspace_changed = base.clone();
        workspace_changed.workspace.workspace_rel_root = PathBuf::from("packages/app");
        assert_ne!(key(&base), key(&workspace_changed));
    }

    #[test]
    fn hit_copies_artifact_to_planned_output_path() {
        let tmp = tempdir_in_workspace();
        let store = ScipCacheStore::new(tmp.path().join("cache"));
        let key = base_ingredients().cache_key().expect("key");
        let artifact = tmp.path().join("artifact.scip");
        fs::write(&artifact, b"scip bytes").expect("write artifact");
        store
            .store_artifact(&key, &artifact)
            .expect("store artifact");

        let output = tmp.path().join("planned/out/index.scip");
        assert_eq!(store.lookup(&key, &output), CacheLookup::Hit);
        assert_eq!(fs::read(&output).expect("read output"), b"scip bytes");
    }

    #[test]
    fn missing_corrupt_and_hash_mismatch_are_misses() {
        let tmp = tempdir_in_workspace();
        let store = ScipCacheStore::new(tmp.path().join("cache"));
        let key = base_ingredients().cache_key().expect("key");
        let output = tmp.path().join("out.scip");
        assert_eq!(store.lookup(&key, &output), CacheLookup::Miss);

        let artifact = tmp.path().join("artifact.scip");
        fs::write(&artifact, b"good").expect("write artifact");
        store
            .store_artifact(&key, &artifact)
            .expect("store artifact");
        let entry = store.entry_dir(&key);

        fs::write(entry.join("manifest.json"), b"not json").expect("corrupt manifest");
        assert_eq!(store.lookup(&key, &output), CacheLookup::Miss);

        store
            .store_artifact(&key, &artifact)
            .expect("idempotent store");
        let manifest = CacheManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            key: key.as_str().to_string(),
            artifact_sha256: hex_sha256(b"different"),
            artifact_len: 4,
        };
        fs::write(
            entry.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest json"),
        )
        .expect("write mismatch manifest");
        assert_eq!(store.lookup(&key, &output), CacheLookup::Miss);
    }

    #[test]
    fn version_bump_is_cache_miss() {
        let tmp = tempdir_in_workspace();
        let store = ScipCacheStore::new(tmp.path().join("cache"));
        let key = base_ingredients().cache_key().expect("key");
        let artifact = tmp.path().join("artifact.scip");
        fs::write(&artifact, b"good").expect("write artifact");
        store
            .store_artifact(&key, &artifact)
            .expect("store artifact");

        let entry = store.entry_dir(&key);
        let manifest = CacheManifest {
            schema_version: "v2".to_string(),
            key: key.as_str().to_string(),
            artifact_sha256: hex_sha256(b"good"),
            artifact_len: 4,
        };
        fs::write(
            entry.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest json"),
        )
        .expect("write v2 manifest");

        assert_eq!(
            store.lookup(&key, &tmp.path().join("out.scip")),
            CacheLookup::Miss
        );
    }

    #[test]
    fn concurrent_atomic_writes_leave_one_valid_artifact_without_partials() {
        let tmp = tempdir_in_workspace();
        let store = Arc::new(ScipCacheStore::new(tmp.path().join("cache")));
        let key = base_ingredients().cache_key().expect("key");
        let first = tmp.path().join("first.scip");
        let second = tmp.path().join("second.scip");
        fs::write(&first, b"first artifact").expect("write first");
        fs::write(&second, b"second artifact").expect("write second");

        let handles = [first, second]
            .into_iter()
            .map(|artifact| {
                let store = Arc::clone(&store);
                let key = key.clone();
                thread::spawn(move || store.store_artifact(&key, &artifact).expect("store"))
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().expect("join");
        }

        let output = tmp.path().join("out.scip");
        assert_eq!(store.lookup(&key, &output), CacheLookup::Hit);
        let copied = fs::read(&output).expect("read output");
        assert!(copied == b"first artifact" || copied == b"second artifact");

        let entry = store.entry_dir(&key);
        let partials = fs::read_dir(&entry)
            .expect("read entry")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(partials, 0, "no temp files should remain visible");
    }
}
