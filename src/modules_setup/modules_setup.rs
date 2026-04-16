use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use crate::common::{OutputMode, confirm_prompt};

/// Source path for the example module file.
const EXAMPLE_MODULE_SRC: &str = "/sw/examples/Lmod/hello/1.0.lua";

/// Arguments for `myrc modules setup`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Skip confirmation prompt (for automation).
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
}

/// Action taken (or skipped) during setup, for JSON output.
#[derive(Debug, Clone)]
enum ActionStatus {
    Created,
    AlreadyExists,
}

impl ActionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::AlreadyExists => "already_exists",
        }
    }
}

#[derive(Debug, Clone)]
struct Action {
    action: &'static str,
    path_display: String,
    src: Option<String>,
    status: ActionStatus,
}

pub fn run(args: &Args, mode: OutputMode) -> Result<()> {
    let home = env::var("HOME").context("$HOME is not set")?;
    let lmod_dir = PathBuf::from(&home).join("Lmod");
    let hello_dir = lmod_dir.join("hello");
    let module_file = hello_dir.join("1.0.lua");

    // In JSON mode, skip confirmation (design: --yes is implicit with --json)
    let skip_prompt = args.yes || mode.is_json();

    if !skip_prompt {
        let confirmed = confirm_prompt("Set up personal Lmod module directory at ~/Lmod?")?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut actions: Vec<Action> = Vec::new();

    // 1. Create ~/Lmod/
    let lmod_status = if lmod_dir.is_dir() {
        ActionStatus::AlreadyExists
    } else {
        fs::create_dir(&lmod_dir).with_context(|| format!("creating {}", lmod_dir.display()))?;
        fs::set_permissions(&lmod_dir, fs::Permissions::from_mode(0o700))?;
        ActionStatus::Created
    };
    if !mode.is_json() {
        match &lmod_status {
            ActionStatus::Created => println!("Created ~/Lmod/"),
            ActionStatus::AlreadyExists => println!("~/Lmod/ already exists."),
        }
    }
    actions.push(Action {
        action: "create_dir",
        path_display: "~/Lmod".into(),
        src: None,
        status: lmod_status,
    });

    // 2. Create ~/Lmod/hello/
    let hello_status = if hello_dir.is_dir() {
        ActionStatus::AlreadyExists
    } else {
        fs::create_dir(&hello_dir).with_context(|| format!("creating {}", hello_dir.display()))?;
        ActionStatus::Created
    };
    if !mode.is_json() {
        match &hello_status {
            ActionStatus::Created => println!("Created ~/Lmod/hello/"),
            ActionStatus::AlreadyExists => println!("~/Lmod/hello/ already exists."),
        }
    }
    actions.push(Action {
        action: "create_dir",
        path_display: "~/Lmod/hello".into(),
        src: None,
        status: hello_status,
    });

    // 3. Copy example module file
    let file_status = if module_file.is_file() {
        ActionStatus::AlreadyExists
    } else {
        fs::copy(EXAMPLE_MODULE_SRC, &module_file).with_context(|| {
            format!("copying {EXAMPLE_MODULE_SRC} to {}", module_file.display())
        })?;
        ActionStatus::Created
    };
    if !mode.is_json() {
        match &file_status {
            ActionStatus::Created => println!("Copied example module to ~/Lmod/hello/1.0.lua"),
            ActionStatus::AlreadyExists => println!("~/Lmod/hello/1.0.lua already exists."),
        }
    }
    actions.push(Action {
        action: "copy_file",
        path_display: "~/Lmod/hello/1.0.lua".into(),
        src: Some(EXAMPLE_MODULE_SRC.into()),
        status: file_status,
    });

    let already_complete = actions
        .iter()
        .all(|a| matches!(a.status, ActionStatus::AlreadyExists));

    if mode.is_json() {
        let json_actions: Vec<serde_json::Value> = actions
            .iter()
            .map(|a| {
                let mut obj = serde_json::json!({
                    "action": a.action,
                    "path": a.path_display,
                    "status": a.status.as_str(),
                });
                if let Some(src) = &a.src {
                    obj["src"] = serde_json::json!(src);
                }
                if a.action == "copy_file" {
                    obj["dst"] = serde_json::json!(a.path_display);
                }
                obj
            })
            .collect();

        let output = serde_json::json!({
            "module": "modules_setup",
            "version": env!("CARGO_PKG_VERSION"),
            "actions": json_actions,
            "already_complete": already_complete,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if already_complete {
        println!("\nLmod setup already complete, nothing to do.");
    } else {
        println!();
    }

    // Print usage instructions
    println!(
        "\
Activating your own modules is done with the module command.

	$ module load use.own

That will add the path to your own modules to the beginning of the _current_
module path when you load use.own.  If you load additional modules after
use.own they may insert module directories in front of yours, so you should
be careful to either not use exactly the same name as existing modules or
to load use.own only after loading all other modules.

Once the use.own module is loaded, you can use these commands

	$ module whatis hello
	$ module help hello
	$ module show hello
	$ module load hello

to investigate the hello example module as well as editing the file that
defines it, ~/Lmod/hello/1.0.lua, which contains comments to help
you understand it."
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_status_as_str() {
        assert_eq!(ActionStatus::Created.as_str(), "created");
        assert_eq!(ActionStatus::AlreadyExists.as_str(), "already_exists");
    }

    #[test]
    fn json_output_already_complete() {
        // Verify that already_complete is correct when all actions are AlreadyExists
        let actions = [
            Action {
                action: "create_dir",
                path_display: "~/Lmod".into(),
                src: None,
                status: ActionStatus::AlreadyExists,
            },
            Action {
                action: "create_dir",
                path_display: "~/Lmod/hello".into(),
                src: None,
                status: ActionStatus::AlreadyExists,
            },
            Action {
                action: "copy_file",
                path_display: "~/Lmod/hello/1.0.lua".into(),
                src: Some(EXAMPLE_MODULE_SRC.into()),
                status: ActionStatus::AlreadyExists,
            },
        ];
        let all_exist = actions
            .iter()
            .all(|a| matches!(a.status, ActionStatus::AlreadyExists));
        assert!(all_exist);
    }

    #[test]
    fn json_output_not_already_complete() {
        let actions = [
            Action {
                action: "create_dir",
                path_display: "~/Lmod".into(),
                src: None,
                status: ActionStatus::Created,
            },
            Action {
                action: "create_dir",
                path_display: "~/Lmod/hello".into(),
                src: None,
                status: ActionStatus::AlreadyExists,
            },
        ];
        let all_exist = actions
            .iter()
            .all(|a| matches!(a.status, ActionStatus::AlreadyExists));
        assert!(!all_exist);
    }

    #[test]
    fn example_module_src_path() {
        assert_eq!(EXAMPLE_MODULE_SRC, "/sw/examples/Lmod/hello/1.0.lua");
    }
}
