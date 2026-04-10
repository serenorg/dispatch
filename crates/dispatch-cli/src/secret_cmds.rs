use anyhow::Result;
use std::{io::Read as _, path::PathBuf};

pub(crate) fn init(path: PathBuf, force: bool, json: bool) -> Result<()> {
    let paths = dispatch_core::init_secret_store(&path, force)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dispatch_root": paths.dispatch_root,
                "secrets_dir": paths.secrets_dir,
                "store_path": paths.store_path,
                "key_path": paths.key_path,
            }))?
        );
    } else {
        println!(
            "Initialized Dispatch secret store {}",
            paths.secrets_dir.display()
        );
    }
    Ok(())
}

pub(crate) fn set(path: PathBuf, name: &str, value: Option<&str>, value_stdin: bool) -> Result<()> {
    let value = if let Some(value) = value {
        value.to_string()
    } else if value_stdin {
        read_secret_value_from_stdin()?
    } else {
        unreachable!("clap enforces that either --value or --value-stdin is provided");
    };
    let paths = dispatch_core::set_secret(&path, name, &value)?;
    println!("Stored secret `{name}` in {}", paths.secrets_dir.display());
    Ok(())
}

pub(crate) fn rm(path: PathBuf, name: &str) -> Result<()> {
    let (_, removed) = dispatch_core::remove_secret(&path, name)?;
    if removed {
        println!("Removed secret `{name}`");
    } else {
        println!("Secret `{name}` was not present");
    }
    Ok(())
}

pub(crate) fn ls(path: PathBuf, json: bool) -> Result<()> {
    let names = dispatch_core::list_secret_names(&path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&names)?);
    } else {
        for name in names {
            println!("{name}");
        }
    }
    Ok(())
}

fn read_secret_value_from_stdin() -> Result<String> {
    let mut value = String::new();
    std::io::stdin().read_to_string(&mut value)?;
    while matches!(value.chars().last(), Some('\n' | '\r')) {
        value.pop();
    }
    Ok(value)
}
