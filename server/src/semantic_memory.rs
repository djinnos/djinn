use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result as AnyhowResult, bail};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::nomic_bert::{self, Config, NomicBertModel};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use serde::Serialize;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};
use tokio::sync::{Mutex, OnceCell};

const DEFAULT_MODEL_ID: &str = "nomic-ai/nomic-embed-text-v1.5";
const DEFAULT_MODEL_REVISION: &str = "main";
pub const DEFAULT_EMBEDDING_DIMENSION: usize = 768;
pub const DOCUMENT_PREFIX: &str = "search_document: ";
pub const QUERY_PREFIX: &str = "search_query: ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingModelDescriptor {
    pub provider: &'static str,
    pub model_id: String,
    pub revision: String,
    pub dimension: usize,
}

impl Default for EmbeddingModelDescriptor {
    fn default() -> Self {
        Self {
            provider: "huggingface/candle",
            model_id: DEFAULT_MODEL_ID.to_string(),
            revision: DEFAULT_MODEL_REVISION.to_string(),
            dimension: DEFAULT_EMBEDDING_DIMENSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum EmbeddingInputKind {
    Document,
    Query,
}

impl EmbeddingInputKind {
    fn prefix(&self) -> &'static str {
        match self {
            Self::Document => DOCUMENT_PREFIX,
            Self::Query => QUERY_PREFIX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmbeddingVector {
    pub values: Vec<f32>,
    pub model: EmbeddingModelDescriptor,
    pub input_kind: EmbeddingInputKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingDegraded {
    pub model: EmbeddingModelDescriptor,
    pub fallback: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum EmbeddingOutcome {
    Ready(EmbeddingVector),
    Degraded(EmbeddingDegraded),
}

impl EmbeddingOutcome {
    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded(_))
    }

    pub fn model(&self) -> &EmbeddingModelDescriptor {
        match self {
            Self::Ready(vector) => &vector.model,
            Self::Degraded(degraded) => &degraded.model,
        }
    }
}

#[derive(Clone)]
pub struct EmbeddingService {
    inner: Arc<EmbeddingServiceInner>,
}

struct EmbeddingServiceInner {
    model: EmbeddingModelDescriptor,
    cache_dir: PathBuf,
    loader: Arc<dyn EmbeddingRuntimeLoader>,
    runtime: OnceCell<Arc<dyn EmbeddingRuntime>>,
    load_failure: Mutex<Option<String>>,
}

impl EmbeddingService {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self::with_loader(
            EmbeddingModelDescriptor::default(),
            cache_dir,
            Arc::new(CandleEmbeddingRuntimeLoader),
        )
    }

    fn with_loader(
        model: EmbeddingModelDescriptor,
        cache_dir: PathBuf,
        loader: Arc<dyn EmbeddingRuntimeLoader>,
    ) -> Self {
        Self {
            inner: Arc::new(EmbeddingServiceInner {
                model,
                cache_dir,
                loader,
                runtime: OnceCell::new(),
                load_failure: Mutex::new(None),
            }),
        }
    }

    pub fn model_descriptor(&self) -> &EmbeddingModelDescriptor {
        &self.inner.model
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.cache_dir
    }

    pub async fn embed_document(&self, text: impl AsRef<str>) -> EmbeddingOutcome {
        self.embed(EmbeddingInputKind::Document, text.as_ref())
            .await
    }

    pub async fn embed_query(&self, text: impl AsRef<str>) -> EmbeddingOutcome {
        self.embed(EmbeddingInputKind::Query, text.as_ref()).await
    }

    async fn embed(&self, kind: EmbeddingInputKind, text: &str) -> EmbeddingOutcome {
        let runtime = match self.runtime().await {
            Ok(runtime) => runtime,
            Err(reason) => return self.degraded(reason),
        };

        match runtime.embed(kind.clone(), text).await {
            Ok(values) => EmbeddingOutcome::Ready(EmbeddingVector {
                values,
                model: self.inner.model.clone(),
                input_kind: kind,
            }),
            Err(error) => EmbeddingOutcome::Degraded(EmbeddingDegraded {
                model: self.inner.model.clone(),
                fallback: "fts-only",
                reason: format!("embedding runtime unavailable: {error:#}"),
            }),
        }
    }

    async fn runtime(&self) -> Result<Arc<dyn EmbeddingRuntime>, String> {
        if let Some(runtime) = self.inner.runtime.get() {
            return Ok(runtime.clone());
        }

        if let Some(error) = self.inner.load_failure.lock().await.clone() {
            return Err(error);
        }

        match self
            .inner
            .runtime
            .get_or_try_init(|| async {
                self.inner
                    .loader
                    .load(&self.inner.model, &self.inner.cache_dir)
                    .await
            })
            .await
        {
            Ok(runtime) => Ok(runtime.clone()),
            Err(error) => {
                let message = format!("failed to load embedding model: {error:#}");
                *self.inner.load_failure.lock().await = Some(message.clone());
                Err(message)
            }
        }
    }

    fn degraded(&self, reason: String) -> EmbeddingOutcome {
        EmbeddingOutcome::Degraded(EmbeddingDegraded {
            model: self.inner.model.clone(),
            fallback: "fts-only",
            reason,
        })
    }
}

#[async_trait::async_trait]
trait EmbeddingRuntimeLoader: Send + Sync {
    async fn load(
        &self,
        model: &EmbeddingModelDescriptor,
        cache_dir: &Path,
    ) -> AnyhowResult<Arc<dyn EmbeddingRuntime>>;
}

#[async_trait::async_trait]
trait EmbeddingRuntime: Send + Sync {
    async fn embed(&self, kind: EmbeddingInputKind, text: &str) -> AnyhowResult<Vec<f32>>;
}

struct CandleEmbeddingRuntimeLoader;

#[async_trait::async_trait]
impl EmbeddingRuntimeLoader for CandleEmbeddingRuntimeLoader {
    async fn load(
        &self,
        model: &EmbeddingModelDescriptor,
        cache_dir: &Path,
    ) -> AnyhowResult<Arc<dyn EmbeddingRuntime>> {
        let model = model.clone();
        let cache_dir = cache_dir.to_path_buf();
        let runtime =
            tokio::task::spawn_blocking(move || CandleEmbeddingRuntime::load(model, &cache_dir))
                .await
                .context("embedding model load task panicked")??;
        Ok(Arc::new(runtime))
    }
}

struct CandleEmbeddingRuntime {
    tokenizer: Mutex<Tokenizer>,
    model: Mutex<NomicBertModel>,
    device: Device,
}

impl CandleEmbeddingRuntime {
    fn load(model: EmbeddingModelDescriptor, cache_dir: &Path) -> AnyhowResult<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("create embedding cache dir {}", cache_dir.display()))?;

        let repo = Repo::with_revision(
            model.model_id.clone(),
            RepoType::Model,
            model.revision.clone(),
        );
        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir.to_path_buf())
            .with_progress(false)
            .build()
            .context("build Hugging Face API")?;
        let api = api.repo(repo);
        let config_path = api.get("config.json").context("download config.json")?;
        let tokenizer_path = api
            .get("tokenizer.json")
            .context("download tokenizer.json")?;
        let weights_path = api
            .get("model.safetensors")
            .context("download model.safetensors")?;

        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path)
                .with_context(|| format!("read {}", config_path.display()))?,
        )
        .context("parse nomic config")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(anyhow::Error::msg)
            .context("load tokenizer.json")?;
        if let Some(pp) = tokenizer.get_padding_mut() {
            pp.strategy = PaddingStrategy::BatchLongest;
        } else {
            tokenizer.with_padding(Some(PaddingParams {
                strategy: PaddingStrategy::BatchLongest,
                ..Default::default()
            }));
        }

