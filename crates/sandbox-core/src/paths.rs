use std::path::PathBuf;

/// Filesystem layout for a user-scoped sandboxd install.
///
/// Everything hangs off `~/Library/Application Support/Sandboxd` unless
/// `SANDBOXD_HOME` is set (useful for tests and running a second instance).
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        if let Some(home) = std::env::var_os("SANDBOXD_HOME") {
            let home = PathBuf::from(home);
            return Self {
                logs: home.join("logs"),
                home,
            };
        }
        let user = dirs::home_dir().expect("no home directory");
        Self {
            home: user.join("Library/Application Support/Sandboxd"),
            logs: user.join("Library/Logs/Sandboxd"),
        }
    }

    pub fn socket(&self) -> PathBuf {
        std::env::var_os("SANDBOXD_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join("sandboxd.sock"))
    }

    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }

    pub fn workspaces(&self) -> PathBuf {
        self.home.join("workspaces")
    }

    pub fn policies(&self) -> PathBuf {
        self.home.join("policies")
    }

    pub fn egress_log(&self, id: &str) -> PathBuf {
        self.logs.join("egress").join(format!("{id}.jsonl"))
    }
}
