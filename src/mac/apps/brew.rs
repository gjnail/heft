//! Updates through Homebrew, the Mac's counterpart of winget.
//!
//! Which apps Homebrew installed is read from its own `Caskroom` folder, so
//! listing apps never runs `brew` (and never touches the network). Heft only
//! runs `brew` when you check for updates. Upgrades and uninstalls run in a
//! Terminal window, so Homebrew's prompts and password requests are shown to
//! the user rather than answered silently.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::mac::sh_quote;

/// Where Homebrew is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brew {
    pub exe: PathBuf,
    pub prefix: PathBuf,
}

/// Homebrew's standard prefixes: Apple silicon, then Intel.
pub fn find() -> Option<Brew> {
    ["/opt/homebrew", "/usr/local"].iter().map(PathBuf::from).find_map(|prefix| {
        let exe = prefix.join("bin/brew");
        exe.is_file().then_some(Brew { exe, prefix })
    })
}

/// An installed cask and the apps it put in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cask {
    pub token: String,
    /// App targets: absolute paths, or names relative to the Applications
    /// folder Homebrew was set to use (`Firefox.app`).
    pub apps: Vec<String>,
}

impl Cask {
    /// Whether the app at `path` (or its link) is one this cask installed.
    pub fn owns(&self, path: &Path, link: Option<&Path>) -> bool {
        self.apps.iter().any(|t| {
            let t = Path::new(t);
            [Some(path), link].into_iter().flatten().any(|p| {
                if t.is_absolute() {
                    crate::platform::names_eq(&p.to_string_lossy(), &t.to_string_lossy())
                } else {
                    let p = p.to_string_lossy().to_lowercase();
                    let t = t.to_string_lossy().to_lowercase();
                    p.ends_with(&format!("/{t}"))
                }
            })
        })
    }
}

/// The cask that installed an app, if any.
pub fn cask_for<'a>(casks: &'a [Cask], path: &Path, link: Option<&Path>) -> Option<&'a Cask> {
    casks.iter().find(|c| c.owns(path, link))
}

/// Installed casks, read from the Caskroom without running `brew`.
pub fn installed_casks() -> Vec<Cask> {
    find().map(|b| casks_in(&b.prefix.join("Caskroom"))).unwrap_or_default()
}

/// Each `Caskroom/<token>` folder, with the apps from its install receipt
/// (Homebrew 4.3 and later) or else from the saved cask file.
fn casks_in(caskroom: &Path) -> Vec<Cask> {
    let Ok(rd) = std::fs::read_dir(caskroom) else { return Vec::new() };
    let mut out: Vec<Cask> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let token = e.file_name().to_string_lossy().into_owned();
            if token.starts_with('.') {
                return None;
            }
            let meta = e.path().join(".metadata");
            let from_receipt = std::fs::read_to_string(meta.join("INSTALL_RECEIPT.json"))
                .ok()
                .and_then(|t| Json::parse(&t).ok())
                .map(|j| app_targets(j.get("uninstall_artifacts")))
                .filter(|a| !a.is_empty());
            let apps = from_receipt.or_else(|| caskfile_apps(&meta, &token)).unwrap_or_default();
            Some(Cask { token, apps })
        })
        .collect();
    out.sort_by(|a, b| a.token.cmp(&b.token));
    out
}

/// Apps named in the newest saved cask file:
/// `.metadata/<version>/<timestamp>/Casks/<token>.json` or `.rb`.
fn caskfile_apps(meta: &Path, token: &str) -> Option<Vec<String>> {
    let newest = std::fs::read_dir(meta)
        .ok()?
        .flatten()
        .filter(|v| v.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|v| std::fs::read_dir(v.path()).into_iter().flatten().flatten())
        .map(|t| t.path())
        .filter(|t| t.is_dir())
        .max_by_key(|t| t.file_name().map(|n| n.to_os_string()))?;
    let casks = newest.join("Casks");
    if let Ok(text) = std::fs::read_to_string(casks.join(format!("{token}.json"))) {
        return Json::parse(&text).ok().map(|j| app_targets(j.get("artifacts")));
    }
    std::fs::read_to_string(casks.join(format!("{token}.rb"))).ok().map(|t| rb_apps(&t))
}

