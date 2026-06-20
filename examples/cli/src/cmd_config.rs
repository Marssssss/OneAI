//! Config management commands.
//!
//! Subcommands for viewing and initializing the OneAI configuration file.

use crate::config::OneaiConfig;

/// Show the current configuration.
///
/// Reads from ~/.oneai/config.toml and displays the effective configuration,
/// including any overrides from environment variables.
pub fn cmd_config_show() {
    let config = OneaiConfig::load_or_default();
    let path = OneaiConfig::default_path();

    println!("📁 Config file: {}", path.display());
    if path.exists() {
        println!("   (loaded from file)");
    } else {
        println!("   (no config file found — using defaults)");
    }
    println!();

    // Provider config
    println!("🔧 Provider:");
    println!("   model: {}", config.provider.model);
    if config.provider.api_key.is_some() {
        println!("   api_key: {}***{}",
            &config.provider.api_key.as_ref().unwrap()[..3.min(config.provider.api_key.as_ref().unwrap().len())],
            if config.provider.api_key.as_ref().unwrap().len() > 3 { "" } else { "" }
        );
    } else {
        println!("   api_key: (not set)");
    }
    if config.provider.base_url.is_some() {
        println!("   base_url: {}", config.provider.base_url.as_ref().unwrap());
    } else {
        println!("   base_url: (default — OpenAI)");
    }

    // Environment variable overrides
    let env_api_key = std::env::var("ONEAI_API_KEY").ok();
    let env_base_url = std::env::var("ONEAI_BASE_URL").ok();
    let env_model = std::env::var("ONEAI_MODEL").ok();

    if env_api_key.is_some() || env_base_url.is_some() || env_model.is_some() {
        println!();
        println!("🌍 Environment overrides:");
        if env_api_key.is_some() {
            println!("   ONEAI_API_KEY: set");
        }
        if env_base_url.is_some() {
            println!("   ONEAI_BASE_URL: {}", env_base_url.unwrap());
        }
        if env_model.is_some() {
            println!("   ONEAI_MODEL: {}", env_model.unwrap());
        }
    }

    println!();
    println!("📦 Domain:");
    println!("   default_pack: {}", config.domain.default_pack);

    println!();
    println!("🎨 UI:");
    println!("   theme: {}", config.ui.theme);

    // Effective model config
    let effective = config.to_model_config();
    println!();
    if effective.is_some() {
        println!("✅ Effective provider: configured (ready to use)");
    } else {
        println!("⚠️  Effective provider: NOT configured");
        println!("   Set ONEAI_API_KEY or run: oneai config init");
    }
}

/// Initialize the configuration file.
///
/// Creates ~/.oneai/config.toml with default values.
/// If a config file already exists, asks the user before overwriting.
pub fn cmd_config_init() {
    let path = OneaiConfig::default_path();

    if path.exists() {
        println!("Config file already exists: {}", path.display());
        println!("To view: oneai config show");
        println!("To reset: delete the file first, then run 'oneai config init' again.");
        println!();
        println!("  rm {}", path.display());
        return;
    }

    // Create directory
    let dir = path.parent().unwrap();
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("Error creating directory {}: {}", dir.display(), e);
        return;
    }

    // Create default config
    let _config = OneaiConfig::default();

    // Create default config — don't auto-fill sensitive values from env
    // (env vars will still be used as overrides when reading config)
    let config = OneaiConfig::default();

    match config.save() {
        Ok(saved_path) => {
            println!("✅ Config file created: {}", saved_path.display());
            println!();
            println!("Edit it to configure your provider:");
            println!("  {}", saved_path.display());
            println!();
            println!("Example configuration:");
            println!("  [provider]");
            println!("  api_key = \"sk-your-api-key\"");
            println!("  base_url = \"https://api.openai.com/v1\"");
            println!("  model = \"gpt-4\"");
            println!();
            println!("  [domain]");
            println!("  default_pack = \"coding\"");
            println!();
            println!("  [ui]");
            println!("  theme = \"dark\"");
        }
        Err(e) => {
            eprintln!("Error saving config: {}", e);
        }
    }
}
