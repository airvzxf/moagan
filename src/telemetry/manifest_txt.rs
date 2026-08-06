//! D.33.4: MANIFEST.txt human-readable writer.

use crate::error::Result;
use std::path::Path;

pub fn write_manifest_txt(run_dir: &Path, body: &str) -> Result<()> {
    let dir = run_dir.join("telemetry");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("MANIFEST.txt"), body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_txt_writes_to_telemetry_dir() {
        let tmp = std::env::temp_dir().join(format!("moagan-mftxt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let result = write_manifest_txt(&tmp, "test body");
        assert!(result.is_ok());
        let written = std::fs::read_to_string(tmp.join("telemetry").join("MANIFEST.txt")).unwrap();
        assert_eq!(written, "test body");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
