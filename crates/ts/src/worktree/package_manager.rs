//! Package manager detection and install command generation.
//!
//! Detects which package manager a project uses by looking for lockfiles,
//! and generates the appropriate install command.

use std::path::Path;

/// Supported package managers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    /// Yarn Berry (v2+), detected by `.yarnrc.yml` presence.
    Yarn,
    /// Yarn Classic (v1), no `.yarnrc.yml`.
    YarnClassic,
    Pnpm,
}

impl PackageManager {
    /// Detect the package manager by checking for lockfiles in the given directory.
    ///
    /// Priority order (if multiple lockfiles exist):
    /// 1. pnpm-lock.yaml -> pnpm
    /// 2. yarn.lock -> yarn
    /// 3. package-lock.json -> npm
    ///
    /// Returns None if no lockfile is found.
    pub fn detect(dir: &Path) -> Option<Self> {
        // pnpm first -- most specific lockfile name
        if dir.join("pnpm-lock.yaml").exists() {
            return Some(Self::Pnpm);
        }
        if dir.join("yarn.lock").exists() {
            // Distinguish Yarn Berry (v2+) from Classic (v1)
            if dir.join(".yarnrc.yml").exists() {
                return Some(Self::Yarn);
            } else {
                return Some(Self::YarnClassic);
            }
        }
        if dir.join("package-lock.json").exists() {
            return Some(Self::Npm);
        }
        None
    }

    /// Return the install command and arguments for this package manager.
    ///
    /// All commands use frozen lockfile mode to ensure reproducible installs.
    /// For Yarn Berry (v2+), uses `--immutable` instead of `--frozen-lockfile`.
    pub fn install_command(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Npm => ("npm", &["ci"]),
            Self::Yarn => ("yarn", &["install", "--immutable"]),
            Self::YarnClassic => ("yarn", &["install", "--frozen-lockfile"]),
            Self::Pnpm => ("pnpm", &["install", "--frozen-lockfile"]),
        }
    }

    /// Return the lockfile name for this package manager.
    pub fn lockfile_name(&self) -> &'static str {
        match self {
            Self::Npm => "package-lock.json",
            Self::Yarn | Self::YarnClassic => "yarn.lock",
            Self::Pnpm => "pnpm-lock.yaml",
        }
    }

    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Yarn => "yarn (berry)",
            Self::YarnClassic => "yarn (classic)",
            Self::Pnpm => "pnpm",
        }
    }
}

impl std::fmt::Display for PackageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn detect_npm_from_package_lock() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::Npm)
        );
    }

    #[test]
    fn detect_yarn_classic_from_yarn_lock() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::YarnClassic)
        );
    }

    #[test]
    fn detect_yarn_berry_from_yarnrc_yml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        fs::write(dir.path().join(".yarnrc.yml"), "nodeLinker: node-modules").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::Yarn)
        );
    }

    #[test]
    fn detect_pnpm_from_pnpm_lock() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::Pnpm)
        );
    }

    #[test]
    fn detect_none_when_no_lockfile() {
        let dir = TempDir::new().unwrap();

        assert_eq!(PackageManager::detect(dir.path()), None);
    }

    #[test]
    fn pnpm_takes_priority_over_npm_and_yarn() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::Pnpm)
        );
    }

    #[test]
    fn yarn_takes_priority_over_npm() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();

        assert_eq!(
            PackageManager::detect(dir.path()),
            Some(PackageManager::YarnClassic)
        );
    }

    #[test]
    fn install_command_npm() {
        let (cmd, args) = PackageManager::Npm.install_command();
        assert_eq!(cmd, "npm");
        assert_eq!(args, &["ci"]);
    }

    #[test]
    fn install_command_yarn_berry() {
        let (cmd, args) = PackageManager::Yarn.install_command();
        assert_eq!(cmd, "yarn");
        assert_eq!(args, &["install", "--immutable"]);
    }

    #[test]
    fn install_command_yarn_classic() {
        let (cmd, args) = PackageManager::YarnClassic.install_command();
        assert_eq!(cmd, "yarn");
        assert_eq!(args, &["install", "--frozen-lockfile"]);
    }

    #[test]
    fn install_command_pnpm() {
        let (cmd, args) = PackageManager::Pnpm.install_command();
        assert_eq!(cmd, "pnpm");
        assert_eq!(args, &["install", "--frozen-lockfile"]);
    }

    #[test]
    fn lockfile_names() {
        assert_eq!(PackageManager::Npm.lockfile_name(), "package-lock.json");
        assert_eq!(PackageManager::Yarn.lockfile_name(), "yarn.lock");
        assert_eq!(PackageManager::YarnClassic.lockfile_name(), "yarn.lock");
        assert_eq!(PackageManager::Pnpm.lockfile_name(), "pnpm-lock.yaml");
    }

    #[test]
    fn display_names() {
        assert_eq!(format!("{}", PackageManager::Npm), "npm");
        assert_eq!(format!("{}", PackageManager::Yarn), "yarn (berry)");
        assert_eq!(format!("{}", PackageManager::YarnClassic), "yarn (classic)");
        assert_eq!(format!("{}", PackageManager::Pnpm), "pnpm");
    }
}
