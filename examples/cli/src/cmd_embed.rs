//! OneAI embed subcommand — embedding generation and service management.
//!
//! Subcommands:
//!   oneai embed generate <text> — Generate an embedding for the given text
//!   oneai embed batch <text1,text2,...> — Generate embeddings for multiple texts
//!   oneai embed list              — List available embedding models
//!   oneai embed health            — Check embedding service health
//!   oneai embed dimension         — Show embedding dimension for configured model

use oneai_core::{EmbeddingModel, EmbeddingConfig};
use oneai_rag::EmbeddingConfigExt;

/// Generate an embedding for a single text string.
pub fn cmd_embed_generate(text: &str, model: Option<&str>, service_type: Option<&str>, api_key: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        let embedding_service = create_embedding_service(model, service_type, api_key);
        match embedding_service {
            Ok(service) => {
                let embedding = service.embed(text).await;
                match embedding {
                    Ok(vec) => {
                        println!("Embedding generated successfully.");
                        println!("Model: {}", service.model().as_str());
                        println!("Dimension: {}", vec.len());
                        println!("First 10 values: {:?}", &vec[..std::cmp::min(10, vec.len())]);
                        // Compute L2 norm
                        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                        println!("L2 norm: {:.6}", norm);
                    }
                    Err(err) => {
                        eprintln!("Error generating embedding: {}", err);
                    }
                }
            }
            Err(err) => {
                eprintln!("Error creating embedding service: {}", err);
            }
        }
    });
}

/// Generate embeddings for a batch of texts.
pub fn cmd_embed_batch(texts: &str, model: Option<&str>, service_type: Option<&str>, api_key: Option<&str>) {
    let text_list: Vec<String> = texts.split(',').map(|s| s.trim().to_string()).collect();
    if text_list.is_empty() {
        eprintln!("No texts provided. Use comma-separated text values.");
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        let embedding_service = create_embedding_service(model, service_type, api_key);
        match embedding_service {
            Ok(service) => {
                let embeddings = service.embed_batch(&text_list).await;
                match embeddings {
                    Ok(vecs) => {
                        println!("Batch embeddings generated successfully.");
                        println!("Model: {}", service.model().as_str());
                        println!("Count: {} embeddings", vecs.len());
                        for (i, vec) in vecs.iter().enumerate() {
                            println!("  [{}] dim={} first5={:?} norm={:.4}",
                                i, vec.len(),
                                &vec[..std::cmp::min(5, vec.len())],
                                vec.iter().map(|x| x * x).sum::<f32>().sqrt()
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!("Error generating batch embeddings: {}", err);
                    }
                }
            }
            Err(err) => {
                eprintln!("Error creating embedding service: {}", err);
            }
        }
    });
}

/// List available embedding models + the auto-detection chain.
pub fn cmd_embed_list() {
    println!("Available embedding providers (--provider):");
    println!();
    println!("  auto          — zero-config auto-detect (the default; see chain below)");
    println!("  openai        — text-embedding-3-small (1536-dim) / 3-large (3072-dim); OPENAI_API_KEY");
    println!("  voyage        — voyage-3 (1024-dim) / voyage-3-lite (512-dim); VOYAGE_API_KEY");
    println!("  ollama        — nomic-embed-text default; local, no key; probes localhost:11434");
    println!("  fastembed     — local ONNX, no key; auto-chain last resort (one-time ~22MB download, then offline)");
    println!("  openai-compat — OpenAI-compatible relay; needs ONEAI_EMBEDDING_API_KEY + base_url");
    println!();
    println!("  Embedding keys are independent of the LLM provider key (LLM has no embed method).");
    println!();
    println!("  Auto-detection chain order:");
    println!("    1. openai-compat  (ONEAI_EMBEDDING_API_KEY + ONEAI_EMBEDDING_BASE_URL)");
    println!("    2. voyage         (VOYAGE_API_KEY)");
    println!("    3. openai         (OPENAI_API_KEY, official api.openai.com)");
    println!("    4. ollama         (localhost:11434 reachable + embedding model installed)");
    println!("    5. fastembed      (local ONNX, no key; one-time download then offline)");
    println!("    6. (none)         → memory recall falls back to keyword matching");
    println!();
    println!("  Models are free-form strings (--model text-embedding-3-small);");
    println!("  unknown names are runtime-dimension-probed.");
}

/// Check embedding service health.
pub fn cmd_embed_health(model: Option<&str>, service_type: Option<&str>, api_key: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        let embedding_service = create_embedding_service(model, service_type, api_key);
        match embedding_service {
            Ok(service) => {
                let result = service.health_check().await;
                match result {
                    Ok(()) => {
                        println!("✅ Embedding service is healthy.");
                        println!("  Model: {}", service.model().as_str());
                        println!("  Dimension: {}", service.dimension());
                    }
                    Err(err) => {
                        println!("❌ Embedding service is unhealthy: {}", err);
                    }
                }
            }
            Err(err) => {
                eprintln!("Error creating embedding service: {}", err);
            }
        }
    });
}

/// Show embedding dimension for a model.
pub fn cmd_embed_dimension(model: Option<&str>, service_type: Option<&str>, api_key: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        let embedding_service = create_embedding_service(model, service_type, api_key);
        match embedding_service {
            Ok(service) => {
                let dim = service.actual_dimension().await;
                match dim {
                    Ok(d) => {
                        println!("Embedding dimension: {}", d);
                        println!("Model: {}", service.model().as_str());
                    }
                    Err(err) => {
                        // For models with known dimension, just report it
                        let known_dim = service.model().dimension();
                        if known_dim > 0 {
                            println!("Embedding dimension: {}", known_dim);
                            println!("Model: {}", service.model().as_str());
                            eprintln!("Note: Could not verify via test embedding: {}", err);
                        } else {
                            eprintln!("Error determining dimension: {}", err);
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("Error creating embedding service: {}", err);
            }
        }
    });
}

/// Create an embedding service from CLI arguments.
fn create_embedding_service(
    model: Option<&str>,
    service_type: Option<&str>,
    api_key: Option<&str>,
) -> oneai_core::error::Result<std::sync::Arc<dyn oneai_core::traits::EmbeddingService>> {
    use oneai_core::traits::EmbeddingProvider;

    let provider = match service_type.unwrap_or("auto") {
        "auto" | "" => EmbeddingProvider::Auto,
        "openai" => EmbeddingProvider::OpenAi,
        "voyage" => EmbeddingProvider::Voyage,
        "ollama" => EmbeddingProvider::Ollama,
        "fastembed" => EmbeddingProvider::FastEmbed,
        "openai-compat" | "openai_compat" | "openai-compatible" => EmbeddingProvider::OpenAiCompat,
        other => return Err(oneai_core::error::OneAIError::Config(format!(
            "Unknown embedding provider: '{}'. Use: auto, openai, voyage, ollama, fastembed, openai-compat", other
        ))),
    };

    let mut config = EmbeddingConfig::default();
    config.provider = provider;
    if let Some(m) = model {
        config.model = Some(EmbeddingModel::new(m));
    }
    if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        config.api_key = Some(k.to_string());
    }

    match config.build_service() {
        Ok(Some(service)) => Ok(service),
        Ok(None) => Err(oneai_core::error::OneAIError::Config(
            "No embedding provider available — set an embedding key (VOYAGE_API_KEY/OPENAI_API_KEY) or run a local Ollama. Otherwise memory recall uses keyword matching.".to_string(),
        )),
        Err(e) => Err(e),
    }
}
