//! OneAI embed subcommand — embedding generation and service management.
//!
//! Subcommands:
//!   oneai embed generate <text> — Generate an embedding for the given text
//!   oneai embed batch <text1,text2,...> — Generate embeddings for multiple texts
//!   oneai embed list              — List available embedding models
//!   oneai embed health            — Check embedding service health
//!   oneai embed dimension         — Show embedding dimension for configured model

use oneai_core::{EmbeddingModel, EmbeddingServiceType, EmbeddingConfig};
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
                        println!("Model: {}", service.model().model_name());
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
                        println!("Model: {}", service.model().model_name());
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

/// List available embedding models.
pub fn cmd_embed_list() {
    println!("Available embedding models:");
    println!();
    println!("  Local models (no API key needed):");
    println!("    AllMiniLML6V2    — 384-dim, ~22MB, fast, good Chinese (FastEmbed default)");
    println!("    BGEBaseENv15     — 768-dim, ~430MB, better English");
    println!("    MxbaiEmbedLargeV1 — 1024-dim, ~670MB, high quality");
    println!("    Ollama            — dim varies, local server required");
    println!();
    println!("  Cloud models (API key required):");
    println!("    OpenAISmall       — 1536-dim, text-embedding-3-small ($0.02/1M tokens)");
    println!("    OpenAILarge       — 3072-dim, text-embedding-3-large ($0.13/1M tokens)");
    println!("    Voyage3           — 1024-dim, Anthropic/Voyage ($0.02/1M tokens)");
    println!("    Voyage3Lite       — 512-dim, Anthropic/Voyage ($0.01/1M tokens)");
    println!();
    println!("  Service types: fastembed, ollama, openai, anthropic");
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
                        println!("  Model: {}", service.model().model_name());
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
                        println!("Model: {}", service.model().model_name());
                    }
                    Err(err) => {
                        // For models with known dimension, just report it
                        let known_dim = service.model().dimension();
                        if known_dim > 0 {
                            println!("Embedding dimension: {}", known_dim);
                            println!("Model: {}", service.model().model_name());
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
    let st = service_type.unwrap_or("fastembed");
    let service_type = match st {
        "fastembed" => EmbeddingServiceType::FastEmbed,
        "ollama" => EmbeddingServiceType::Ollama,
        "openai" => EmbeddingServiceType::OpenAI,
        "anthropic" => EmbeddingServiceType::Anthropic,
        _ => return Err(oneai_core::error::OneAIError::Config(format!(
            "Unknown embedding service type: '{}'. Use: fastembed, ollama, openai, anthropic", st
        ))),
    };

    let embedding_model = match model {
        Some("all-MiniLM-L6-v2") | Some("allminilm") | None => EmbeddingModel::AllMiniLML6V2,
        Some("bge-base-en-v1.5") | Some("bge") => EmbeddingModel::BGEBaseENv15,
        Some("mxbai-embed-large") | Some("mxbai") => EmbeddingModel::MxbaiEmbedLargeV1,
        Some("text-embedding-3-small") | Some("openai-small") => EmbeddingModel::OpenAISmall,
        Some("text-embedding-3-large") | Some("openai-large") => EmbeddingModel::OpenAILarge,
        Some("voyage-3") | Some("voyage3") => EmbeddingModel::Voyage3,
        Some("voyage-3-lite") | Some("voyage3-lite") => EmbeddingModel::Voyage3Lite,
        Some("ollama") => EmbeddingModel::Ollama,
        Some(m) => return Err(oneai_core::error::OneAIError::Config(format!(
            "Unknown embedding model: '{}'", m
        ))),
    };

    let config = EmbeddingConfig::from_parts(service_type, embedding_model, api_key.map(|s| s.to_string()));

    config.build_service()
}
