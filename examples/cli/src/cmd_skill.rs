//! Skill inspection commands — list and show skills discovered from the
//! convention directories (.claude/skills/, .agents/skills/, .opencode/skills/,
//! .oneai/skills/ — project walked up to git root + global under home).
//!
//! There is no install command: skills are plain files dropped into those dirs,
//! mirroring OpenCode/Claude Code. The built-in `skill-creator` is always present.

use oneai_skill::{discover_skills, find_skill};

/// `oneai skill list` — list all discovered skills (convention dirs + builtins
/// that the session would load; this CLI lists the discovered-file skills).
pub fn cmd_skill_list() {
    println!("🧩 Discovered Skills (convention directories)\n");
    let skills = discover_skills();
    if skills.is_empty() {
        println!("  (none found in .claude/skills · .agents/skills · .opencode/skills · .oneai/skills)");
        println!("\n  Drop a skill as <name>/SKILL.md into any of those dirs (project or ~/).");
        println!("  The built-in skill-creator is always available inside a session.");
        return;
    }
    for s in &skills {
        println!("  ✅ {} — {}", s.name, s.description);
    }
    println!("\nUse: oneai skill show <name> for details");
}

/// `oneai skill show <name>` — show a discovered skill's details.
pub fn cmd_skill_show(name: &str) {
    match find_skill(name) {
        Some(s) => {
            println!("🧩 Skill: {}\n", s.name);
            println!("  Description: {}", s.description);
            println!("\nPrompt template:\n{}\n", s.prompt_template);
            println!("Trigger keywords: [{}]", s.trigger_keywords.join(", "));
        }
        None => {
            eprintln!("Skill '{}' not found in convention dirs. Use `oneai skill list` to see discovered skills.", name);
            eprintln!("  (The built-in skill-creator is always available inside a `oneai chat`/`run` session.)");
            std::process::exit(1);
        }
    }
}