/// App targets from a list of cask artifacts, as `brew info --json=v2` and
/// the install receipt write them: `{"app": ["Foo.app"], "target": "/Applications/Foo.app"}`
/// (newer), `{"app": ["Foo.app", {"target": "Bar.app"}]}` (renamed), or
/// `{"app": ["Foo.app"]}`.
fn app_targets(artifacts: Option<&Json>) -> Vec<String> {
    let mut out = Vec::new();
    for a in artifacts.and_then(Json::as_array).unwrap_or(&[]) {
        let Some(args) = a.get("app").and_then(Json::as_array) else { continue };
        let explicit = a.get("target").and_then(Json::as_str).or_else(|| {
            args.iter().find_map(|x| x.get("target").and_then(Json::as_str))
        });
        let target = match explicit {
            Some(t) => t.to_string(),
            // The source can be a path inside the download; it's installed
            // under its own name.
            None => match args.first().and_then(Json::as_str) {
                Some(s) => s.rsplit('/').next().unwrap_or(s).to_string(),
                None => continue,
            },
        };
        if !target.is_empty() && !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// App targets from a Ruby cask file: `app "Foo.app"` or
/// `app "Foo.app", target: "Bar.app"`.
fn rb_apps(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("app ") else { continue };
        let quoted: Vec<&str> = rest.split(['"', '\'']).skip(1).step_by(2).collect();
        let Some(source) = quoted.first() else { continue };
        let target = if rest.contains("target:") && quoted.len() > 1 {
            quoted[1].to_string()
        } else {
            source.rsplit('/').next().unwrap_or(source).to_string()
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// A cask or formula with a newer version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upgrade {
    /// Cask token or formula name.
    pub name: String,
    pub cask: bool,
    pub installed: String,
    pub available: String,
    /// Pinned formulae are skipped by `brew upgrade`.
    pub pinned: bool,
}

/// `brew outdated --json=v2`: `{"formulae": [...], "casks": [...]}`, each
/// with `name`, `installed_versions`, `current_version` and `pinned`.
pub fn parse_outdated(text: &str) -> Result<Vec<Upgrade>, String> {
    let j = Json::parse(text)?;
    let mut out = Vec::new();
    for (key, cask) in [("casks", true), ("formulae", false)] {
        for e in j.get(key).and_then(Json::as_array).unwrap_or(&[]) {
            let Some(name) = e.get("name").and_then(Json::as_str) else { continue };
            let installed = e
                .get("installed_versions")
                .and_then(Json::as_array)
                .and_then(|v| v.last())
                .and_then(Json::as_str)
                .unwrap_or_default();
            out.push(Upgrade {
                name: name.to_string(),
                cask,
                installed: installed.to_string(),
                available: e.get("current_version").and_then(Json::as_str).unwrap_or_default().to_string(),
                pinned: e.get("pinned").and_then(Json::as_bool).unwrap_or(false),
            });
        }
    }
    Ok(out)
}

/// `brew info --cask --json=v2 --installed`: the installed casks and their apps.
pub fn parse_info(text: &str) -> Result<Vec<Cask>, String> {
    let j = Json::parse(text)?;
    Ok(j.get("casks")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|c| {
            let token = c.get("token").and_then(Json::as_str)?;
            Some(Cask { token: token.to_string(), apps: app_targets(c.get("artifacts")) })
        })
        .collect())
}

/// Run `brew` quietly and return what it printed, or its own last error line.
fn run(brew: &Brew, args: &[&str], auto_update: bool) -> Result<String, String> {
    let mut cmd = Command::new(&brew.exe);
    cmd.args(args).env("HOMEBREW_NO_COLOR", "1").env("HOMEBREW_NO_ENV_HINTS", "1");
    if !auto_update {
        cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    }
    let out = cmd.output().map_err(|e| format!("Homebrew couldn't be started: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() || stdout.trim_start().starts_with('{') {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("Homebrew failed");
    Err(last.trim_start_matches("Error: ").to_string())
}

/// Ask Homebrew what can be updated, including casks that update themselves
/// (`--greedy`), and which apps its casks installed. `brew outdated` refreshes
/// Homebrew's package lists first when they're old, which is the only network
/// access Heft does.
pub fn check(brew: &Brew) -> Result<(Vec<Upgrade>, Vec<Cask>), String> {
    let upgrades = parse_outdated(&run(brew, &["outdated", "--greedy", "--json=v2"], true)?)?;
    // The Caskroom already said which apps are casks; this only refines it.
    let casks = run(brew, &["info", "--cask", "--json=v2", "--installed"], false)
        .and_then(|t| parse_info(&t))
        .unwrap_or_default();
    Ok((upgrades, casks))
}

/// The command that upgrades one package. Naming a cask upgrades it even if
/// it updates itself.
pub fn upgrade_command(brew: &Brew, u: &Upgrade) -> String {
    let kind = if u.cask { " --cask" } else { "" };
    format!("{} upgrade{kind} {}", sh_quote(&brew.exe.to_string_lossy()), sh_quote(&u.name))
}

pub fn upgrade(brew: &Brew, u: &Upgrade) -> Result<(), String> {
    crate::mac::run_in_terminal("Heft - updating", &upgrade_command(brew, u))
}

pub fn upgrade_all(brew: &Brew) -> Result<(), String> {
    crate::mac::run_in_terminal("Heft - updating", &format!("{} upgrade --greedy", sh_quote(&brew.exe.to_string_lossy())))
}

pub fn uninstall_command(brew: &Brew, token: &str) -> String {
    format!("{} uninstall --cask {}", sh_quote(&brew.exe.to_string_lossy()), sh_quote(token))
}

/// Uninstall a cask in Terminal. This also runs the cask's own uninstall
/// steps (quitting it, removing its helpers and package receipts).
pub fn uninstall(brew: &Brew, token: &str) -> Result<(), String> {
    crate::mac::run_in_terminal("Heft - uninstalling", &uninstall_command(brew, token))
}

/// Versions like `131.0,20240926` carry a build after the comma.
pub fn short_version(v: &str) -> &str {
    v.split(',').next().unwrap_or(v)
}

// ----------------------------------------------------------------------
// A small JSON reader, enough for Homebrew's output.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser { s: text.as_bytes(), i: 0, depth: 0 };
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(p.error("unexpected text after the value"));
        }
        Ok(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self { Some(s) } else { None }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        if let Json::Array(a) = self { Some(a) } else { None }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self { Some(*b) } else { None }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("Homebrew's output couldn't be read ({what} at byte {})", self.i)
    }

    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') | Some(b'[') => {
                self.depth += 1;
                if self.depth > 64 {
                    return Err(self.error("nested too deeply"));
                }
                let v = if self.s[self.i] == b'{' { self.object() } else { self.array() };
                self.depth -= 1;
                v
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') if self.eat("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Json::Null),
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || b"+-.eE".contains(c)) {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.s[start..self.i]).unwrap_or_default();
                t.parse().map(Json::Num).map_err(|_| self.error("bad number"))
            }
            _ => Err(self.error("expected a value")),
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat("]") {
            return Ok(Json::Array(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            if self.eat(",") {
                continue;
            }
            if self.eat("]") {
                return Ok(Json::Array(out));
            }
            return Err(self.error("expected , or ]"));
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat("}") {
            return Ok(Json::Object(out));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.error("expected a key"));
            }
            let k = self.string()?;
            self.ws();
            if !self.eat(":") {
                return Err(self.error("expected :"));
            }
            out.push((k, self.value()?));
            self.ws();
            if self.eat(",") {
                continue;
            }
            if self.eat("}") {
                return Ok(Json::Object(out));
            }
            return Err(self.error("expected , or }"));
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok());
        let v = h.and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.error("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.i) else { return Err(self.error("unterminated string")) };
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| self.error("invalid UTF-8")),
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else { return Err(self.error("unterminated string")) };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) && self.eat("\\u") {
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF)
                            } else {
                                hi
                            };
                            char::from_u32(code).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err(self.error("bad escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like `brew outdated --greedy --json=v2` (Homebrew 4.x).
    const OUTDATED: &str = r#"{
  "formulae": [
    {
      "name": "openssl@3",
      "installed_versions": [
        "3.3.1"
      ],
      "current_version": "3.3.2",
      "pinned": false,
      "pinned_version": null
    },
    {
      "name": "node",
      "installed_versions": ["22.1.0", "22.2.0"],
      "current_version": "22.9.0",
      "pinned": true,
      "pinned_version": "22.2.0"
    }
  ],
  "casks": [
    {
      "name": "firefox",
      "installed_versions": [
        "130.0"
      ],
      "current_version": "131.0",
      "pinned": false,
      "pinned_version": null
    },
    {
      "name": "google-chrome",
      "installed_versions": ["latest"],
      "current_version": "latest"
    },
    {
      "name": "visual-studio-code",
      "installed_versions": ["1.93.0"],
      "current_version": "1.94.2,9f2c1bb42b0f7b45f5c9d8a3d4e1c6a7b8c9d0e1"
    }
  ]
}
"#;

    /// Shaped like `brew info --cask --json=v2 --installed`: current Homebrew
    /// adds the absolute `target`, older versions only list the app, and a
    /// renamed app carries `{"target": ...}` in its arguments.
    const INFO: &str = r#"{
  "formulae": [],
  "casks": [
    {
      "token": "firefox",
      "full_token": "firefox",
      "old_tokens": [],
      "tap": "homebrew/cask",
      "name": ["Mozilla Firefox"],
      "desc": "Web browser",
      "homepage": "https://www.mozilla.org/firefox/",
      "url": "https://download-installer.cdn.mozilla.net/pub/firefox/releases/131.0/mac/en-US/Firefox%20131.0.dmg",
      "url_specs": {},
      "version": "131.0",
      "installed": "130.0",
      "installed_time": 1726061130,
      "bundle_version": "13024.9.2",
      "bundle_short_version": "130.0",
      "outdated": true,
      "sha256": "a2b3c4d5e6f7",
      "artifacts": [
        {"uninstall": [{"quit": "org.mozilla.firefox"}]},
        {"app": ["Firefox.app"], "target": "/Applications/Firefox.app"},
        {"binary": ["/Applications/Firefox.app/Contents/MacOS/firefox"], "target": "/opt/homebrew/bin/firefox"},
        {"zap": [{"trash": ["~/Library/Application Support/Firefox", "~/Library/Caches/Firefox"]}]}
      ],
      "caveats": null,
      "depends_on": {"macos": {">=": ["10.15"]}},
      "conflicts_with": {"cask": ["firefox@beta"]},
      "container": null,
      "auto_updates": true,
      "deprecated": false,
      "deprecation_date": null,
      "languages": ["af", "ar", "zh-TW"]
    },
    {
      "token": "visual-studio-code",
      "name": ["Microsoft Visual Studio Code", "VS Code"],
      "installed": "1.93.0",
      "artifacts": [
        {"app": ["Visual Studio Code.app"]},
        {"binary": ["{{appdir}}/Visual Studio Code.app/Contents/Resources/app/bin/code"]}
      ]
    },
    {
      "token": "some-renamed",
      "artifacts": [
        {"app": ["Some Vendor/Some App.app", {"target": "Some App (Homebrew).app"}]},
        {"pkg": ["SomeHelper.pkg"]}
      ]
    },
    {
      "token": "font-fira-code",
      "artifacts": [{"font": ["FiraCode-Regular.ttf"]}]
    }
  ]
}"#;

    #[test]
    fn outdated_packages() {
        let u = parse_outdated(OUTDATED).unwrap();
        assert_eq!(u.len(), 5, "{u:#?}");
        assert_eq!(
            u[0],
            Upgrade { name: "firefox".into(), cask: true, installed: "130.0".into(), available: "131.0".into(), pinned: false }
        );
        assert_eq!(u[1].installed, "latest");
        assert_eq!(short_version(&u[2].available), "1.94.2");
        let node = u.iter().find(|u| u.name == "node").unwrap();
        assert!(!node.cask && node.pinned);
        assert_eq!(node.installed, "22.2.0", "the newest installed version counts");
        assert!(parse_outdated(r#"{"formulae": [], "casks": []}"#).unwrap().is_empty());
        assert!(parse_outdated("Error: Permission denied").is_err());
    }

    #[test]
    fn installed_cask_apps() {
        let c = parse_info(INFO).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], Cask { token: "firefox".into(), apps: vec!["/Applications/Firefox.app".into()] });
        assert_eq!(c[1].apps, ["Visual Studio Code.app"]);
        assert_eq!(c[2].apps, ["Some App (Homebrew).app"]);
        assert!(c[3].apps.is_empty());

        let p = |s: &str| PathBuf::from(s);
        assert_eq!(cask_for(&c, &p("/Applications/Firefox.app"), None).map(|c| c.token.as_str()), Some("firefox"));
        assert_eq!(
            cask_for(&c, &p("/Users/me/Applications/Visual Studio Code.app"), None).map(|c| c.token.as_str()),
            Some("visual-studio-code")
        );
        assert!(cask_for(&c, &p("/Applications/Firefox Developer Edition.app"), None).is_none());
        assert!(cask_for(&c, &p("/Applications/Old Firefox.app"), None).is_none());
        // The Applications link of an app that lives elsewhere.
        assert!(cask_for(&c, &p("/Users/Shared/Firefox.app"), Some(&p("/Applications/Firefox.app"))).is_some());
    }

    #[test]
    fn ruby_cask_files() {
        let rb = r##"cask "foo" do
  version "1.2.3"
  app "Foo.app"
  app "Bar/Bar Helper.app"
  app 'Baz.app', target: "Baz Renamed.app"
  binary "#{appdir}/Foo.app/Contents/MacOS/foo"
  uninstall_postflight do
    system_command "/usr/bin/true"
  end
end
"##;
        assert_eq!(rb_apps(rb), ["Foo.app", "Bar Helper.app", "Baz Renamed.app"]);
    }

    #[test]
    fn reads_the_caskroom() {
        let base = std::env::temp_dir().join(format!("heft-caskroom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Newer installs: a receipt with the uninstall artifacts.
        let ff = base.join("firefox/.metadata");
        std::fs::create_dir_all(ff.join("131.0/20240926101010.123/Casks")).unwrap();
        std::fs::write(
            ff.join("INSTALL_RECEIPT.json"),
            r#"{"homebrew_version":"4.4.0","loaded_from_api":true,"uninstall_flight_blocks":false,
               "installed_on_request":true,"time":1727345410,"runtime_dependencies":[],
               "source":{"path":"/opt/homebrew/Library/Taps/homebrew/homebrew-cask/Casks/f/firefox.rb","tap":"homebrew/cask","version":"131.0"},
               "arch":"arm64","uninstall_artifacts":[{"uninstall":[{"quit":"org.mozilla.firefox"}]},{"app":["Firefox.app"]},{"zap":[{"trash":["~/Library/Caches/Firefox"]}]}],
               "built_on":{"os":"Macintosh","os_version":"macOS 15.0"}}"#,
        )
        .unwrap();
        // Older installs: only the saved cask file, the newest timestamp wins.
        let old = base.join("oldapp/.metadata");
        std::fs::create_dir_all(old.join("1.0/20200101000000.000/Casks")).unwrap();
        std::fs::create_dir_all(old.join("2.0/20230101000000.000/Casks")).unwrap();
        std::fs::write(old.join("1.0/20200101000000.000/Casks/oldapp.rb"), "cask \"oldapp\" do\n  app \"Ancient.app\"\nend\n").unwrap();
        std::fs::write(
            old.join("2.0/20230101000000.000/Casks/oldapp.json"),
            r#"{"token":"oldapp","artifacts":[{"app":["Old App.app"]}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(base.join(".hidden")).unwrap();
        let c = casks_in(&base);
        assert_eq!(
            c,
            [
                Cask { token: "firefox".into(), apps: vec!["Firefox.app".into()] },
                Cask { token: "oldapp".into(), apps: vec!["Old App.app".into()] },
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn commands_are_quoted() {
        let b = Brew { exe: "/opt/homebrew/bin/brew".into(), prefix: "/opt/homebrew".into() };
        let cask = Upgrade { name: "firefox".into(), cask: true, installed: String::new(), available: String::new(), pinned: false };
        assert_eq!(upgrade_command(&b, &cask), "'/opt/homebrew/bin/brew' upgrade --cask 'firefox'");
        let formula = Upgrade { name: "openssl@3".into(), cask: false, ..cask };
        assert_eq!(upgrade_command(&b, &formula), "'/opt/homebrew/bin/brew' upgrade 'openssl@3'");
        assert_eq!(uninstall_command(&b, "it's"), r"'/opt/homebrew/bin/brew' uninstall --cask 'it'\''s'");
    }

    #[test]
    fn json_values() {
        let j = Json::parse(r#" {"a": [1, -2.5e3, true, false, null], "s": "x\"\\\/\n\u00e9\ud83d\ude00", "o": {}} "#).unwrap();
        assert_eq!(
            j.get("a"),
            Some(&Json::Array(vec![Json::Num(1.0), Json::Num(-2500.0), Json::Bool(true), Json::Bool(false), Json::Null]))
        );
        assert_eq!(j.get("s").and_then(Json::as_str), Some("x\"\\/\né😀"));
        assert_eq!(j.get("o"), Some(&Json::Object(Vec::new())));
        assert_eq!(Json::parse("\"日本\"").unwrap(), Json::Str("日本".into()));
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "\"abc", "tru", "[1] x", "{\"a\":\"\\q\"}"] {
            assert!(Json::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Json::parse(&"[".repeat(10_000)).is_err(), "deep nesting is refused, not a stack overflow");
    }
}
