//! Shared secret-input resolution for commands that need a password or OTP.
//!
//! Secrets must never travel through argv: argv is visible to process listings
//! and persists in shell history. They are read from an environment variable
//! (for headless automation/CI) or from a hidden terminal prompt (for
//! interactive use). A noninteractive caller that lacks a required secret
//! fails before the network with an actionable message naming the variable or
//! interaction it needs. No secret value is ever echoed, logged, or included
//! in an error.
//!
//! Environment variables take precedence over prompting so CI can supply
//! secrets without a terminal. Empty environment values and empty interactive
//! input are rejected when the secret was requested or required.

use std::io::IsTerminal;

/// Resolve the account password required by `bpm token create`.
///
/// Precedence: nonempty `$BPM_PASSWORD`; otherwise a hidden interactive prompt
/// when stdin is a terminal; otherwise an actionable error. Passwords are not
/// trimmed; an empty value (from either source) is rejected.
pub(crate) fn required_password() -> anyhow::Result<String> {
    let env_value = std::env::var_os("BPM_PASSWORD").map(|s| s.to_string_lossy().into_owned());
    resolve_required_secret(
        env_value,
        std::io::stdin().is_terminal(),
        || rpassword::prompt_password("Password: ").map_err(anyhow::Error::from),
        "BPM_PASSWORD",
        "password",
    )
}

/// Resolve the optional OTP for publish and token mutations.
///
/// Precedence: nonempty `$BPM_OTP` when set (an empty value is rejected);
/// otherwise a hidden interactive prompt only when `prompt_requested`
/// (`--prompt-otp`) is true and stdin is a terminal; otherwise no OTP is sent.
/// Surrounding whitespace is rejected rather than silently stripped.
pub(crate) fn optional_otp(prompt_requested: bool) -> anyhow::Result<Option<String>> {
    let env_value = std::env::var_os("BPM_OTP").map(|s| s.to_string_lossy().into_owned());
    resolve_optional_secret(
        env_value,
        prompt_requested,
        std::io::stdin().is_terminal(),
        || rpassword::prompt_password("OTP: ").map_err(anyhow::Error::from),
        "BPM_OTP",
    )
}

/// Pure resolution of a required secret, factored out so unit tests can cover
/// precedence and error branches without mutating the process-global
/// environment. The prompt callback is invoked only when the environment value
/// is absent and a terminal is available.
fn resolve_required_secret(
    env_value: Option<String>,
    is_terminal: bool,
    prompt: impl FnOnce() -> anyhow::Result<String>,
    var_name: &str,
    kind: &str,
) -> anyhow::Result<String> {
    if let Some(value) = env_value {
        if value.is_empty() {
            anyhow::bail!("${var_name} is set but empty; provide a nonempty {kind}");
        }
        return Ok(value);
    }
    if is_terminal {
        let value = prompt()?;
        if value.is_empty() {
            anyhow::bail!("no {kind} entered");
        }
        return Ok(value);
    }
    anyhow::bail!(
        "a {kind} is required but none was provided: set ${var_name} or run in an interactive terminal"
    )
}

/// Pure resolution of an optional secret (OTP), factored out for testing.
fn resolve_optional_secret(
    env_value: Option<String>,
    prompt_requested: bool,
    is_terminal: bool,
    prompt: impl FnOnce() -> anyhow::Result<String>,
    var_name: &str,
) -> anyhow::Result<Option<String>> {
    if let Some(value) = env_value {
        validate_otp(&value, var_name)?;
        return Ok(Some(value));
    }
    if prompt_requested && is_terminal {
        let value = prompt()?;
        validate_otp(&value, var_name)?;
        return Ok(Some(value));
    }
    Ok(None)
}

