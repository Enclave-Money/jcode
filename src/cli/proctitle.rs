//! Mapping from parsed CLI arguments to an initial process title.
//!
//! This logic depends on the clap `Args`/`Command` types defined in `cli`, so
//! it lives in the CLI layer. The low-level title-setting primitives it uses
//! (`compact_process_title`, `session_name`, `set_title`) live in the
//! `process_title` core module.

use crate::cli::args::{AmbientCommand, Args, Command};
use crate::process_title::{compact_process_title, session_name, set_title};

pub(crate) fn initial_title(args: &Args) -> String {
    match &args.command {
        Some(Command::Serve { .. }) => "blaude:server".to_string(),
        Some(Command::Acp) => "blaude acp".to_string(),
        Some(Command::Server { .. }) => "blaude server".to_string(),
        Some(Command::Connect) => "blaude:client".to_string(),
        #[cfg(unix)]
        Some(Command::ApiBridge { .. }) => "blaude api-bridge".to_string(),
        Some(Command::Run { .. }) => "blaude run".to_string(),
        Some(Command::Login { .. }) => "blaude login".to_string(),
        Some(Command::Account { .. }) => "blaude account".to_string(),
        Some(Command::Repl) => "blaude repl".to_string(),
        Some(Command::Update) => "blaude update".to_string(),
        Some(Command::Version { .. }) => "blaude version".to_string(),
        Some(Command::Brief { .. }) => "blaude brief".to_string(),
        Some(Command::Prune { .. }) => "blaude prune".to_string(),
        Some(Command::Usage { .. }) => "blaude usage".to_string(),
        Some(Command::Telemetry(_)) => "blaude telemetry".to_string(),
        Some(Command::SelfDev { .. }) => "blaude:selfdev".to_string(),
        Some(Command::Debug { .. }) => "blaude debug".to_string(),
        Some(Command::Auth(_)) => "blaude auth".to_string(),
        Some(Command::Provider(_)) => "blaude provider".to_string(),
        Some(Command::Memory(_)) => "blaude memory".to_string(),
        Some(Command::Council(_)) => "blaude council".to_string(),
        Some(Command::Session(_)) => "blaude session".to_string(),
        Some(Command::Ambient(subcommand)) => match subcommand {
            AmbientCommand::RunVisible => "blaude ambient visible".to_string(),
            _ => "blaude ambient".to_string(),
        },
        Some(Command::Cloud(_)) => "blaude cloud".to_string(),
        Some(Command::Pair { .. }) => "blaude pair".to_string(),
        Some(Command::Permissions) => "blaude permissions".to_string(),
        Some(Command::Transcript { .. }) => "blaude transcript".to_string(),
        Some(Command::Dictate { .. }) => "blaude dictate".to_string(),
        Some(Command::SetupHotkey {
            listen_macos_hotkey,
            notify_cli_launch,
            listen_windows_hotkey,
            uninstall,
        }) => {
            if *listen_macos_hotkey || *listen_windows_hotkey {
                "blaude hotkey listener".to_string()
            } else if notify_cli_launch.is_some() {
                "blaude shortcut reminder".to_string()
            } else if *uninstall {
                "blaude hotkey uninstall".to_string()
            } else {
                "blaude hotkey setup".to_string()
            }
        }
        Some(Command::Browser { .. }) => "blaude browser".to_string(),
        Some(Command::Replay { .. }) => "blaude replay".to_string(),
        Some(Command::Model(_)) => "blaude model".to_string(),
        Some(Command::ProviderTestCoverage { .. }) => "blaude provider-test-coverage".to_string(),
        Some(Command::ProviderDoctor { .. }) => "blaude provider-doctor".to_string(),
        Some(Command::AuthTest { .. }) => "blaude auth-test".to_string(),
        Some(Command::Restart { .. }) => "blaude restart".to_string(),
        Some(Command::Menubar { .. }) => "blaude menubar".to_string(),
        Some(Command::SetupLauncher) => "blaude setup-launcher".to_string(),
        None => {
            if let Some(resume) = args.resume.as_deref().filter(|resume| !resume.is_empty()) {
                let prefix = if crate::cli::selfdev::client_selfdev_requested() {
                    "blaude:d:"
                } else {
                    "blaude:c:"
                };
                compact_process_title(prefix, Some(&session_name(resume)))
            } else if crate::cli::selfdev::client_selfdev_requested() {
                "blaude:selfdev".to_string()
            } else {
                "blaude:client".to_string()
            }
        }
    }
}

pub(crate) fn set_initial_title(args: &Args) {
    set_title(initial_title(args));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::lock_test_env;
    use clap::Parser;

    const SELFDEV_ENV: &str = jcode_selfdev_types::CLIENT_SELFDEV_ENV;

    fn with_selfdev_env_removed<T>(f: impl FnOnce() -> T) -> T {
        let _guard = lock_test_env();
        let previous = std::env::var_os(SELFDEV_ENV);
        crate::env::remove_var(SELFDEV_ENV);
        let result = f();
        if let Some(value) = previous {
            crate::env::set_var(SELFDEV_ENV, value);
        }
        result
    }

    #[test]
    fn initial_title_labels_server() {
        with_selfdev_env_removed(|| {
            let args = Args::parse_from(["jcode", "serve"]);
            assert_eq!(initial_title(&args), "blaude:server");
        });
    }

    #[test]
    fn initial_title_labels_resume_client_with_short_name() {
        with_selfdev_env_removed(|| {
            let args = Args::parse_from(["jcode", "--resume", "session_fox_123"]);
            assert_eq!(initial_title(&args), "blaude:c:fox");
        });
    }

    #[test]
    fn initial_title_labels_selfdev_command() {
        with_selfdev_env_removed(|| {
            let args = Args::parse_from(["jcode", "self-dev"]);
            assert_eq!(initial_title(&args), "blaude:selfdev");
        });
    }

    #[test]
    fn initial_title_labels_windows_hotkey_listener() {
        let args = Args::parse_from(["jcode", "setup-hotkey", "--listen-windows-hotkey"]);
        assert_eq!(initial_title(&args), "blaude hotkey listener");
    }

    #[test]
    fn initial_title_labels_hotkey_uninstall() {
        let args = Args::parse_from(["jcode", "setup-hotkey", "--uninstall"]);
        assert_eq!(initial_title(&args), "blaude hotkey uninstall");
    }
}
