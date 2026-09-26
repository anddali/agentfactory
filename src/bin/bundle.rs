use anyhow::{bail, ensure, Context, Result};
use factories::{config::Platform, releases::Bundle};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct Lock {
    format: u32,
    workflow: String,
    base_generation: i64,
    digest: String,
}
fn safe_name(name: &str) -> bool {
    name.split('@').all(factories::workflow::identifier)
        && !matches!(
            name.split('@')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ensure!(args.len() >= 4, "Usage: factory-bundle validate PLATFORM BUNDLE | unpack PLATFORM BUNDLE NEW_DIRECTORY | pack PLATFORM WORKFLOW DIRECTORY OUTPUT [--locked]");
    let platform = Platform::load(Path::new(&args[2]))?;
    match args[1].as_str() {
        "validate" => {
            let b: Bundle = serde_json::from_slice(&std::fs::read(&args[3])?)?;
            b.resolve(&platform)?;
            println!(
                "Valid bundle {}: {} (no model or tools executed)",
                b.workflow,
                b.digest()?
            );
        }
        "unpack" => {
            ensure!(args.len() == 5, "unpack requires a new output directory");
            let b: Bundle = serde_json::from_slice(&std::fs::read(&args[3])?)?;
            b.resolve(&platform)?;
            ensure!(
                b.workflows
                    .keys()
                    .chain(b.prompts.keys())
                    .all(|n| safe_name(n)),
                "unsafe filename"
            );
            let root = Path::new(&args[4]);
            ensure!(
                !root.exists(),
                "output directory already exists; unpack into a new directory"
            );
            std::fs::create_dir(root)?;
            for (folder, extension, items) in [
                ("workflows", "yaml", &b.workflows),
                ("prompts", "md", &b.prompts),
            ] {
                std::fs::create_dir(root.join(folder))?;
                for (name, content) in items {
                    std::fs::write(
                        root.join(folder).join(format!("{name}.{extension}")),
                        content,
                    )?;
                }
            }
            let lock = Lock {
                format: 1,
                workflow: b.workflow.clone(),
                base_generation: b.base_generation,
                digest: b.digest()?,
            };
            std::fs::write(root.join("release.lock"), serde_json::to_vec_pretty(&lock)?)?;
            println!("Unpacked {} into {}", b.workflow, root.display());
        }
        "pack" => {
            ensure!(
                args.len() == 6 || (args.len() == 7 && args[6] == "--locked"),
                "pack requires WORKFLOW DIRECTORY OUTPUT [--locked]"
            );
            let root = Path::new(&args[4]);
            let snapshot = platform.snapshot(&root.join("workflows"), &root.join("prompts"))?;
            let mut b = Bundle::from_snapshot(&snapshot, &args[3])?;
            let lock_path = root.join("release.lock");
            let lock: Option<Lock> = if lock_path.exists() {
                Some(serde_json::from_slice(&std::fs::read(lock_path)?)?)
            } else {
                None
            };
            if let Some(lock) = &lock {
                ensure!(
                    lock.format == 1 && lock.workflow == b.workflow,
                    "lock belongs to another workflow"
                );
                b.base_generation = lock.base_generation;
            }
            b.resolve(&platform)?;
            if args.len() == 7 {
                ensure!(
                    lock.as_ref()
                        .context("--locked requires release.lock")?
                        .digest
                        == b.digest()?,
                    "files changed since export; locked validation failed"
                );
            }
            std::fs::write(&args[5], serde_json::to_vec_pretty(&b)?)?;
            println!("Packed {}: {}", b.workflow, b.digest()?);
        }
        _ => bail!("unknown command"),
    }
    Ok(())
}