/// Reject empty or whitespace-padded OTP values without echoing them. An OTP
/// with surrounding whitespace is rejected rather than silently trimmed so a
/// mis-pasted value cannot be sent to the registry.
fn validate_otp(value: &str, var_name: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.trim().is_empty() {
        anyhow::bail!("OTP is empty; provide a nonempty value or unset ${var_name}");
    }
    if value != value.trim() {
        anyhow::bail!(
            "OTP has surrounding whitespace; re-enter it without leading/trailing spaces or unset ${var_name}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_secret_prefers_environment_value() {
        let value = resolve_required_secret(
            Some("hunter2-not-real".into()),
            true,
            || panic!("prompt must not run when env value is present"),
            "BPM_PASSWORD",
            "password",
        )
        .expect("env value resolves");
        assert_eq!(value, "hunter2-not-real");
    }

    #[test]
    fn required_secret_rejects_empty_environment_value() {
        let err = resolve_required_secret(
            Some(String::new()),
            true,
            || panic!("prompt must not run for empty env"),
            "BPM_PASSWORD",
            "password",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("BPM_PASSWORD"),
            "error names the variable: {err}"
        );
        assert!(!err.contains("hunter2"), "error never echoes a value");
    }

    #[test]
    fn required_secret_prompts_in_terminal_without_env() {
        let value = resolve_required_secret(
            None,
            true,
            || Ok("from-prompt".into()),
            "BPM_PASSWORD",
            "password",
        )
        .expect("terminal prompt resolves");
        assert_eq!(value, "from-prompt");
    }

    #[test]
    fn required_secret_fails_noninteractive_without_env() {
        let err = resolve_required_secret(
            None,
            false,
            || panic!("prompt must not run without a terminal"),
            "BPM_PASSWORD",
            "password",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("BPM_PASSWORD") && err.contains("interactive terminal"),
            "actionable noninteractive error: {err}"
        );
    }

    #[test]
    fn required_secret_rejects_empty_prompt_input() {
        let err =
            resolve_required_secret(None, true, || Ok(String::new()), "BPM_PASSWORD", "password")
                .unwrap_err()
                .to_string();
        assert!(err.contains("password"), "error names the kind: {err}");
    }

    #[test]
    fn optional_secret_prefers_environment_value() {
        let otp = resolve_optional_secret(
            Some("123456".into()),
            false,
            false,
            || panic!("prompt must not run"),
            "BPM_OTP",
        )
        .expect("env value resolves");
        assert_eq!(otp.as_deref(), Some("123456"));
    }

    #[test]
    fn optional_secret_rejects_empty_environment_value() {
        let err = resolve_optional_secret(
            Some(String::new()),
            false,
            true,
            || panic!("prompt must not run"),
            "BPM_OTP",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("BPM_OTP"), "error names the variable: {err}");
    }

    #[test]
    fn optional_secret_rejects_whitespace_padding() {
        let err = resolve_optional_secret(
            Some(" 123456 ".into()),
            false,
            true,
            || panic!("prompt must not run"),
            "BPM_OTP",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("whitespace"), "error flags padding: {err}");
    }

    #[test]
    fn optional_secret_returns_none_when_not_requested() {
        // No env value and no prompt request → no OTP, regardless of terminal.
        let otp = resolve_optional_secret(
            None,
            false,
            true,
            || panic!("prompt must not run when not requested"),
            "BPM_OTP",
        )
        .expect("absent OTP is not an error");
        assert!(otp.is_none());
    }

    #[test]
    fn optional_secret_does_not_prompt_without_terminal_even_when_requested() {
        // --prompt-otp in a noninteractive context silently sends no OTP rather
        // than blocking or erroring.
        let otp = resolve_optional_secret(
            None,
            true,
            false,
            || panic!("prompt must not run without a terminal"),
            "BPM_OTP",
        )
        .expect("no OTP is fine");
        assert!(otp.is_none());
    }

    #[test]
    fn optional_secret_prompts_when_requested_in_terminal() {
        let otp = resolve_optional_secret(None, true, true, || Ok("654321".into()), "BPM_OTP")
            .expect("prompted OTP resolves");
        assert_eq!(otp.as_deref(), Some("654321"));
    }

    #[test]
    fn optional_secret_rejects_empty_prompted_otp() {
        let err = resolve_optional_secret(None, true, true, || Ok(String::new()), "BPM_OTP")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OTP"), "error names the kind: {err}");
    }
}
