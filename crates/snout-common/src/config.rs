//! snout's settings and the layered load that produces them.
//!
//! The flake path and host name are runtime configuration rather than being
//! baked in, so one binary serves every host and the NixOS module's only job is
//! to write a JSON file.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::fail::{Code, Error, Result};

/// Where the NixOS module writes its generated configuration.
pub const SYSTEM_PATH: &str = "/etc/snout/config.json";

/// Component kinds.
pub const KIND_DAEMON: &str = "daemon";
pub const KIND_SYSTEMD_USER: &str = "systemd-user";
pub const KIND_COMMAND: &str = "command";

/// Every accepted log level, quietest first. One list, so the validation
/// message and the completion script cannot disagree about it.
pub const LOG_LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];

pub fn valid_log_level(s: &str) -> bool {
    LOG_LEVELS.contains(&s)
}

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// A duration that reads and writes as a string (`"2h"`, `"5m"`).
///
/// Spelling an interval as a bare number of hours stops working the moment a
/// setting is measured in minutes — and the settle delay and both backoff
/// bounds are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration(pub StdDuration);

impl Duration {
    pub const fn from_secs(s: u64) -> Duration {
        Duration(StdDuration::from_secs(s))
    }

    pub fn as_std(self) -> StdDuration {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl std::fmt::Display for Duration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", humantime::format_duration(self.0))
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Duration, D::Error> {
        use serde::de::Error as _;
        let s = String::deserialize(d)
            .map_err(|e| D::Error::custom(format!("duration must be a string like \"2h\": {e}")))?;
        humantime::parse_duration(&s)
            .map(Duration)
            .map_err(|e| D::Error::custom(format!("invalid duration {s:?}: {e}")))
    }
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// The whole settings document.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Absolute path to the working copy this snout operates on. Deliberately
    /// the working tree and not a store path: pointing at the flake's own store
    /// path would make every deploy rebuild the revision that is already
    /// running, which is the opposite of what it is for.
    pub flake: String,
    pub host: String,
    /// Overrides `$XDG_CACHE_HOME/snout`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cache_dir: String,
    pub log_level: String,
    pub daemon: DaemonConfig,
    pub reload: ReloadConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonConfig {
    pub enable: bool,
    /// Paces the quick check. Nothing paces the package-level diff directly: it
    /// runs when a quick check finds the cached one no longer describes the
    /// current lock, and so inherits this as its upper bound.
    pub interval: Duration,
    /// Keeps the first check away from login, which is the worst moment to
    /// start fetching.
    pub settle_delay: Duration,
    pub backoff: Backoff,
}

/// Paces retries after a transient failure — a network blip, a half-reachable
/// cache. A build that fails is handled separately and keyed on the lock it
/// failed for, since that one repeats identically until upstream moves again.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadConfig {
    /// Names an environment variable that must be set for `reload` to mean
    /// anything — the spawn template generally talks to a compositor.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub require_env: String,
    /// How a restarted daemon is launched; `{}` is replaced with the program
    /// name. Launching through the session's own launcher rather than forking
    /// from snout is what parents the process correctly and lets it outlive the
    /// terminal `snout reload` was typed into.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spawn: Vec<String>,
    pub hint_after_deploy: bool,
    pub after_deploy: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub post: Vec<PostCmd>,
}

/// One thing `reload` restarts.
///
/// There is no staleness detection: reload restarts what it is told to,
/// unconditionally, and deciding whether that is wanted is the user's call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    pub name: String,
    pub kind: String,
    /// Overrides the systemd unit name for `systemd-user` components.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// The escape hatch, used by kind `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostCmd {
    pub name: String,
    pub command: Vec<String>,
}

impl ReloadConfig {
    /// A reload section with nothing configured, for tests and for the default.
    pub fn default_empty() -> ReloadConfig {
        Config::default().reload
    }
}

