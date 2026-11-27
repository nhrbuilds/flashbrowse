//! Anti-fingerprinting scripts and UA/Client-Hints profiles.
//!
//! Injected before any page script runs. Scope:
//! - Strip `navigator.webdriver`
//! - Stub `window.chrome` runtime
//! - Normalize `plugins`/`mimeTypes`, `languages`, `platform`, `vendor`,
//!   `hardwareConcurrency`, `deviceMemory`, `maxTouchPoints`, `webdriver`
//! - Stub `permissions.query` for `notifications`
//! - Mask WebGL renderer/vendor strings
//!
//! Honest limits (do not over-market as "undetectable"): this does not
//! hide `canvas`/`AudioContext` fingerprint, screen/UA entropy, or TLS
//! fingerprinting. It defeats naive bot checks, not determined
//! fingerprinting. See README § Limitations.

use std::fmt;
use std::str::FromStr;

use serde_json::{json, Value};

/// Configurable browser fingerprint profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StealthProfile {
    /// Keep the host OS/architecture in navigator.platform, User-Agent, and Client Hints.
    #[default]
    HostNative,
    /// Present a coherent Windows x86_64 desktop profile across UA, platform, and Client Hints.
    Windows,
}

impl StealthProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostNative => "host-native",
            Self::Windows => "windows",
        }
    }

    pub fn navigator_platform(self) -> &'static str {
        match self {
            Self::Windows => "Win32",
            Self::HostNative => match (std::env::consts::OS, std::env::consts::ARCH) {
                ("macos", _) => "MacIntel",
                ("windows", _) => "Win32",
                ("linux", "aarch64") => "Linux aarch64",
                _ => "Linux x86_64",
            },
        }
    }
}

impl fmt::Display for StealthProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StealthProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "host-native" => Ok(Self::HostNative),
            "windows" => Ok(Self::Windows),
            other => Err(format!(
                "invalid stealth profile '{other}' (expected 'host-native' or 'windows')"
            )),
        }
    }
}

/// Build the stealth init script for the selected profile.
pub fn stealth_script(profile: StealthProfile) -> String {
    let platform = profile.navigator_platform();
    STEALTH_SCRIPT.replace(
        "Object.defineProperty(navigator, 'platform', { configurable: true, get: () => 'Win32' });",
        &format!(
            "Object.defineProperty(navigator, 'platform', {{ configurable: true, get: () => '{platform}' }});"
        ),
    )
}

/// Build a coherent `Emulation.setUserAgentOverride` payload from `Browser.getVersion().userAgent`.
pub fn build_ua_override(raw_ua: &str, profile: StealthProfile) -> Value {
    let cleaned = raw_ua.replace("HeadlessChrome/", "Chrome/");
    let ua = match profile {
        StealthProfile::HostNative => cleaned,
        StealthProfile::Windows => rewrite_ua_os(&cleaned, "Windows NT 10.0; Win64; x64"),
    };
    let full_version = extract_chrome_version(&ua).unwrap_or("131.0.0.0");
    let major = full_version.split('.').next().unwrap_or("131");
    let (ch_platform, ch_platform_version, arch) = match profile {
        StealthProfile::Windows => ("Windows", "10.0.0", "x86"),
        StealthProfile::HostNative => {
            let arch = if std::env::consts::ARCH == "aarch64" {
                "arm"
            } else {
                "x86"
            };
            match std::env::consts::OS {
                "macos" => ("macOS", "14.0.0", arch),
                "windows" => ("Windows", "10.0.0", arch),
                _ => ("Linux", "6.0.0", arch),
            }
        }
    };
    let brands = json!([
        {"brand": "Chromium", "version": major},
        {"brand": "Google Chrome", "version": major},
        {"brand": "Not_A Brand", "version": "24"}
    ]);
    let full_version_list = json!([
        {"brand": "Chromium", "version": full_version},
        {"brand": "Google Chrome", "version": full_version},
        {"brand": "Not_A Brand", "version": "24.0.0.0"}
    ]);
    json!({
        "userAgent": ua,
        "platform": profile.navigator_platform(),
        "userAgentMetadata": {
            "brands": brands,
            "fullVersionList": full_version_list,
            "platform": ch_platform,
            "platformVersion": ch_platform_version,
            "architecture": arch,
            "model": "",
            "mobile": false,
            "bitness": "64",
            "wow64": false
        }
    })
}