        let device = Device::Cpu;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)
                .context("memory-map model weights")?
        };
        let model = NomicBertModel::load(vb, &config).context("load nomic bert model")?;

        Ok(Self {
            tokenizer: Mutex::new(tokenizer),
            model: Mutex::new(model),
            device,
        })
    }

    fn prepare_text(kind: &EmbeddingInputKind, text: &str) -> String {
        let trimmed = text.trim();
        format!("{}{}", kind.prefix(), trimmed)
    }

    fn tensor_to_vec(tensor: Tensor) -> AnyhowResult<Vec<f32>> {
        Ok(tensor
            .to_dtype(DType::F32)
            .context("convert embedding tensor to f32")?
            .to_vec1()
            .context("extract embedding vector")?)
    }
}

#[async_trait::async_trait]
impl EmbeddingRuntime for CandleEmbeddingRuntime {
    async fn embed(&self, kind: EmbeddingInputKind, text: &str) -> AnyhowResult<Vec<f32>> {
        if text.trim().is_empty() {
            bail!("cannot embed empty text")
        }

        let prepared = Self::prepare_text(&kind, text);
        let encoding = {
            let tokenizer = self.tokenizer.lock().await;
            tokenizer
                .encode(prepared, true)
                .map_err(anyhow::Error::msg)
                .context("tokenize embedding input")?
        };

        let token_ids = Tensor::new(encoding.get_ids(), &self.device)
            .context("build token id tensor")?
            .unsqueeze(0)
            .context("reshape token id tensor")?;
        let attention_mask = Tensor::new(encoding.get_attention_mask(), &self.device)
            .context("build attention mask tensor")?
            .unsqueeze(0)
            .context("reshape attention mask tensor")?;

        let hidden_states = {
            let model = self.model.lock().await;
            model
                .forward(&token_ids, None, Some(&attention_mask))
                .context("run embedding model")?
        };
        let pooled = nomic_bert::mean_pooling(&hidden_states, &attention_mask)
            .context("mean-pool embedding")?;
        let normalized = nomic_bert::l2_normalize(&pooled).context("normalize embedding")?;
        Self::tensor_to_vec(normalized.get(0).context("extract batch item 0")?)
    }
}

