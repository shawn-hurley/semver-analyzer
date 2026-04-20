//! Node.js version resolution via nvm.
//!
//! Resolves a version specifier (e.g., "18", "18.17.0", "lts/hydrogen") to
//! a bin directory path that can be prepended to PATH for subprocess calls.

use super::error::WorktreeError;
use std::path::PathBuf;
use std::process::Command;

/// Resolve a Node.js version to its nvm-managed bin directory.
///
/// Runs `nvm which <version>` (sourcing nvm first, since it's a shell
/// function) and returns the parent directory of the resulting node binary.
///
/// Example: `"18"` → `/Users/you/.nvm/versions/node/v18.20.4/bin`
pub fn resolve_node_bin_dir(version: &str) -> Result<PathBuf, WorktreeError> {
    let nvm_dir = std::env::var("NVM_DIR").map_err(|_| {
        WorktreeError::CommandFailed(
            "NVM_DIR not set — install nvm or set NVM_DIR to use --from-node-version / --to-node-version".to_string(),
        )
    })?;

    let script = format!(
        "source \"{nvm_dir}/nvm.sh\" && nvm which {version}",
        nvm_dir = nvm_dir,
        version = version,
    );

    let output = Command::new("bash")
        .args(["-c", &script])
        .output()
        .map_err(|e| {
            WorktreeError::CommandFailed(format!("Failed to run nvm which: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WorktreeError::CommandFailed(format!(
            "nvm cannot find Node.js version '{version}': {stderr}",
        )));
    }

    let node_binary = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let bin_dir = node_binary.parent().ok_or_else(|| {
        WorktreeError::CommandFailed(format!(
            "nvm returned an invalid path: {}",
            node_binary.display()
        ))
    })?;

    Ok(bin_dir.to_path_buf())
}

/// Build environment variables that set a specific Node.js version via PATH.
///
/// When `node_version` is `Some`, resolves the version via nvm and returns
/// a `PATH` entry with the nvm bin directory prepended. The caller passes
/// the result to `Command::envs()`.
///
/// When `node_version` is `None`, returns an empty vec (inherit parent PATH).
pub fn build_node_env(
    node_version: Option<&str>,
) -> Result<Vec<(String, String)>, WorktreeError> {
    let version = match node_version {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    let bin_dir = resolve_node_bin_dir(version)?;
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), current_path);

    tracing::info!(
        node_version = %version,
        bin_dir = %bin_dir.display(),
        "Resolved Node.js version via nvm"
    );

    Ok(vec![("PATH".to_string(), new_path)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_node_env_none_returns_empty() {
        let result = build_node_env(None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn build_node_env_without_nvm_dir_returns_error() {
        // Temporarily unset NVM_DIR if it exists
        let original = std::env::var("NVM_DIR").ok();
        std::env::remove_var("NVM_DIR");

        let result = build_node_env(Some("18"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("NVM_DIR"), "error should mention NVM_DIR: {err}");

        // Restore
        if let Some(val) = original {
            std::env::set_var("NVM_DIR", val);
        }
    }

    /// Detect the currently active nvm Node version.
    ///
    /// Returns None if nvm is not available.
    fn detect_current_nvm_version() -> Option<String> {
        let nvm_dir = std::env::var("NVM_DIR").ok()?;
        let nvm_sh = PathBuf::from(&nvm_dir).join("nvm.sh");
        if !nvm_sh.exists() {
            return None;
        }

        let output = Command::new("bash")
            .args([
                "-c",
                &format!("source \"{nvm_dir}/nvm.sh\" && nvm current"),
            ])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if version.is_empty() || version == "none" || version == "system" {
            return None;
        }
        Some(version)
    }

    #[test]
    #[ignore] // requires nvm — run with: cargo test -p semver-analyzer-ts nvm -- --ignored
    fn resolve_node_bin_dir_returns_valid_path() {
        let version = match detect_current_nvm_version() {
            Some(v) => v,
            None => {
                eprintln!("skipping: nvm not available or no version active");
                return;
            }
        };

        let bin_dir = resolve_node_bin_dir(&version).unwrap();
        assert!(bin_dir.exists(), "bin dir should exist: {}", bin_dir.display());
        assert!(
            bin_dir.join("node").exists(),
            "bin dir should contain node binary: {}",
            bin_dir.display()
        );
    }

    #[test]
    #[ignore] // requires nvm — run with: cargo test -p semver-analyzer-ts nvm -- --ignored
    fn build_node_env_produces_valid_path_entry() {
        let version = match detect_current_nvm_version() {
            Some(v) => v,
            None => {
                eprintln!("skipping: nvm not available or no version active");
                return;
            }
        };

        let env = build_node_env(Some(&version)).unwrap();
        assert_eq!(env.len(), 1, "should return exactly one env var");
        assert_eq!(env[0].0, "PATH", "env var key should be PATH");

        let new_path = &env[0].1;
        let first_component = new_path.split(':').next().unwrap();
        let node_binary = PathBuf::from(first_component).join("node");
        assert!(
            node_binary.exists(),
            "first PATH component should contain node: {}",
            node_binary.display()
        );
    }
}