fn extract_chrome_version(ua: &str) -> Option<&str> {
    let after = ua.split("Chrome/").nth(1)?;
    let ver = after.split_whitespace().next()?;
    if ver.is_empty() {
        None
    } else {
        Some(ver)
    }
}

fn rewrite_ua_os(ua: &str, target_os: &str) -> String {
    if let Some((prefix, suffix)) = ua.split_once("Mozilla/5.0 (") {
        if let Some((_, tail)) = suffix.split_once(')') {
            return format!("{prefix}Mozilla/5.0 ({target_os}){tail}");
        }
    }
    ua.to_string()
}

/// Full stealth payload - passed as `Page.addScriptToEvaluateOnNewDocument { source }`.
pub const STEALTH_SCRIPT: &str = r#"(() => {
  // 1. webdriver: try to remove, fall back to undefined. Each block is
  // isolated so one failure cannot disable the rest (H9/M9).
  try {
    delete navigator.webdriver;
    if (navigator.webdriver === undefined) { /* gone - nothing more */ }
    Object.defineProperty(navigator, 'webdriver', { configurable: true, get: () => undefined });
  } catch (_) {}

  // 2. Mock window.chrome runtime
  try {
    if (!window.chrome) {
      window.chrome = {
        app: { isInstalled: false, InstallState: { DISABLED: 'disabled' }, RunningState: { CANNOT_RUN: 'cannot_run' } },
        runtime: { OnInstalledReason: {}, OnRestartRequiredReason: {}, PlatformArch: {}, PlatformNaclArch: {}, PlatformOs: {}, RequestUpdateCheckStatus: {} },
        loadTimes: () => {},
        csi: () => {}
      };
    }
  } catch (_) {}

  // 3. Normalize plugins / mimeTypes (best-effort PluginArray shape)
  try {
    const plugins = Object.freeze([
      { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
      { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbpisiamfdolgcggjagofn', description: '' }
    ]);
    const mimeTypes = Object.freeze([
      { type: 'application/pdf', suffixes: 'pdf', description: 'Portable Document Format' }
    ]);
    Object.defineProperty(navigator, 'plugins', { configurable: true, get: () => plugins });
    Object.defineProperty(navigator, 'mimeTypes', { configurable: true, get: () => mimeTypes });
  } catch (_) {}

  // 4. Normalize languages / locale / platform / vendor / CPU signals
  try {
    Object.defineProperty(navigator, 'languages', { configurable: true, get: () => ['en-US', 'en'] });
    Object.defineProperty(navigator, 'language', { configurable: true, get: () => 'en-US' });
    Object.defineProperty(navigator, 'platform', { configurable: true, get: () => 'Win32' });
    Object.defineProperty(navigator, 'vendor', { configurable: true, get: () => 'Google Inc.' });
    Object.defineProperty(navigator, 'hardwareConcurrency', { configurable: true, get: () => 8 });
    if ('deviceMemory' in navigator) {
      Object.defineProperty(navigator, 'deviceMemory', { configurable: true, get: () => 8 });
    }
    Object.defineProperty(navigator, 'maxTouchPoints', { configurable: true, get: () => 0 });
  } catch (_) {}

  // 5. Permissions query (bound receiver - fixes Illegal Invocation)
  try {
    const originalQuery = window.navigator.permissions.query.bind(window.navigator.permissions);
    window.navigator.permissions.query = (parameters) => (
      parameters.name === 'notifications' && window.Notification ?
        Promise.resolve({ state: Notification.permission }) :
        originalQuery(parameters)
    );
  } catch (_) {}

  // 6. Normalize WebGL renderer/vendor (best-effort string masking)
  try {
    const getParameter = WebGLRenderingContext.prototype.getParameter;
    WebGLRenderingContext.prototype.getParameter = function(parameter) {
      if (parameter === 37445) return 'Google Inc.';
      if (parameter === 37446) return 'ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0.0000000059604645)))';
      return getParameter.call(this, parameter);
    };
  } catch (_) {}

})();
"#;

