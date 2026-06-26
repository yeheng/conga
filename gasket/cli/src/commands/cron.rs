//! Cron commands implementation

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use uuid::Uuid;

use gasket_engine::config::config_dir;
use gasket_engine::cron::{CronJob, CronService};

/// Helper to create a CronService with JSON-file persistence
async fn create_cron_service() -> Result<CronService> {
    let workspace = config_dir();
    let cron_path = config_dir().join("state").join("cron.json");
    Ok(CronService::new(workspace, cron_path).await)
}

/// List all scheduled cron jobs
pub async fn cmd_cron_list() -> Result<()> {
    println!("{}\n", "Scheduled Jobs".bold());

    let service = create_cron_service().await?;
    let jobs = service.list_jobs();

    if jobs.is_empty() {
        println!("No scheduled jobs found.");
        println!("\nUse 'gasket cron add' to create a new job.");
        return Ok(());
    }

    for job in jobs {
        let status = if job.enabled {
            "✓".green()
        } else {
            "✗".red()
        };
        let next = job
            .next_run
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "N/A".to_string());

        println!("{}", job.name.cyan().bold());
        println!("  ID:       {}", job.id.dimmed());
        println!("  Status:   {}", status);
        println!("  Cron:     {}", job.cron);
        println!("  Message:  {}", job.message);
        println!("  Next:     {}", next);
        if let Some(ch) = &job.channel {
            println!("  Channel:  {}", ch);
        }
        if let Some(cid) = &job.chat_id {
            println!("  Chat ID:  {}", cid);
        }
        println!();
    }

    Ok(())
}

/// Add a new cron job
pub async fn cmd_cron_add(name: String, cron_expr: String, message: String) -> Result<()> {
    // Validate cron expression (supports both 5-field and 6-field formats)
    let normalized_cron = normalize_cron_expression(&cron_expr)?;
    let schedule: cron::Schedule = normalized_cron
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", cron_expr, e))?;

    let next_run = schedule.after(&Utc::now()).next();

    let id = Uuid::new_v4().to_string();
    let mut job = CronJob::new(&id, &name, &normalized_cron, &message);
    job.next_run = next_run;

    let service = create_cron_service().await?;
    service
        .add_job(job.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to add cron job '{}': {}", name, e))?;

    println!(
        "{} Scheduled job '{}' with ID: {}",
        "✓".green(),
        name.bold(),
        id.dimmed()
    );

    if let Some(next) = next_run {
        println!("  Next run: {}", next.format("%Y-%m-%d %H:%M UTC"));
    }

    Ok(())
}

/// Normalize cron expression to 6-field format (sec min hour day month weekday)
/// Accepts both 5-field (traditional) and 6-field (with seconds) formats
fn normalize_cron_expression(expr: &str) -> Result<String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();

    if parts.len() == 5 {
        // 5-field format: min hour day month weekday -> prepend "0" for seconds
        Ok(format!("0 {}", expr))
    } else if parts.len() == 6 {
        // Already 6-field format
        Ok(expr.to_string())
    } else {
        anyhow::bail!(
            "Invalid cron expression: expected 5 or 6 fields, got {}. Expression: '{}'",
            parts.len(),
            expr
        )
    }
}

/// Remove a cron job by ID
pub async fn cmd_cron_remove(id: String) -> Result<()> {
    let service = create_cron_service().await?;

    // Try to get job info first for better feedback
    let job = service.get_job(&id);

    let removed = service
        .remove_job(&id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to remove cron job '{}': {}", id, e))?;

    if removed {
        if let Some(job) = job {
            println!(
                "{} Removed job: {} ({})",
                "✓".green(),
                job.name.bold(),
                id.dimmed()
            );
        } else {
            println!("{} Removed job: {}", "✓".green(), id.dimmed());
        }
    } else {
        println!("{} Job not found: {}", "✗".red(), id);
    }

    Ok(())
}

/// Enable a cron job
pub async fn cmd_cron_enable(id: String) -> Result<()> {
    let service = create_cron_service().await?;

    let job = service.get_job(&id).context("Job not found")?;

    if job.enabled {
        println!("Job '{}' is already enabled.", job.name);
        return Ok(());
    }

    // Need to update the job - remove and re-add with enabled=true
    service.remove_job(&id).await?;
    let mut updated_job = job.clone();
    updated_job.enabled = true;
    service.add_job(updated_job.clone()).await?;

    println!(
        "{} Enabled job: {} ({})",
        "✓".green(),
        updated_job.name.bold(),
        id.dimmed()
    );
    Ok(())
}

/// Disable a cron job
pub async fn cmd_cron_disable(id: String) -> Result<()> {
    let service = create_cron_service().await?;

    let job = service.get_job(&id).context("Job not found")?;

    if !job.enabled {
        println!("Job '{}' is already disabled.", job.name);
        return Ok(());
    }

    // Need to update the job - remove and re-add with enabled=false
    service.remove_job(&id).await?;
    let mut updated_job = job.clone();
    updated_job.enabled = false;
    service.add_job(updated_job.clone()).await?;

    println!(
        "{} Disabled job: {} ({})",
        "✓".green(),
        updated_job.name.bold(),
        id.dimmed()
    );
    Ok(())
}

/// Show detailed info for a cron job
pub async fn cmd_cron_show(id: String) -> Result<()> {
    let service = create_cron_service().await?;

    let job = service.get_job(&id).context("Job not found")?;

    let status = if job.enabled {
        "enabled".green()
    } else {
        "disabled".red()
    };
    let next = job
        .next_run
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "N/A".to_string());

    println!("{}", job.name.cyan().bold());
    println!();
    println!("  ID:       {}", job.id);
    println!("  Status:   {}", status);
    println!("  Cron:     {}", job.cron);
    println!("  Message:  {}", job.message);
    println!("  Next:     {}", next);

    if let Some(ch) = &job.channel {
        println!("  Channel:  {}", ch);
    }
    if let Some(cid) = &job.chat_id {
        println!("  Chat ID:  {}", cid);
    }

    // Parse and show human-readable schedule
    if let Ok(schedule) = job.cron.parse::<cron::Schedule>() {
        println!();
        println!("  {}", "Upcoming runs:".dimmed());
        let now = Utc::now();
        for (i, dt) in schedule.after(&now).take(5).enumerate() {
            println!("    {}. {}", i + 1, dt.format("%Y-%m-%d %H:%M %Z"));
        }
    }

    Ok(())
}

/// Refresh all cron jobs from disk
pub async fn cmd_cron_refresh() -> Result<()> {
    let service = create_cron_service().await?;

    let report = service
        .refresh_all_jobs()
        .await
        .context("Failed to refresh cron jobs")?;

    println!("{}", "Cron Jobs Refreshed".bold().cyan());
    println!();
    println!("  Loaded:   {}", report.loaded.to_string().green());
    println!("  Updated:  {}", report.updated.to_string().yellow());
    println!("  Removed:  {}", report.removed.to_string().red());
    println!("  Errors:   {}", report.errors.to_string().red());

    Ok(())
}
