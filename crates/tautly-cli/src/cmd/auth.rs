//! `tautly auth add <provider>` (spec 7.11.2): store an API key in the OS keychain
//! (service `tautly`, account = the provider). The key is read with hidden input from the
//! terminal, or (with `--stdin`) as one line of a pipe, and never printed.

use std::io::{BufRead, IsTerminal};

use clap::Subcommand;
use tautly_core::{AuthError, KEYCHAIN_SERVICE};

use crate::local::{self, Secrets};

/// Longest key accepted (no provider's key is anywhere near this).
const MAX_KEY_BYTES: u64 = 16 * 1024;

const PROVIDERS: &str = "anthropic, openai, google, openrouter or compat:<name>";

#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    /// Store an API key in the OS keychain (service `tautly`). It is read with hidden
    /// input from the terminal, and never printed.
    Add {
        /// anthropic, openai, google, openrouter or compat:<name>.
        #[arg(value_parser = parse_provider, value_name = "PROVIDER")]
        provider: AuthProvider,
        /// Read the key from standard input (its first line) when that is not a terminal.
        #[arg(long)]
        stdin: bool,
    },
}

/// A provider a key can be stored for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthProvider {
    /// How messages name it (`openrouter`, `compat:local`).
    pub label: String,
    /// The keychain account (`anthropic`, `compat:openrouter`).
    pub account: String,
    /// The environment variable that holds the key instead; `None` for a compat endpoint
    /// (its `api_key_env` names one).
    pub env_var: Option<&'static str>,
}

fn parse_provider(s: &str) -> Result<AuthProvider, String> {
    let native = |var| AuthProvider {
        label: s.to_string(),
        account: s.to_string(),
        env_var: Some(var),
    };
    match s {
        "anthropic" => Ok(native("ANTHROPIC_API_KEY")),
        "openai" => Ok(native("OPENAI_API_KEY")),
        "google" => Ok(native("GEMINI_API_KEY")),
        "openrouter" | "compat:openrouter" => Ok(AuthProvider {
            label: s.to_string(),
            account: "compat:openrouter".to_string(),
            env_var: Some("OPENROUTER_API_KEY"),
        }),
        _ => match s.strip_prefix("compat:") {
            // The same names `[[providers.compat]]` allows.
            Some(name)
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) =>
            {
                Ok(AuthProvider {
                    label: s.to_string(),
                    account: s.to_string(),
                    env_var: None,
                })
            }
            _ => Err(format!("unknown provider; use {PROVIDERS}")),
        },
    }
}

/// Run `tautly auth`; the process exit code.
pub fn main(cmd: AuthCmd) -> i32 {
    match cmd {
        AuthCmd::Add { provider, stdin } => {
            let input = std::io::stdin();
            let tty = input.is_terminal();
            let read = || -> std::io::Result<String> {
                if tty {
                    // Prompts on the terminal with echo off.
                    rpassword::prompt_password(format!("API key for {}: ", provider.label))
                } else {
                    read_line(input.lock())
                }
            };
            match add(&provider, tty || stdin, read, &local::secrets()) {
                Ok(message) => {
                    println!("{message}");
                    0
                }
                Err(e) => {
                    eprintln!("tautly: {e}");
                    1
                }
            }
        }
    }
}

/// The first line of `input`, at most [`MAX_KEY_BYTES`].
fn read_line(input: impl BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    input.take(MAX_KEY_BYTES + 1).read_line(&mut line)?;
    if line.len() as u64 > MAX_KEY_BYTES {
        return Err(std::io::Error::other("the key is too long"));
    }
    Ok(line)
}

/// Read, check, store and verify the key; the message to print. `may_read` is false when
/// standard input is a pipe and `--stdin` was not given. No message contains the key.
fn add(
    provider: &AuthProvider,
    may_read: bool,
    read: impl FnOnce() -> std::io::Result<String>,
    secrets: &Secrets,
) -> Result<String, String> {
    if !may_read {
        return Err(
            "standard input is not a terminal: pass --stdin to read the key from it".into(),
        );
    }
    let raw = read().map_err(|e| format!("cannot read the key: {e}"))?;
    let key = raw.trim();
    if key.is_empty() {
        return Err("the key is blank; nothing was stored".into());
    }
    let account = provider.account.as_str();
    secrets
        .store
        .set(KEYCHAIN_SERVICE, account, key)
        .map_err(|e| store_error(provider, e))?;
    match secrets.store.get(KEYCHAIN_SERVICE, account) {
        Ok(Some(stored)) if stored == key => Ok(format!(
            "stored key for {} in the {}",
            provider.label, secrets.name
        )),
        Ok(_) => Err(format!(
            "the {} did not return the key just stored for {}",
            secrets.name, provider.label
        )),
        Err(e) => Err(store_error(provider, e)),
    }
}

fn store_error(provider: &AuthProvider, e: AuthError) -> String {
    match e {
        AuthError::KeychainUnavailable(why) => {
            let instead = match provider.env_var {
                Some(var) => format!("set {var} in the environment instead"),
                None => format!(
                    "set api_key_env in the [[providers.compat]] entry `{}` and put the key in \
                     that environment variable instead",
                    provider.account.trim_start_matches("compat:")
                ),
            };
            format!("no keychain available ({why}): {instead}")
        }
        other => other.to_string(),
    }
}
