//! Filesystem confinement for the `bash` tool, enabled by CONGA_SANDBOX=1.
//! Fail-closed: if confinement cannot be applied, the command is refused.

/// Generate a Seatbelt (sandbox-exec) SBPL profile: allow everything broadly,
/// deny file writes everywhere except cwd / tmp / var/tmp. Pure function; the
/// CALLER canonicalizes paths (Seatbelt matches by resolved real path, so a
/// symlinked root like /var/tmp would never match as a literal).
#[cfg(target_os = "macos")]
fn seatbelt_profile(cwd: &str, tmp: &str, var_tmp: &str) -> String {
    format!(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{cwd}\"))\n\
         (allow file-write* (subpath \"{tmp}\"))\n\
         (allow file-write* (subpath \"{var_tmp}\"))\n"
    )
}

/// Read the sandbox flag from the ToolContext env map (host-populated from
/// the process env). Exact "1" only — no truthy-string guessing.
pub(crate) fn sandbox_enabled(env: &std::collections::HashMap<String, String>) -> bool {
    env.get("CONGA_SANDBOX").map(String::as_str) == Some("1")
}

/// Verify Seatbelt can actually apply a RESTRICTIVE profile on this machine.
///
/// Apple has been locking `sandbox-exec` down: on recent macOS any profile
/// containing a `deny` rule is refused with `sandbox_apply: Operation not
/// permitted` (exit 71), while a pure-allow profile still applies. Wrapping
/// commands in a sandbox that silently cannot confine is worse than no
/// sandbox — the operator believes they are protected — so probe the real
/// capability once and let the caller refuse to run.
///
/// The probe is generic (it asks "can this machine deny at all?", not "is
/// this particular profile valid?") so the verdict can be cached for the
/// process; per-profile rejections still surface as a failing command.
/// Cached because it spawns a process.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_usable() -> Result<(), String> {
    static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PROBE
        .get_or_init(|| {
            // /usr/bin/true (not /bin/true — that path does not exist on macOS)
            // with all stdio nulled: it performs no file writes, so on a machine
            // where Seatbelt works the deny rule is simply never triggered.
            match std::process::Command::new("sandbox-exec")
                .arg("-p")
                .arg("(version 1)(allow default)(deny file-write*)")
                .arg("/usr/bin/true")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => Err(format!(
                    "macOS refused to apply the sandbox profile (sandbox-exec exited {}); \
                 this system cannot confine the bash tool, so CONGA_SANDBOX=1 is unavailable",
                    s.code().unwrap_or(-1)
                )),
                Err(e) => Err(format!(
                    "sandbox self-check could not run sandbox-exec: {e}"
                )),
            }
        })
        .clone()
}

/// Apply filesystem confinement to `cmd`. MUST be called before cwd/env are
/// set on `cmd` (the macOS branch rewrites program+args wholesale).
/// Err = fail-closed: the caller must refuse to run the command.
pub(crate) fn confine(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // Capability check first: a platform that cannot deny at all must be
        // reported as "sandbox unavailable", not as a failed command.
        seatbelt_usable()?;
        let cwd_c = cwd
            .canonicalize()
            .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
        // Premise: this whitelist is read from the PROCESS env on the
        // assumption that hosts populate ToolContext.env from the process
        // env (the child sees the filtered ctx env); a host building its
        // own env map could diverge whitelist and the child's $TMPDIR.
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        // /var -> /private/var etc: canonicalize or the subpath rules
        // never match Seatbelt's resolved-path checks. Unresolvable path
        // (e.g. dangling TMPDIR) falls back to the literal unchanged.
        let canon = |p: String| {
            std::path::Path::new(&p)
                .canonicalize()
                .map(|c| c.display().to_string())
                .unwrap_or(p)
        };
        let profile = seatbelt_profile(
            &cwd_c.display().to_string(),
            &canon(tmp),
            &canon("/var/tmp".to_string()),
        );
        let std_cmd = cmd.as_std_mut();
        let program = std_cmd.get_program().to_os_string();
        let args: Vec<_> = std_cmd.get_args().map(std::ffi::OsString::from).collect();
        *cmd = tokio::process::Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile).arg(program).args(args);
        Ok(())
    }
    #[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
    {
        confine_landlock(cmd, cwd)
    }
    #[cfg(all(target_os = "linux", not(feature = "sandbox-landlock")))]
    {
        let _ = (cmd, cwd);
        Err("CONGA_SANDBOX=1 but this build lacks the landlock backend; rebuild conga with --features sandbox-landlock".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cmd, cwd);
        Err("sandbox unsupported on this platform".into())
    }
}

/// Landlock confinement for Linux: enforced in pre_exec so the ruleset
/// applies to the exec'd child and its whole process tree.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn confine_landlock(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
    let tmp = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
    // pre_exec runs between fork and exec: Landlock is inherited by the
    // exec'd child and its whole process tree. Owned paths (no borrows) so
    // the closure is Send + 'static.
    unsafe {
        cmd.as_std_mut()
            .pre_exec(move || landlock_ruleset(&cwd, &tmp).map_err(std::io::Error::other));
    }
    Ok(())
}

