use anyhow::Result;
use crate::core::config::{Config, Preset};
use crate::core::executor::{run_dotnet_test, RunOptions};

pub fn list() -> Result<()> {
    let config = Config::new()?;
    let presets = config.load_presets()?;
    
    if presets.is_empty() {
        println!("No presets found. Add one with `dotest preset add <name> <filter>`");
        return Ok(());
    }

    println!("{:<20} | {:<40} | {}", "NAME", "FILTER", "NO-BUILD");
    println!("{:-<20}-|-{:-<40}-|-{:-<8}", "", "", "");
    for p in presets {
        println!("{:<20} | {:<40} | {}", p.name, p.filter, p.no_build);
    }

    Ok(())
}

pub fn run(name: &str) -> Result<()> {
    let config = Config::new()?;
    let preset = config.get_preset(name)?;

    match preset {
        Some(p) => {
            println!("Running preset: {}", p.name);
            let options = RunOptions {
                filter: Some(p.filter),
                no_build: p.no_build,
            };
            run_dotnet_test(options)
        }
        None => {
            anyhow::bail!("Preset '{}' not found.", name);
        }
    }
}

pub fn add(name: &str, filter: &str, build: bool) -> Result<()> {
    let config = Config::new()?;
    let preset = Preset {
        name: name.to_string(),
        filter: filter.to_string(),
        no_build: !build,
    };
    
    config.add_preset(preset)?;
    println!("Preset '{}' added successfully.", name);
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let config = Config::new()?;
    let removed = config.remove_preset(name)?;
    
    if removed {
        println!("Preset '{}' removed successfully.", name);
    } else {
        println!("Preset '{}' not found.", name);
    }
    Ok(())
}