impl Default for Config {
    /// The bottom layer. `flake` is deliberately empty: a standalone binary has
    /// no business guessing where somebody's configuration lives, and an unset
    /// flake produces a real error naming the file and the flag.
    fn default() -> Config {
        Config {
            flake: String::new(),
            host: String::new(),
            cache_dir: String::new(),
            log_level: "info".into(),
            daemon: DaemonConfig {
                enable: true,
                interval: Duration::from_secs(60 * 60),
                settle_delay: Duration::from_secs(5 * 60),
                backoff: Backoff {
                    min: Duration::from_secs(5 * 60),
                    max: Duration::from_secs(2 * 60 * 60),
                },
            },
            reload: ReloadConfig {
                require_env: String::new(),
                spawn: Vec::new(),
                hint_after_deploy: true,
                after_deploy: false,
                notes: Vec::new(),
                components: Vec::new(),
                post: Vec::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Layering
// ---------------------------------------------------------------------------

/// One configuration file, as read.
///
/// Every field is optional so that a document overrides exactly the keys it
/// names and leaves the rest of the accumulated value alone — the merge a
/// layered configuration needs. Note the asymmetry this preserves: a nested
/// object merges field by field, but an array replaces wholesale, and an
/// explicit `false` does override a default `true`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigLayer {
    flake: Option<String>,
    host: Option<String>,
    cache_dir: Option<String>,
    log_level: Option<String>,
    daemon: Option<DaemonLayer>,
    reload: Option<ReloadLayer>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonLayer {
    enable: Option<bool>,
    interval: Option<Duration>,
    settle_delay: Option<Duration>,
    backoff: Option<BackoffLayer>,
}

#[derive(Debug, Default, Deserialize)]
struct BackoffLayer {
    min: Option<Duration>,
    max: Option<Duration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReloadLayer {
    require_env: Option<String>,
    spawn: Option<Vec<String>>,
    hint_after_deploy: Option<bool>,
    after_deploy: Option<bool>,
    notes: Option<Vec<String>>,
    components: Option<Vec<Component>>,
    post: Option<Vec<PostCmd>>,
}

impl ConfigLayer {
    fn apply(self, cfg: &mut Config) {
        set(&mut cfg.flake, self.flake);
        set(&mut cfg.host, self.host);
        set(&mut cfg.cache_dir, self.cache_dir);
        set(&mut cfg.log_level, self.log_level);
        if let Some(d) = self.daemon {
            set(&mut cfg.daemon.enable, d.enable);
            set(&mut cfg.daemon.interval, d.interval);
            set(&mut cfg.daemon.settle_delay, d.settle_delay);
            if let Some(b) = d.backoff {
                set(&mut cfg.daemon.backoff.min, b.min);
                set(&mut cfg.daemon.backoff.max, b.max);
            }
        }
        if let Some(r) = self.reload {
            set(&mut cfg.reload.require_env, r.require_env);
            set(&mut cfg.reload.spawn, r.spawn);
            set(&mut cfg.reload.hint_after_deploy, r.hint_after_deploy);
            set(&mut cfg.reload.after_deploy, r.after_deploy);
            set(&mut cfg.reload.notes, r.notes);
            set(&mut cfg.reload.components, r.components);
            set(&mut cfg.reload.post, r.post);
        }
    }
}

fn set<T>(dst: &mut T, src: Option<T>) {
    if let Some(v) = src {
        *dst = v;
    }
}

/// Builds the configuration from every layer, later winning over earlier:
///
/// ```text
/// built-in defaults
/// /etc/snout/config.json          (or $SNOUT_CONFIG, which replaces that path)
/// $XDG_CONFIG_HOME/snout/config.json
/// environment
/// flags                           (applied by the caller, after this returns)
/// ```
pub fn load() -> Result<Config> {
    let mut cfg = Config::default();

    // A replacement rather than another layer, which is what makes a test
    // hermetic: point this at a fixture and /etc stops existing.
    let named = std::env::var("SNOUT_CONFIG").ok().filter(|s| !s.is_empty());
    let system = named.clone().unwrap_or_else(|| SYSTEM_PATH.to_string());
    merge_file(&mut cfg, Path::new(&system), named.is_some())?;

    if let Some(dir) = user_config_dir() {
        merge_file(&mut cfg, &dir.join("snout").join("config.json"), false)?;
    }

    apply_env(&mut cfg);

    if cfg.host.is_empty()
        && let Ok(h) = hostname()
    {
        cfg.host = h;
    }
    Ok(cfg)
}

/// Overlays one document. A missing file is not an error unless it was named
/// explicitly — asking for a config that is not there should say so, whereas
/// the absence of the optional per-user override is the normal case.
fn merge_file(cfg: &mut Config, path: &Path, required: bool) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if required {
                return Err(Error::config(format!(
                    "config file {} does not exist",
                    path.display()
                )));
            }
            return Ok(());
        }
        Err(e) => {
            return Err(Error::wrap(
                Code::Config,
                e,
                format!("reading {}", path.display()),
            ));
        }
    };

    let layer: ConfigLayer = serde_json::from_str(&text)
        .map_err(|e| Error::wrap(Code::Config, e, format!("parsing {}", path.display())))?;
    layer.apply(cfg);
    Ok(())
}

fn apply_env(cfg: &mut Config) {
    for (key, dst) in [
        ("SNOUT_FLAKE", &mut cfg.flake),
        ("SNOUT_HOST", &mut cfg.host),
        ("SNOUT_LOG_LEVEL", &mut cfg.log_level),
    ] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            *dst = v;
        }
    }
}

