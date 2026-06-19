//! CLI commands for session management — list, resume, delete, info.
//!
//! These commands operate on the SQLite session store to manage
//! saved conversations and enable session resume.

use oneai_persistence::SqliteSessionStore;
use oneai_core::traits::MemoryPersistence;

/// List all saved sessions.
pub fn cmd_session_list() {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let sessions = rt.block_on(async {
        store.list_conversations().await
    });

    match sessions {
        Ok(sessions) => {
            if sessions.is_empty() {
                println!("No saved sessions found.");
                println!("Sessions are created automatically when SQLite persistence is enabled.");
                println!("Enable with: oneai chat --persist");
                return;
            }
            let total = sessions.len();
            println!("Saved sessions:");
            println!("{:<40} {:<20} {:<8} {}", "ID", "Updated", "Msgs", "Created");
            println!("{}", "-".repeat(90));
            for session in &sessions {
                println!(
                    "{:<40} {:<20} {:<8} {}",
                    session.id,
                    session.updated_at.format("%Y-%m-%d %H:%M"),
                    session.message_count,
                    session.created_at.format("%Y-%m-%d %H:%M"),
                );
            }
            println!();
            println!("Total: {} sessions", total);
        }
        Err(e) => {
            eprintln!("Error listing sessions: {}", e);
        }
    }
}

/// Resume a saved session (interactive mode with prior conversation history).
pub fn cmd_session_resume(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let conversation = rt.block_on(async {
        store.load_conversation(session_id).await
    });

    match conversation {
        Ok(Some(conv)) => {
            println!("Resuming session: {}", session_id);
            println!("Conversation has {} messages.", conv.messages.len());

            // Show last few messages as context
            let recent = conv.messages.iter().rev().take(5).collect::<Vec<_>>();
            if !recent.is_empty() {
                println!("\nRecent messages:");
                for msg in recent.iter().rev() {
                    let role = match msg.role {
                        oneai_core::Role::User => "User",
                        oneai_core::Role::Assistant => "Assistant",
                        oneai_core::Role::System => "System",
                        _ => "Other",
                    };
                    let text = msg.text_content();
                    let preview = if text.len() > 100 {
                        format!("{}...", &text[..100])
                    } else {
                        text
                    };
                    println!("  [{}] {}", role, preview);
                }
            }

            println!("\nNote: Full session resume with agent loop requires the interactive CLI.");
            println!("Use: oneai chat --resume {}", session_id);
        }
        Ok(None) => {
            eprintln!("Session '{}' not found.", session_id);
            eprintln!("Use 'oneai session list' to see available sessions.");
        }
        Err(e) => {
            eprintln!("Error loading session '{}': {}", session_id, e);
        }
    }
}

/// Delete a saved session and its associated STM entries.
pub fn cmd_session_delete(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let result = rt.block_on(async {
        store.delete_conversation(session_id).await
    });

    match result {
        Ok(()) => {
            println!("Session '{}' deleted successfully.", session_id);
        }
        Err(e) => {
            eprintln!("Error deleting session '{}': {}", session_id, e);
        }
    }
}

/// Show detailed info about a saved session.
pub fn cmd_session_info(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    // Load conversation
    let conversation = rt.block_on(async {
        store.load_conversation(session_id).await
    });

    match conversation {
        Ok(Some(conv)) => {
            println!("Session: {}", session_id);
            println!("Messages: {}", conv.messages.len());

            // Load STM entries
            let stm_entries = rt.block_on(async {
                store.load_stm(session_id).await
            });

            match stm_entries {
                Ok(entries) => {
                    println!("STM entries: {}", entries.len());
                }
                Err(e) => {
                    println!("STM entries: (error: {})", e);
                }
            }

            // Show all messages
            println!("\nConversation history:");
            for (i, msg) in conv.messages.iter().enumerate() {
                let role = match msg.role {
                    oneai_core::Role::User => "User",
                    oneai_core::Role::Assistant => "Assistant",
                    oneai_core::Role::System => "System",
                    oneai_core::Role::Tool => "Tool",
                    _ => "Other",
                };
                let text = msg.text_content();
                let preview = if text.len() > 200 {
                    format!("{}...", &text[..200])
                } else {
                    text.clone()
                };
                println!("  {}. [{}] {}", i + 1, role, preview);
            }
        }
        Ok(None) => {
            eprintln!("Session '{}' not found.", session_id);
        }
        Err(e) => {
            eprintln!("Error loading session '{}': {}", session_id, e);
        }
    }
}
