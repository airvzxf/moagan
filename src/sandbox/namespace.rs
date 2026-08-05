//! Opt-in namespace isolation for the sandbox via `unshare(CLONE_NEW*)`.
//!
//! Unix-only. Each flag maps to a `CLONE_NEW*` constant:
//! - `CLONE_NEWNS`: mount namespace
//! - `CLONE_NEWPID`: PID namespace
//! - `CLONE_NEWNET`: network namespace
//! - `CLONE_NEWUTS`: hostname/domain namespace
//! - `CLONE_NEWIPC`: IPC namespace

use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Configurable set of Linux namespaces applied to a sandbox child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NamespaceFlags(u32);

impl NamespaceFlags {
    /// Isolate the mount table with `CLONE_NEWNS`.
    pub const MOUNT: Self = Self(1 << 0);
    /// Isolate process IDs with `CLONE_NEWPID`.
    pub const PID: Self = Self(1 << 1);
    /// Isolate networking with `CLONE_NEWNET`.
    pub const NET: Self = Self(1 << 2);
    /// Isolate hostname and domain name with `CLONE_NEWUTS`.
    pub const UTS: Self = Self(1 << 3);
    /// Isolate System V IPC and POSIX message queues with `CLONE_NEWIPC`.
    pub const IPC: Self = Self(1 << 4);

    /// Return a flag set with no namespaces enabled.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Return a flag set with every supported namespace enabled.
    pub const fn all() -> Self {
        Self(Self::MOUNT.0 | Self::PID.0 | Self::NET.0 | Self::UTS.0 | Self::IPC.0)
    }

    /// Return whether no namespaces are enabled.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Return whether every flag in `other` is enabled.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Convert the configured namespaces to the flags accepted by `libc::unshare`.
    pub const fn to_libc(self) -> i32 {
        let mut flags = 0;
        if self.contains(Self::MOUNT) {
            flags |= 0x0002_0000;
        }
        if self.contains(Self::PID) {
            flags |= 0x2000_0000;
        }
        if self.contains(Self::NET) {
            flags |= 0x4000_0000;
        }
        if self.contains(Self::UTS) {
            flags |= 0x0400_0000;
        }
        if self.contains(Self::IPC) {
            flags |= 0x0800_0000;
        }
        flags
    }
}

impl Default for NamespaceFlags {
    fn default() -> Self {
        Self::empty()
    }
}

impl BitOr for NamespaceFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for NamespaceFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Display for NamespaceFlags {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut separator = "";
        for (flag, name) in [
            (Self::MOUNT, "mount"),
            (Self::PID, "pid"),
            (Self::NET, "net"),
            (Self::UTS, "uts"),
            (Self::IPC, "ipc"),
        ] {
            if self.contains(flag) {
                formatter.write_str(separator)?;
                formatter.write_str(name)?;
                separator = ",";
            }
        }
        Ok(())
    }
}

impl FromStr for NamespaceFlags {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut flags = Self::empty();
        for name in value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            flags |= match name.to_ascii_lowercase().as_str() {
                "mount" => Self::MOUNT,
                "pid" => Self::PID,
                "net" => Self::NET,
                "uts" => Self::UTS,
                "ipc" => Self::IPC,
                _ => return Err(format!("unknown namespace: {name}")),
            };
        }
        Ok(flags)
    }
}

impl Serialize for NamespaceFlags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NamespaceFlags {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Apply namespace isolation to the current process.
///
/// Returns successfully when no flags are requested. Callers should treat
/// errors as best-effort isolation failures and continue after logging them.
#[cfg(target_os = "linux")]
pub fn apply(flags: NamespaceFlags) -> std::io::Result<()> {
    if flags.is_empty() {
        return Ok(());
    }
    let result = unsafe { libc::unshare(flags.to_libc()) };
    if result != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Apply namespace isolation to the current process.
///
/// Namespace isolation is unavailable outside Linux, so this is a no-op.
#[cfg(not(target_os = "linux"))]
pub fn apply(_flags: NamespaceFlags) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_flags_default_is_empty() {
        assert!(NamespaceFlags::default().is_empty());
    }

    #[test]
    fn namespace_flags_to_libc_combines_all_set_bits() {
        assert_eq!(NamespaceFlags::all().to_libc(), 0x6c02_0000);
    }

    #[test]
    fn namespace_flags_serializes_to_csv() {
        let flags = NamespaceFlags::MOUNT | NamespaceFlags::PID | NamespaceFlags::NET;
        assert_eq!(serde_json::to_string(&flags).unwrap(), r#""mount,pid,net""#);
    }

    #[test]
    fn namespace_flags_deserializes_from_csv() {
        let flags: NamespaceFlags = serde_json::from_str(r#""mount,pid,net""#).unwrap();
        assert_eq!(
            flags,
            NamespaceFlags::MOUNT | NamespaceFlags::PID | NamespaceFlags::NET
        );
    }

    #[test]
    fn namespace_apply_with_empty_flags_is_noop() {
        assert!(apply(NamespaceFlags::empty()).is_ok());
    }

    #[cfg(not(unix))]
    #[test]
    fn namespace_apply_on_non_unix_returns_ok() {
        assert!(apply(NamespaceFlags::all()).is_ok());
    }
}
