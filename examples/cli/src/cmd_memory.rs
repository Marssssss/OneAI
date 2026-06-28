//! CLI commands for long-term memory management — search/list durable facts.
//!
//! Operates on the SQLite `memories` table (the unified fact store backing the
//! core/archival tiers). These commands let the user inspect what the agent has
//! remembered across sessions — the "越用越好用" transparency surface.

use oneai_core::keyword_matches;
use oneai_core::traits::MemoryPersistence;
use oneai_persistence::SqliteSessionStore;

/// Search durable facts by keyword.
pub fn cmd_memory_search(query: &str, user: &str, top_k: usize) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let facts = rt.block_on(async {
        // Empty session scope → all of the user's facts (cross-session habits).
        store.load_facts(user, "").await
    });

    match facts {
        Ok(all) => {
            let mut hits: Vec<_> = all.into_iter()
                .filter(|f| {
                    keyword_matches(&f.content, query)
                        || keyword_matches(&f.subject, query)
                        || keyword_matches(&f.predicate, query)
                })
                .collect();
            // Most-recently-updated first.
            hits.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            hits.truncate(top_k);

            if hits.is_empty() {
                println!("No memories matching '{}' for user '{}'.", query, user);
                return;
            }
            println!("Memories matching '{}' (user: {}):", query, user);
            println!("{}", "-".repeat(80));
            for f in &hits {
                println!("- [{}] {} {}: {}", f.fact_type, f.subject, f.predicate, f.content);
                println!("    session: {} | v{} | updated: {}",
                    f.session_id, f.version, f.updated_at.to_rfc3339());
            }
            println!("\n{} fact(s).", hits.len());
        }
        Err(e) => {
            eprintln!("Error searching memories: {}", e);
            std::process::exit(1);
        }
    }
}

/// List durable facts for a user (optionally scoped to a session).
pub fn cmd_memory_list(user: &str, session: Option<&str>) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let facts = rt.block_on(async {
        store.load_facts(user, session.unwrap_or("")).await
    });

    match facts {
        Ok(facts) => {
            if facts.is_empty() {
                println!("No memories stored for user '{}'.", user);
                println!("Memories accumulate as the agent extracts facts (on compression)");
                println!("or the agent archives them via memory tools.");
                return;
            }
            let scope = session.map(|s| format!("session {}", s)).unwrap_or_else(|| "all sessions".to_string());
            println!("Memories for user '{}' ({}):", user, scope);
            println!("{}", "-".repeat(80));
            for f in &facts {
                println!("- [{}] {} {}: {}", f.fact_type, f.subject, f.predicate, f.content);
                println!("    session: {} | v{} | updated: {}",
                    f.session_id, f.version, f.updated_at.to_rfc3339());
            }
            println!("\n{} fact(s).", facts.len());
        }
        Err(e) => {
            eprintln!("Error listing memories: {}", e);
            std::process::exit(1);
        }
    }
}
