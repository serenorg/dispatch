use anyhow::Result;
use std::path::PathBuf;

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

pub(crate) fn set(path: PathBuf, name: &str, value: &str) -> Result<()> {
    let paths = dispatch_core::set_secret(&path, name, value)?;
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