/// Additional page instrumentation used only by the settle shadow benchmark.
pub const SETTLE_DIAGNOSTICS_SCRIPT: &str = r#"(() => {
  try {
    const state = { count: 0, last_ms: 0 };
    Object.defineProperty(window, '__flashbrowseMutationState', {
      configurable: false,
      enumerable: false,
      value: state
    });
    const observer = new MutationObserver((records) => {
      state.count += records.length;
      state.last_ms = performance.now();
      for (const rec of records) {
        for (const node of rec.addedNodes || []) {
          if (node && node.nodeType === 1) observeTree(node);
        }
      }
    });
    const observeOpts = {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true
    };
    const observed = new WeakSet();
    const observeRoot = (root) => {
      if (!root || observed.has(root)) return;
      observed.add(root);
      observer.observe(root, observeOpts);
      for (const el of root.querySelectorAll('*')) {
        if (el.shadowRoot) observeRoot(el.shadowRoot);
      }
    };
    const observeTree = (el) => {
      if (el.shadowRoot) observeRoot(el.shadowRoot);
      if (el.querySelectorAll) {
        for (const child of el.querySelectorAll('*')) {
          if (child.shadowRoot) observeRoot(child.shadowRoot);
        }
      }
    };
    const origAttachShadow = Element.prototype.attachShadow;
    Element.prototype.attachShadow = function(init) {
      const root = origAttachShadow.call(this, init);
      if (init && init.mode === 'open') observeRoot(root);
      return root;
    };
    observeRoot(document);
  } catch (_) {}
})();
"#;

#[cfg(test)]
mod tests {
    use super::{SETTLE_DIAGNOSTICS_SCRIPT, STEALTH_SCRIPT};

    #[test]
    fn script_contains_key_evasions() {
        for needle in [
            "navigator, 'webdriver'",
            "window.chrome",
            "navigator, 'plugins'",
            "navigator, 'languages'",
            "permissions.query",
            "getParameter",
        ] {
            assert!(
                STEALTH_SCRIPT.contains(needle),
                "stealth script missing: {needle}"
            );
        }
    }

    #[test]
    fn mutation_observer_is_separate_from_default_stealth_script() {
        assert!(!STEALTH_SCRIPT.contains("MutationObserver"));
        assert!(SETTLE_DIAGNOSTICS_SCRIPT.contains("MutationObserver"));
        assert!(SETTLE_DIAGNOSTICS_SCRIPT.contains("__flashbrowseMutationState"));
    }

    #[test]
    fn stealth_profile_parses_and_builds_coherent_ua_without_headless_chrome() {
        use super::{build_ua_override, stealth_script, StealthProfile};

        assert_eq!(
            "host-native".parse::<StealthProfile>().unwrap(),
            StealthProfile::HostNative
        );
        assert_eq!(
            "windows".parse::<StealthProfile>().unwrap(),
            StealthProfile::Windows
        );
        assert!("bogus".parse::<StealthProfile>().is_err());

        let raw_ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/131.0.6778.0 Safari/537.36";
        let win = build_ua_override(raw_ua, StealthProfile::Windows);
        let win_ua = win["userAgent"].as_str().unwrap();
        assert!(!win_ua.contains("HeadlessChrome"));
        assert!(win_ua.contains("Windows NT 10.0; Win64; x64"));
        assert_eq!(win["platform"], "Win32");
        assert_eq!(win["userAgentMetadata"]["platform"], "Windows");
        assert!(stealth_script(StealthProfile::Windows).contains("'Win32'"));

        let native = build_ua_override(raw_ua, StealthProfile::HostNative);
        let native_ua = native["userAgent"].as_str().unwrap();
        assert!(!native_ua.contains("HeadlessChrome"));
        assert_eq!(
            native["platform"],
            StealthProfile::HostNative.navigator_platform()
        );
        assert!(stealth_script(StealthProfile::HostNative)
            .contains(StealthProfile::HostNative.navigator_platform()));
    }
}