pub fn default_embedding_cache_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join(".djinn").join("models");
    }
    PathBuf::from(".djinn/models")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubLoader {
        loads: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl EmbeddingRuntimeLoader for StubLoader {
        async fn load(
            &self,
            _model: &EmbeddingModelDescriptor,
            _cache_dir: &Path,
        ) -> AnyhowResult<Arc<dyn EmbeddingRuntime>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                bail!("boom")
            }
            Ok(Arc::new(StubRuntime))
        }
    }

    struct StubRuntime;

    #[async_trait::async_trait]
    impl EmbeddingRuntime for StubRuntime {
        async fn embed(&self, kind: EmbeddingInputKind, text: &str) -> AnyhowResult<Vec<f32>> {
            Ok(vec![kind.prefix().len() as f32, text.len() as f32])
        }
    }

    #[tokio::test]
    async fn caches_runtime_across_calls() {
        let loads = Arc::new(AtomicUsize::new(0));
        let service = EmbeddingService::with_loader(
            EmbeddingModelDescriptor::default(),
            PathBuf::from("cache"),
            Arc::new(StubLoader {
                loads: loads.clone(),
                fail: false,
            }),
        );

        let first = service.embed_query("alpha").await;
        let second = service.embed_document("beta").await;

        assert!(matches!(first, EmbeddingOutcome::Ready(_)));
        assert!(matches!(second, EmbeddingOutcome::Ready(_)));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn returns_degraded_when_model_load_fails() {
        let service = EmbeddingService::with_loader(
            EmbeddingModelDescriptor::default(),
            PathBuf::from("cache"),
            Arc::new(StubLoader {
                loads: Arc::new(AtomicUsize::new(0)),
                fail: true,
            }),
        );

        let outcome = service.embed_query("alpha").await;
        match outcome {
            EmbeddingOutcome::Degraded(degraded) => {
                assert_eq!(degraded.fallback, "fts-only");
                assert!(degraded.reason.contains("failed to load embedding model"));
            }
            other => panic!("expected degraded outcome, got {other:?}"),
        }
    }
}
