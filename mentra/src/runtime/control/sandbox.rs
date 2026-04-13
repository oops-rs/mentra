//! Container and sandbox environment detection.
//!
//! Detects whether the runtime is executing inside a container (Docker,
//! Podman, etc.) or other restricted environment, which informs policy
//! decisions about file access and shell execution.

use std::path::Path;

/// Describes the detected execution environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEnvironment {
    /// Running directly on the host OS.
    Host,
    /// Running inside a Docker container.
    Docker,
    /// Running inside a generic container (cgroup signals).
    Container,
    /// Running inside a CI environment.
    ContinuousIntegration,
}

impl std::fmt::Display for ExecutionEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host => write!(f, "host"),
            Self::Docker => write!(f, "docker"),
            Self::Container => write!(f, "container"),
            Self::ContinuousIntegration => write!(f, "ci"),
        }
    }
}

/// Detect the current execution environment.
///
/// Uses multiple heuristics to determine if we're in a container or CI:
/// - `/.dockerenv` file presence → Docker
/// - `/run/.containerenv` file presence → Podman/container
/// - `container=` in `/proc/1/environ` → generic container
/// - CI-related environment variables → CI
pub fn detect_environment() -> ExecutionEnvironment {
    // Check for CI environment variables first.
    if is_ci_environment() {
        return ExecutionEnvironment::ContinuousIntegration;
    }

    // Docker detection.
    if Path::new("/.dockerenv").exists() {
        return ExecutionEnvironment::Docker;
    }

    // Podman / generic container detection.
    if Path::new("/run/.containerenv").exists() {
        return ExecutionEnvironment::Container;
    }

    // Check cgroup for container signals (Linux only).
    #[cfg(target_os = "linux")]
    if is_in_container_cgroup() {
        return ExecutionEnvironment::Container;
    }

    ExecutionEnvironment::Host
}

/// Returns `true` if common CI environment variables are set.
fn is_ci_environment() -> bool {
    // Standard CI indicators.
    std::env::var("CI").is_ok()
        || std::env::var("GITHUB_ACTIONS").is_ok()
        || std::env::var("GITLAB_CI").is_ok()
        || std::env::var("JENKINS_HOME").is_ok()
        || std::env::var("CIRCLECI").is_ok()
        || std::env::var("BUILDKITE").is_ok()
        || std::env::var("TRAVIS").is_ok()
}

/// Check `/proc/1/cgroup` for container indicators (Linux only).
#[cfg(target_os = "linux")]
fn is_in_container_cgroup() -> bool {
    let Ok(cgroup) = std::fs::read_to_string("/proc/1/cgroup") else {
        return false;
    };
    cgroup.contains("docker")
        || cgroup.contains("lxc")
        || cgroup.contains("containerd")
        || cgroup.contains("kubepods")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_environment_returns_valid_variant() {
        let env = detect_environment();
        // We can't control the test environment, but the function should
        // always return a valid variant without panicking.
        let display = env.to_string();
        assert!(
            ["host", "docker", "container", "ci"].contains(&display.as_str()),
            "unexpected environment: {display}"
        );
    }

    #[test]
    fn display_formats_correctly() {
        assert_eq!(ExecutionEnvironment::Host.to_string(), "host");
        assert_eq!(ExecutionEnvironment::Docker.to_string(), "docker");
        assert_eq!(ExecutionEnvironment::Container.to_string(), "container");
        assert_eq!(
            ExecutionEnvironment::ContinuousIntegration.to_string(),
            "ci"
        );
    }
}