/// Read-only filesystem everywhere except cwd/TMPDIR (/var/tmp via a fourth
/// rule). Errors (unsupported kernel, missing paths) reach pre_exec and fail
/// the spawn -> fail-closed.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn landlock_ruleset(cwd: &std::path::Path, tmp: &std::path::Path) -> Result<(), String> {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };
    let read = AccessFs::from_read(ABI::V5);
    let read_write = AccessFs::from_all(ABI::V5);

    Ruleset::default()
        // Fail-closed: a kernel without Landlock (or missing the V1 core
        // access set) must error out here, not silently skip confinement.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .map_err(|e| e.to_string())?
        // Everything beyond V1 (Refer, Truncate, IoctlDev, ...) is enforced
        // opportunistically where the running kernel supports it.
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(read_write)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| e.to_string())?,
            read,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new(cwd).map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new(tmp).map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/var/tmp").map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .no_new_privs(true)
        .restrict_self()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn profile_allows_read_execute_and_denies_write_by_default() {
        let p = seatbelt_profile("/tmp/cwd", "/tmp/dir", "/private/var/tmp");
        assert!(p.contains("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "read/exec broadly: {p}");
        assert!(p.contains("(deny file-write*)"), "deny writes: {p}");
        assert!(
            p.contains("(allow file-write* (subpath \"/tmp/cwd\"))"),
            "{p}"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/tmp/dir\"))"),
            "{p}"
        );
    }

    /// The probe must agree with reality: run the same deny-rule profile
    /// directly and assert `seatbelt_usable()` reached the same verdict.
    /// This is the test that catches the platform regression — on a macOS
    /// where Seatbelt still works it asserts Ok; on one where `sandbox-exec`
    /// has been neutered it asserts Err. Self-consistent either way, so it
    /// never goes red purely because the host OS changed.
    #[test]
    fn seatbelt_usable_matches_direct_sandbox_exec() {
        let direct = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1)(allow default)(deny file-write*)")
            .arg("/usr/bin/true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let expected_ok = matches!(direct, Ok(s) if s.success());
        assert_eq!(
            seatbelt_usable().is_ok(),
            expected_ok,
            "probe disagrees with a direct sandbox-exec invocation: {:?}",
            direct
        );
        if !expected_ok {
            // Documented degradation, not a code defect: surface it loudly.
            eprintln!(
                "NOTE: this macOS refuses restrictive Seatbelt profiles; \
                 CONGA_SANDBOX=1 is unavailable here."
            );
        }
    }

    #[test]
    fn profile_includes_var_tmp_as_passed() {
        let p = seatbelt_profile("/x", "/y", "/private/var/tmp");
        assert!(
            p.contains("(allow file-write* (subpath \"/private/var/tmp\"))"),
            "{p}"
        );
    }

    /// Seatbelt applies the LAST matching rule, so the blanket deny must
    /// come BEFORE the per-path allows — otherwise the allows would be
    /// dead text and every write would be refused.
    #[test]
    fn profile_denies_writes_before_allowing_the_whitelisted_roots() {
        let p = seatbelt_profile("/tmp/cwd", "/tmp/dir", "/private/var/tmp");
        let deny = p.find("(deny file-write*)").expect("deny rule");
        for sub in ["/tmp/cwd", "/tmp/dir", "/private/var/tmp"] {
            let allow = p
                .find(&format!("(allow file-write* (subpath \"{sub}\"))"))
                .unwrap_or_else(|| panic!("no allow rule for {sub}: {p}"));
            assert!(
                deny < allow,
                "the deny rule must precede the allow for {sub}: {p}"
            );
        }
    }

    /// Exactly three write roots are whitelisted — no more, no less.
    #[test]
    fn profile_whitelists_exactly_the_three_write_roots() {
        let p = seatbelt_profile("/a", "/b", "/c");
        assert_eq!(p.matches("(allow file-write*").count(), 3, "{p}");
        assert_eq!(p.matches("(deny file-write*)").count(), 1, "{p}");
        assert!(p.contains("(subpath \"/a\")"), "{p}");
        assert!(p.contains("(subpath \"/b\")"), "{p}");
        assert!(p.contains("(subpath \"/c\")"), "{p}");
    }

    /// The capability probe is cached per process: repeated calls must give
    /// the same verdict, and a refusal must say why (and name the flag that
    /// asked for it).
    #[test]
    fn seatbelt_usable_is_cached_and_explains_a_refusal() {
        let first = seatbelt_usable();
        let second = seatbelt_usable();
        assert_eq!(first.is_ok(), second.is_ok(), "the probe must be stable");
        if let Err(e) = &first {
            assert!(e.contains("sandbox"), "{e}");
            assert!(e.contains("CONGA_SANDBOX=1"), "{e}");
        }
    }

    /// `confine` rewrites the command to run under `sandbox-exec` with the
    /// generated profile, keeping the original program and arguments. When
    /// the platform cannot confine, it must fail closed (an `Err` that
    /// names the sandbox) instead of running the command unguarded.
    #[test]
    fn confine_rewrites_the_command_to_sandbox_exec() {
        let cwd = tempfile::tempdir().unwrap();
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        if let Err(e) = confine(&mut cmd, cwd.path()) {
            // Seatbelt cannot confine on this machine: fail closed, loudly.
            assert!(e.contains("sandbox"), "{e}");
            assert!(e.contains("CONGA_SANDBOX=1"), "{e}");
            return;
        }
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "sandbox-exec");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-p");
        let profile = &args[1];
        assert!(profile.contains("(deny file-write*)"), "{profile}");
        let cwd_canon = cwd.path().canonicalize().unwrap().display().to_string();
        assert!(
            profile.contains(&cwd_canon),
            "the profile must whitelist the cwd {cwd_canon}: {profile}"
        );
        // The original program and arguments survive the rewrite.
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "-c");
        assert_eq!(args[4], "echo hi");
    }

    /// Error path: a cwd that does not exist cannot be confined, so the
    /// caller must fail closed instead of running the command.
    #[test]
    fn confine_rejects_a_missing_cwd() {
        if seatbelt_usable().is_err() {
            // Without a working Seatbelt the capability check fires first —
            // also a fail-closed refusal, but with a different reason.
            let mut cmd = tokio::process::Command::new("true");
            let err = confine(&mut cmd, std::path::Path::new("/conga-no-such-dir-xyz"))
                .unwrap_err();
            assert!(err.contains("sandbox"), "{err}");
            return;
        }
        let mut cmd = tokio::process::Command::new("true");
        let err = confine(&mut cmd, std::path::Path::new("/conga-no-such-dir-xyz")).unwrap_err();
        assert!(err.contains("cwd not accessible"), "{err}");
    }
}

