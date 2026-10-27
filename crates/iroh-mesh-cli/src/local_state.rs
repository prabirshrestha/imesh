use anyhow::{Context, Result};
use std::{fs::File, path::Path};

pub fn lock(path: &Path) -> Result<File> {
    iroh_mesh::os::create_private_dir(path.parent().context("lock has no directory")?)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.lock().with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}