fn hostname() -> std::io::Result<String> {
    Ok(rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned())
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl Config {
    /// Reports settings that would fail later, at a point where the message can
    /// still name the file and the flag that would fix them.
    pub fn validate(&self) -> Result<()> {
        if self.flake.trim().is_empty() {
            return Err(Error::config(format!(
                "no flake path configured — set \"flake\" in {SYSTEM_PATH} or pass --flake"
            )));
        }
        if !Path::new(&self.flake).is_absolute() {
            return Err(Error::config(format!(
                "flake path {:?} must be absolute",
                self.flake
            )));
        }
        if self.host.is_empty() {
            return Err(Error::config(
                "no host name configured — set \"host\" or pass --host",
            ));
        }
        if self.daemon.interval.is_zero() {
            return Err(Error::config("daemon.interval must be positive"));
        }
        // Checked alongside the others rather than left out: a zero settle delay
        // makes the very first check due at login, which is the one moment the
        // setting exists to avoid.
        if self.daemon.settle_delay.is_zero() {
            return Err(Error::config("daemon.settleDelay must be positive"));
        }
        if self.daemon.backoff.min.is_zero() || self.daemon.backoff.max < self.daemon.backoff.min {
            return Err(Error::config(
                "daemon.backoff.min must be positive and no greater than max",
            ));
        }
        if !valid_log_level(&self.log_level) {
            return Err(Error::config(format!(
                "unknown logLevel {:?} (want {})",
                self.log_level,
                LOG_LEVELS.join(", ")
            )));
        }

        let mut seen: Vec<&str> = Vec::new();
        let mut needs_spawn = false;
        for (i, comp) in self.reload.components.iter().enumerate() {
            if comp.name.is_empty() {
                return Err(Error::config(format!("reload.components[{i}] has no name")));
            }
            if seen.contains(&comp.name.as_str()) {
                return Err(Error::config(format!(
                    "reload component {:?} is listed twice",
                    comp.name
                )));
            }
            seen.push(&comp.name);

            match comp.kind.as_str() {
                KIND_DAEMON => needs_spawn = true,
                KIND_SYSTEMD_USER => {}
                KIND_COMMAND => {
                    if comp.start.is_empty() && comp.stop.is_empty() {
                        return Err(Error::config(format!(
                            "reload component {:?} is kind {KIND_COMMAND:?} but has neither stop nor start",
                            comp.name
                        )));
                    }
                }
                "" => {
                    return Err(Error::config(format!(
                        "reload component {:?} has no kind",
                        comp.name
                    )));
                }
                other => {
                    return Err(Error::config(format!(
                        "reload component {:?} has unknown kind {other:?} (want {KIND_DAEMON}, {KIND_SYSTEMD_USER} or {KIND_COMMAND})",
                        comp.name
                    )));
                }
            }
        }

        if needs_spawn {
            if self.reload.spawn.is_empty() {
                return Err(Error::config(format!(
                    "reload.spawn is required when any component has kind {KIND_DAEMON:?}"
                )));
            }
            if !self.reload.spawn.iter().any(|a| a.contains("{}")) {
                return Err(Error::config(
                    "reload.spawn must contain \"{}\" somewhere, as the placeholder for the program to start",
                ));
            }
        }

        for (i, p) in self.reload.post.iter().enumerate() {
            if p.command.is_empty() {
                return Err(Error::config(format!(
                    "reload.post[{i}] has an empty command"
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Derived paths
// ---------------------------------------------------------------------------

impl Config {
    /// The directory snout keeps its answers in.
    ///
    /// Errors rather than falling back to a relative path: a cache resolved
    /// against the working directory would follow the user around, so several
    /// `snout` invocations would disagree about what had already been checked.
    pub fn cache(&self) -> Result<PathBuf> {
        if !self.cache_dir.is_empty() {
            return Ok(PathBuf::from(&self.cache_dir));
        }
        if let Some(d) = non_empty_env("XDG_CACHE_HOME") {
            return Ok(PathBuf::from(d).join("snout"));
        }
        if let Some(h) = non_empty_env("HOME") {
            return Ok(PathBuf::from(h).join(".cache").join("snout"));
        }
        Err(Error::config(
            "nowhere to keep the cache — set \"cacheDir\", or $XDG_CACHE_HOME, or $HOME",
        ))
    }

    /// Holds the two published answers and the checker's working state.
    pub fn updates_dir(&self) -> Result<PathBuf> {
        Ok(self.cache()?.join("updates"))
    }

    /// Holds the out-link `snout build` produces. Separate from the checker's
    /// own out-link so the two cannot collide; both are GC roots, so neither is
    /// thrown away by a weekly `nix.gc`.
    fn build_dir(&self) -> Result<PathBuf> {
        Ok(self.cache()?.join("build"))
    }

    /// The default target of `snout build`, overridden by `--out-link`.
    pub fn build_link(&self) -> Result<PathBuf> {
        Ok(self.build_dir()?.join("result"))
    }

    /// The flake reference a rebuild operates on.
    pub fn target(&self) -> String {
        format!("{}#{}", self.flake, self.host)
    }

    /// The `nix build` installable for this host's system closure.
    ///
    /// Spelled out rather than going through `nixos-rebuild build`, which has
    /// no `--out-link` in the ng rewrite. `flake_ref` is a parameter because
    /// the checker builds a scratch copy of the flake rather than the
    /// configured one.
    pub fn toplevel_attr(&self, flake_ref: &str) -> String {
        format!(
            "{flake_ref}#nixosConfigurations.{}.config.system.build.toplevel",
            self.host
        )
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn user_config_dir() -> Option<PathBuf> {
    if let Some(d) = non_empty_env("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(d));
    }
    non_empty_env("HOME").map(|h| PathBuf::from(h).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Every environment variable the load path reads, pinned to a scratch
    /// directory. An env layer leaking in from the developer's own shell would
    /// silently override the fixture and make a passing test meaningless.
    struct Isolated {
        _dir: tempfile::TempDir,
        home: PathBuf,
    }

    fn isolate() -> Isolated {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
            std::env::set_var("XDG_RUNTIME_DIR", dir.path().join("run"));
            std::env::set_var("SNOUT_CONFIG", dir.path().join("system.json"));
            std::env::remove_var("SNOUT_FLAKE");
            std::env::remove_var("SNOUT_HOST");
            std::env::remove_var("SNOUT_LOG_LEVEL");
        }
        Isolated { _dir: dir, home }
    }

    impl Isolated {
        fn write_system(&self, body: &str) {
            let p = std::env::var("SNOUT_CONFIG").unwrap();
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        }
        fn write_user(&self, body: &str) {
            let dir = PathBuf::from(std::env::var("XDG_CONFIG_HOME").unwrap()).join("snout");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.json"), body).unwrap();
        }
    }

    use crate::testenv::env_guard as guard;

    #[test]
    fn defaults_leave_the_flake_unset() {
        let c = Config::default();
        assert_eq!(c.flake, "");
        assert_eq!(c.log_level, "info");
        assert!(c.daemon.enable);
        assert_eq!(c.daemon.interval, Duration::from_secs(3600));
        assert_eq!(c.daemon.settle_delay, Duration::from_secs(300));
        assert!(c.reload.hint_after_deploy);
        assert!(!c.reload.after_deploy);
    }

    #[test]
    fn later_layers_win_key_by_key() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(r#"{"flake":"/a","host":"from-system","logLevel":"warn"}"#);
        iso.write_user(r#"{"host":"user"}"#);

        let cfg = load().unwrap();
        // The user layer named only `host`, so the system layer's flake and
        // logLevel survive untouched.
        assert_eq!(cfg.flake, "/a");
        assert_eq!(cfg.host, "user");
        assert_eq!(cfg.log_level, "warn");
    }

    #[test]
    fn a_nested_object_merges_but_an_array_replaces() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(
            r#"{"flake":"/a","daemon":{"interval":"6h","settleDelay":"1m"},
                "reload":{"notes":["one","two"]}}"#,
        );
        iso.write_user(r#"{"daemon":{"interval":"30m"},"reload":{"notes":["only"]}}"#);

        let cfg = load().unwrap();
        assert_eq!(cfg.daemon.interval, Duration::from_secs(1800));
        // settleDelay was not named by the user layer, so it is still the
        // system layer's value rather than the built-in default.
        assert_eq!(cfg.daemon.settle_delay, Duration::from_secs(60));
        // Arrays are not merged element-wise; the later one replaces.
        assert_eq!(cfg.reload.notes, vec!["only"]);
    }

    #[test]
    fn an_explicit_false_overrides_a_default_true() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(
            r#"{"flake":"/a","daemon":{"enable":false},"reload":{"hintAfterDeploy":false}}"#,
        );
        let cfg = load().unwrap();
        assert!(!cfg.daemon.enable);
        assert!(!cfg.reload.hint_after_deploy);
    }

    #[test]
    fn the_environment_layer_wins_over_the_files() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(r#"{"flake":"/from-file","host":"file","logLevel":"warn"}"#);
        unsafe {
            std::env::set_var("SNOUT_FLAKE", "/from-env");
            std::env::set_var("SNOUT_LOG_LEVEL", "debug");
        }
        let cfg = load().unwrap();
        unsafe {
            std::env::remove_var("SNOUT_FLAKE");
            std::env::remove_var("SNOUT_LOG_LEVEL");
        }
        assert_eq!(cfg.flake, "/from-env");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.host, "file");
    }

    #[test]
    fn an_explicitly_named_config_that_is_missing_is_an_error() {
        let _g = guard();
        let _iso = isolate();
        // SNOUT_CONFIG points at a file that was never written.
        let err = load().unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_missing_optional_user_config_is_fine() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(r#"{"flake":"/a"}"#);
        // No user config written at all.
        assert_eq!(load().unwrap().flake, "/a");
    }

    #[test]
    fn unparseable_json_names_the_file() {
        let _g = guard();
        let iso = isolate();
        iso.write_system("{not json");
        let err = load().unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(err.to_string().starts_with("parsing "), "{err}");
    }

    #[test]
    fn the_host_falls_back_to_the_machine_name() {
        let _g = guard();
        let iso = isolate();
        iso.write_system(r#"{"flake":"/a"}"#);
        assert!(!load().unwrap().host.is_empty());
    }

    // -- durations ---------------------------------------------------------

    #[test]
    fn durations_accept_the_forms_a_config_file_actually_contains() {
        for (text, want) in [
            (r#""2h""#, 7200),
            (r#""5m""#, 300),
            (r#""90s""#, 90),
            (r#""1h30m""#, 5400),
            (r#""2h0m0s""#, 7200),
        ] {
            let d: Duration = serde_json::from_str(text).unwrap();
            assert_eq!(d.as_std().as_secs(), want, "{text}");
        }
    }

    #[test]
    fn a_duration_that_is_not_a_string_says_so() {
        let err = serde_json::from_str::<Duration>("7200").unwrap_err();
        assert!(
            err.to_string().contains("must be a string like \"2h\""),
            "{err}"
        );
    }

    #[test]
    fn an_unparseable_duration_quotes_what_was_given() {
        let err = serde_json::from_str::<Duration>(r#""every other tuesday""#).unwrap_err();
        assert!(err.to_string().contains("invalid duration"), "{err}");
    }

    // -- validation --------------------------------------------------------

    fn valid() -> Config {
        Config {
            flake: "/etc/nixos".into(),
            host: "somehost".into(),
            ..Config::default()
        }
    }

    fn rejects(mutate: impl FnOnce(&mut Config), needle: &str) {
        let mut c = valid();
        mutate(&mut c);
        let err = c.validate().unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(
            err.to_string().contains(needle),
            "want {needle:?} in {err:?}"
        );
    }

    #[test]
    fn a_valid_config_validates() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn the_flake_must_be_set_and_absolute() {
        rejects(|c| c.flake = "  ".into(), "no flake path configured");
        rejects(|c| c.flake = "relative/path".into(), "must be absolute");
    }

    #[test]
    fn the_host_must_be_set() {
        rejects(|c| c.host = String::new(), "no host name configured");
    }

    #[test]
    fn the_daemon_timings_must_make_sense() {
        rejects(
            |c| c.daemon.interval = Duration::from_secs(0),
            "daemon.interval must be positive",
        );
        rejects(
            |c| c.daemon.settle_delay = Duration::from_secs(0),
            "daemon.settleDelay must be positive",
        );
        rejects(
            |c| c.daemon.backoff.min = Duration::from_secs(0),
            "daemon.backoff.min",
        );
        rejects(
            |c| {
                c.daemon.backoff.min = Duration::from_secs(3600);
                c.daemon.backoff.max = Duration::from_secs(60);
            },
            "no greater than max",
        );
    }

    #[test]
    fn the_log_level_must_be_one_of_the_four() {
        rejects(|c| c.log_level = "chatty".into(), "unknown logLevel");
    }

    fn comp(name: &str, kind: &str) -> Component {
        Component {
            name: name.into(),
            kind: kind.into(),
            unit: String::new(),
            stop: Vec::new(),
            start: Vec::new(),
        }
    }

    #[test]
    fn reload_components_are_checked() {
        rejects(
            |c| c.reload.components = vec![comp("", "daemon")],
            "reload.components[0] has no name",
        );
        rejects(|c| c.reload.components = vec![comp("a", "")], "has no kind");
        rejects(
            |c| c.reload.components = vec![comp("a", "sorcery")],
            "unknown kind",
        );
        rejects(
            |c| {
                c.reload.spawn = vec!["run".into(), "{}".into()];
                c.reload.components = vec![comp("a", "daemon"), comp("a", "daemon")];
            },
            "is listed twice",
        );
        rejects(
            |c| c.reload.components = vec![comp("a", "command")],
            "neither stop nor start",
        );
    }

    #[test]
    fn a_daemon_component_requires_a_spawn_template_with_a_placeholder() {
        rejects(
            |c| c.reload.components = vec![comp("a", "daemon")],
            "reload.spawn is required",
        );
        rejects(
            |c| {
                c.reload.components = vec![comp("a", "daemon")];
                c.reload.spawn = vec!["launch".into()];
            },
            "must contain \"{}\"",
        );

        // A systemd-user component needs no spawn template at all.
        let mut c = valid();
        c.reload.components = vec![comp("a", "systemd-user")];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn a_post_command_cannot_be_empty() {
        rejects(
            |c| {
                c.reload.post = vec![PostCmd {
                    name: "x".into(),
                    command: Vec::new(),
                }]
            },
            "reload.post[0] has an empty command",
        );
    }

    // -- derived paths -----------------------------------------------------

    #[test]
    fn cache_dir_wins_over_the_environment() {
        let _g = guard();
        let _iso = isolate();
        let mut c = valid();
        c.cache_dir = "/var/cache/snout".into();
        assert_eq!(c.cache().unwrap(), PathBuf::from("/var/cache/snout"));
        assert_eq!(
            c.updates_dir().unwrap(),
            PathBuf::from("/var/cache/snout/updates")
        );
        assert_eq!(
            c.build_link().unwrap(),
            PathBuf::from("/var/cache/snout/build/result")
        );
    }

    #[test]
    fn the_cache_follows_xdg_then_home() {
        let _g = guard();
        let iso = isolate();
        let c = valid();
        let xdg = std::env::var("XDG_CACHE_HOME").unwrap();
        assert_eq!(c.cache().unwrap(), PathBuf::from(xdg).join("snout"));

        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        assert_eq!(c.cache().unwrap(), iso.home.join(".cache/snout"));
    }

    #[test]
    fn a_cache_with_nowhere_to_live_is_an_error_not_a_relative_path() {
        // A path resolved against the working directory would follow the user
        // around, so two invocations in different directories would disagree
        // about what had already been checked.
        let _g = guard();
        let _iso = isolate();
        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
            std::env::set_var("HOME", "");
        }
        let err = valid().cache().unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(
            err.to_string().contains("nowhere to keep the cache"),
            "{err}"
        );
    }

    #[test]
    fn the_target_is_flake_hash_host() {
        assert_eq!(valid().target(), "/etc/nixos#somehost");
    }
}
