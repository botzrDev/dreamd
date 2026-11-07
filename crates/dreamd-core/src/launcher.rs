//! How the user started dreamd, which decides the command a hint names
//! (BZR-1469, BZR-1531). The npm shim sets `DREAMD_LAUNCHER=npx`.

/// The variable the npm shim sets for every binary it runs.
pub const LAUNCHER_ENV: &str = "DREAMD_LAUNCHER";

/// How the user started dreamd, which decides the command a hint names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Launcher {
    /// A `dreamd` binary the user runs directly.
    #[default]
    Binary,
    /// The `npx dreamd-mcp` shim, which puts no `dreamd` on PATH.
    Npx,
}

impl Launcher {
    /// `DREAMD_LAUNCHER`'s value. Only the exact string `npx` means npx.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some("npx") => Launcher::Npx,
            _ => Launcher::Binary,
        }
    }

    /// This process's [`LAUNCHER_ENV`].
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var(LAUNCHER_ENV).ok().as_deref())
    }

    /// The command line for `args`, e.g. `watch` or `init --uninstall-project`.
    pub fn command(self, args: &str) -> String {
        match self {
            Launcher::Binary => format!("dreamd {args}"),
            Launcher::Npx => format!("npx -y dreamd-mcp {args}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BZR-1469 test 7: only the exact value `npx` means npx.
    #[test]
    fn launcher_is_npx_only_for_the_exact_value() {
        for value in [None, Some(""), Some("NPX"), Some("npx "), Some("1")] {
            assert_eq!(
                Launcher::from_env_value(value),
                Launcher::Binary,
                "{value:?} is not npx"
            );
        }
        assert_eq!(Launcher::from_env_value(Some("npx")), Launcher::Npx);
        assert_eq!(Launcher::Npx.command("watch"), "npx -y dreamd-mcp watch");
        assert_eq!(
            Launcher::Binary.command("init --uninstall-project"),
            "dreamd init --uninstall-project"
        );
    }
}
