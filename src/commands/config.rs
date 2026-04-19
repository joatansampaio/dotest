use anyhow::Result;
use crate::core::config::Config;

pub fn execute_exclude_add(category: &str) -> Result<()> {
    let config = Config::new()?;
    config.add_excluded_category(category)?;
    println!("Category '{}' added to exclusions.", category);
    Ok(())
}

pub fn execute_exclude_remove(category: &str) -> Result<()> {
    let config = Config::new()?;
    if config.remove_excluded_category(category)? {
        println!("Category '{}' removed from exclusions.", category);
    } else {
        println!("Category '{}' was not in exclusions.", category);
    }
    Ok(())
}

pub fn execute_exclude_list() -> Result<()> {
    let config = Config::new()?;
    let settings = config.load_settings()?;
    
    if settings.excluded_categories.is_empty() {
        println!("No excluded categories.");
    } else {
        println!("Excluded Categories:");
        for cat in settings.excluded_categories {
            println!("  - {}", cat);
        }
    }
    Ok(())
}
