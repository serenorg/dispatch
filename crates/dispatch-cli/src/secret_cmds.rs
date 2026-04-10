use anyhow::Result;
use std::{io::Read as _, path::PathBuf};

const MAX_SECRET_STDIN_BYTES: usize = 1024 * 1024;

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
    read_secret_value(std::io::stdin())
}

fn read_secret_value(reader: impl std::io::Read) -> Result<String> {
    let mut value = String::new();
    let limit = (MAX_SECRET_STDIN_BYTES as u64) + 1;
    reader.take(limit).read_to_string(&mut value)?;
    if value.len() > MAX_SECRET_STDIN_BYTES {
        anyhow::bail!(
            "stdin secret value exceeds maximum size of {} bytes",
            MAX_SECRET_STDIN_BYTES
        );
    }
    while matches!(value.chars().last(), Some('\n' | '\r')) {
        value.pop();
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::read_secret_value;
    use std::io::Cursor;

    #[test]
    fn read_secret_value_trims_trailing_newlines() {
        let value = read_secret_value(Cursor::new("secret-value\r\n")).expect("value");
        assert_eq!(value, "secret-value");
    }

    #[test]
    fn read_secret_value_rejects_oversized_input() {
        let oversized = "x".repeat(super::MAX_SECRET_STDIN_BYTES + 1);
        let error = read_secret_value(Cursor::new(oversized)).expect_err("oversized stdin");
        assert!(
            error
                .to_string()
                .contains("stdin secret value exceeds maximum size"),
            "unexpected error: {error}"
        );
    }
}