// The env-flag logic is cross-platform, so it gets its own non-gated test
// module (the `tests` module above is macOS-only for the seatbelt profile).
#[cfg(test)]
mod flag_tests {
    use super::*;

    #[test]
    fn sandbox_enabled_only_on_exact_flag() {
        let mut env = std::collections::HashMap::new();
        assert!(!sandbox_enabled(&env));
        env.insert("CONGA_SANDBOX".to_string(), "0".to_string());
        assert!(!sandbox_enabled(&env));
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());
        assert!(sandbox_enabled(&env));
    }

    /// Boundary: only the exact string "1" enables the sandbox — no
    /// truthy-string guessing, no surrounding whitespace.
    #[test]
    fn sandbox_enabled_rejects_truthy_lookalikes() {
        let mut env = std::collections::HashMap::new();
        for value in ["true", "TRUE", "yes", "1 ", " 1", "01", "1.0", ""] {
            env.insert("CONGA_SANDBOX".to_string(), value.to_string());
            assert!(
                !sandbox_enabled(&env),
                "{value:?} must not enable the sandbox"
            );
        }
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());
        assert!(sandbox_enabled(&env));
    }
}

// Landlock tests compile (and run) only on Linux with the feature on. On the
// macOS dev box they are verified via cross-target `cargo check`.
#[cfg(all(test, target_os = "linux", feature = "sandbox-landlock"))]
mod landlock_tests {
    use super::*;

    #[test]
    fn landlock_ruleset_builds_for_existing_paths() {
        let cwd = tempfile::tempdir().unwrap();
        // Enforcing here sandboxes only this test's thread (Landlock is
        // per-thread and libtest spawns one thread per test), and the ruleset
        // grants rw beneath the tempdir, so the test runner is unaffected.
        assert!(landlock_ruleset(cwd.path(), std::path::Path::new("/tmp")).is_ok());
    }

    /// `confine_landlock` installs the ruleset in `pre_exec` (between fork
    /// and exec), so installing it must succeed without touching this
    /// process — the confinement applies to the spawned child only.
    #[test]
    fn confine_landlock_installs_the_ruleset_for_existing_paths() {
        let cwd = tempfile::tempdir().unwrap();
        let mut cmd = tokio::process::Command::new("true");
        assert!(
            confine_landlock(&mut cmd, cwd.path()).is_ok(),
            "installing the ruleset must succeed"
        );
    }

    /// Error path: a cwd that does not exist cannot be confined — the
    /// caller has to fail closed rather than run the command unguarded.
    #[test]
    fn confine_landlock_rejects_a_missing_cwd() {
        let mut cmd = tokio::process::Command::new("true");
        let err = confine_landlock(&mut cmd, std::path::Path::new("/conga-no-such-dir-xyz"))
            .unwrap_err();
        assert!(err.contains("cwd not accessible"), "{err}");
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "sandbox-landlock")))]
mod no_landlock_tests {
    use super::*;

    #[test]
    fn confine_without_feature_fails_closed_with_hint() {
        let mut cmd = tokio::process::Command::new("true");
        let err = confine(&mut cmd, std::path::Path::new("/tmp")).unwrap_err();
        assert!(err.contains("--features sandbox-landlock"), "{err}");
    }
}
