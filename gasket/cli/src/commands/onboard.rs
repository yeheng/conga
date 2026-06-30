//! Onboard 命令实现

use anyhow::Result;

use gasket_engine::config::ConfigLoader;

/// Initialize gasket configuration
pub async fn cmd_onboard() -> Result<()> {
    println!("🐈 Initializing gasket...\n");

    let loader = ConfigLoader::new();
    let config_path = loader.config_path();
    let workspace = gasket_engine::config::config_dir();

    if loader.exists() {
        println!("Configuration already exists at: {:?}", config_path);
        println!("Edit it manually to add your API keys.");
    } else {
        // Create default config
        let _config = loader.init_default().await?;
        println!("Created configuration at: {:?}", config_path);
        println!("\nEdit the config to add your API key:");
        println!("  providers:");
        println!("    openrouter:");
        println!("      apiKey: sk-or-v1-xxx");
    }

    println!("\n🐈 Initialization complete!");
    println!("Workspace: {:?}", workspace);

    Ok(())
}
