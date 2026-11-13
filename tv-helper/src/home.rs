use crate::{
    APP, Result, accepted, files,
    system::{self, System},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

const STATE: &str = "/var/lib/astro-home";
const CONFIG: &str = "/var/lib/astro-home/state.json";
const STAGE: &str = "/var/lib/astro-home/app";
const HOOK: &str = "/var/lib/webosbrew/init.d/64-astro-home";
/// Astro starts as Home that have not become healthy yet. tmpfs: a reboot clears it, and with it
/// any fallback to LG Home.
pub const FAILURES: &str = "/var/run/astro-home-failures";
/// Consecutive unhealthy starts after which the next start gives LG Home back until reboot.
pub const FAILURE_LIMIT: u32 = 3;
pub const HOME: &str = "com.webos.app.home";
fn default_home() -> String {
    HOME.into()
}
fn default_method() -> String {
    "manifest".into()
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    #[serde(default = "default_method")]
    pub method: String,
    pub app_dir: String,
    pub stock: Value,
    #[serde(default = "default_home")]
    pub home_id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
impl Config {
    fn validate(&self) -> Result<()> {
        if self.method != "manifest" {
            return Err("Invalid Home method".into());
        }
        target(&self.home_id)?;
        if !Path::new(&self.app_dir).is_absolute()
            || Path::new(&self.app_dir)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("Invalid installed directory".into());
        }
        if self.stock["id"] != self.home_id {
            return Err("Saved stock Home identity does not match target".into());
        }
        Ok(())
    }
}
fn target(id: &str) -> Result<()> {
    if id == HOME {
        Ok(())
    } else {
        Err("Unsupported Home target".into())
    }
}
pub struct Home<'a, S: System> {
    system: &'a S,
    id: String,
    target: String,
}
impl<'a, S: System> Home<'a, S> {
    pub fn new(system: &'a S, id: &str) -> Result<Self> {
        target(id)?;
        Ok(Self {
            system,
            id: id.into(),
            target: format!("/usr/palm/applications/{id}"),
        })
    }
    fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        system::read(self.system, path, 2 * 1024 * 1024)
    }
    fn exists(&self, path: impl AsRef<Path>) -> bool {
        system::exists(self.system, path)
    }
    fn luna(&self, method: &str, payload: Value) -> Result<Value> {
        system::luna(
            self.system,
            "com.webos.applicationManager",
            method,
            &payload,
        )
    }
    fn foreground(&self) -> Result<String> {
        accepted(self.luna("getForegroundAppInfo", json!({}))?)?["appId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Invalid foreground app".into())
    }
    fn mounted(&self) -> bool {
        system::same_file(self.system, STAGE, &self.target)
    }
    fn mounts(&self) -> Result<Vec<(String, String)>> {
        Ok(String::from_utf8(self.read("/proc/self/mountinfo")?)
            .map_err(|e| e.to_string())?
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.len() > 4).then(|| (unescape(fields[3]), unescape(fields[4])))
            })
            .collect())
    }
    fn foreign_mount(&self) -> Result<bool> {
        Ok(!self.mounted()
            && self
                .mounts()?
                .iter()
                .any(|(_, p)| p == &self.target || p.starts_with(&format!("{}/", self.target))))
    }
    fn unmount(&self, path: &str) -> Result<()> {
        if let Err(error) = system::command(self.system, "/bin/umount", &[path]) {
            eprintln!("Astro Home: {error}; detaching mount");
            system::command(self.system, "/bin/umount", &["-l", path])?;
        }
        Ok(())
    }
    fn stop_home(&self, app: &str) -> Result<()> {
        let mut stopped = vec![];
        for entry in fs::read_dir(self.system.path(Path::new("/proc")))
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
            else {
                continue;
            };
            let proc = format!("/proc/{pid}");
            let Ok(bytes) = self.read(format!("{proc}/cmdline")) else {
                continue;
            };
            let args: Vec<_> = bytes
                .split(|b| *b == 0)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .collect();
            let executable =
                fs::read_link(self.system.path(Path::new(&format!("{proc}/exe")))).ok();
            if home_process(&args, executable.as_deref(), app, &self.id, self.system) {
                self.system.terminate(pid)?;
                stopped.push(proc);
            }
        }
        // The same 5 s budget in short steps: SIGTERM usually takes well under a second.
        for _ in 0..25 {
            if stopped.iter().all(|p| !self.exists(p)) {
                return Ok(());
            }
            self.system.pause_briefly();
        }
        Err("The previous Home process has not stopped".into())
    }
    fn launch(&self) -> Result<()> {
        accepted(self.luna("launch", json!({"id":self.id}))?).map(|_| ())
    }
    fn release(&self) -> Result<()> {
        let mut last = String::new();
        for _ in 0..3 {
            match self.luna("releaseBlockSystemApp", json!({"id":self.id})) {
                Ok(reply) => {
                    let text = reply["errorText"]
                        .as_str()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if reply["returnValue"] == true
                        || (text.contains("not") && text.contains("blocked"))
                    {
                        return Ok(());
                    }
                    last = reply.to_string();
                }
                Err(error) => last = error,
            }
            self.system.pause();
        }
        Err(format!("Could not release Home: {last}"))
    }
    fn reread(&self, expected: &Value) -> Result<()> {
        let mut payload = json!({"id":self.id});
        if self
            .read("/etc/palm/schemas/sam/applicationManager.blockSystemApp.schema")
            .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains("\"forceBlockSystemApp\""))
        {
            payload["forceBlockSystemApp"] = json!(false);
        }
        // Release even if the request timed out after taking effect; preserve the original error.
        let blocked = self.luna("blockSystemApp", payload).and_then(|reply| {
            let text = reply["errorText"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase();
            if text.contains("already") && text.contains("blocked") {
                Ok(reply)
            } else {
                accepted(reply)
            }
        });
        let released = self.release();
        match (blocked, released) {
            (Err(block), Err(release)) => {
                return Err(format!("{block}; release also failed: {release}"));
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
            _ => {}
        }
        for _ in 0..15 {
            let reply = accepted(self.luna("getAppInfo", json!({"id":self.id}))?)?;
            let info = &reply["appInfo"];
            let main = expected["main"].as_str();
            if info.is_object()
                && info["type"] == expected["type"]
                && main.is_none_or(|main| {
                    Path::new(info["main"].as_str().unwrap_or("")).file_name()
                        == Path::new(main).file_name()
                })
                && (main != Some("astro")
                    || info["appCategory"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == "home")))
            {
                return Ok(());
            }
            self.system.pause();
        }
        Err("SAM did not read the expected Home manifest".into())
    }
    fn normal_running(&self) -> Result<bool> {
        let reply = accepted(self.luna("running", json!({}))?)?;
        Ok(reply["running"]
            .as_array()
            .is_some_and(|apps| apps.iter().any(|app| app["id"] == APP)))
    }
    fn close_normal(&self) -> Result<()> {
        if !self.normal_running()? {
            return Ok(());
        }
        if let Err(error) = self
            .luna("closeByAppId", json!({"id":APP}))
            .and_then(accepted)
        {
            if self.normal_running()? {
                return Err(error);
            }
            return Ok(());
        }
        for i in 0..=5 {
            if !self.normal_running()? {
                return Ok(());
            }
            if i < 5 {
                self.system.pause();
            }
        }
        Err("Close Astro before enabling Home integration".into())
    }
    pub fn restore(&self, config: &Config) -> Result<()> {
        self.restore_home(config, false)
    }
    /// Gives LG Home back without disabling the integration: state.json and the boot hook stay,
    /// so the next cold boot applies Astro again. A failing Astro may not be in front, so LG Home
    /// is always opened.
    pub fn fallback(&self, config: &Config) -> Result<()> {
        self.restore_home(config, true)
    }
    fn restore_home(&self, config: &Config, open: bool) -> Result<()> {
        let was_home = match self.foreground() {
            Ok(id) => id == self.id,
            Err(e) => {
                eprintln!("Astro Home: {e}; restoring files first");
                false
            }
        };
        if self.foreign_mount()? {
            return Err("Another integration is mounted over LG Home".into());
        }
        if self.mounted() {
            self.unmount(&self.target)?;
            if self.mounted() {
                return Err("The Home bind mount is still active".into());
            }
        }
        self.reread(&system::json(
            self.system,
            format!("{}/appinfo.json", self.target),
        )?)?;
        self.stop_home(&config.app_dir)?;
        if open || was_home {
            self.launch()?;
        }
        Ok(())
    }
    pub fn apply(&self, config: &Config, open: bool) -> Result<()> {
        // Stage timings land in boot.log at cold boot, to see where the delay before Astro goes.
        let started = std::time::Instant::now();
        let stage = |name: &str| {
            eprintln!(
                "Astro Home: {name} after {} ms",
                started.elapsed().as_millis()
            );
        };
        if !self.exists(Path::new(&config.app_dir).join("astro")) {
            return Err("Astro is no longer installed".into());
        }
        // Every apply (boot, enable, manual) gives Astro a fresh set of attempts.
        system::remove(self.system, FAILURES)?;
        if self.foreign_mount()? {
            return Err("Another integration is mounted over LG Home".into());
        }
        if let Err(e) = files::stage_splash(self.system, Path::new(&config.app_dir)) {
            eprintln!("Astro Home: splash not staged: {e}");
        }
        let front = self.foreground()?;
        let result = (|| {
            if !self.mounted() {
                system::command(self.system, "/bin/mount", &["--bind", STAGE, &self.target])?;
            }
            if !self.mounted() {
                return Err("The Home bind mount did not take".into());
            }
            stage("mounted");
            self.reread(&system::json(self.system, format!("{STAGE}/appinfo.json"))?)?;
            stage("manifest reread");
            self.stop_home(&config.app_dir)?;
            stage("previous Home stopped");
            if open || (front == self.id && self.foreground()? == self.id) {
                self.launch()?;
                stage("launched");
            }
            Ok(())
        })();
        if let Err(error) = result {
            return match self.restore(config) {
                Ok(()) => Err(error),
                Err(recovery) => Err(format!("{error}\nRestore also failed: {recovery}")),
            };
        }
        Ok(())
    }
    fn install_scripts(&self, app: &Path) -> Result<()> {
        // The copied native helper lets disable/recovery work after an app package was removed.
        for (name, mode) in [("home.sh", 0o755), ("astro-helper", 0o755)] {
            let bytes = system::read(self.system, app.join(name), 16 * 1024 * 1024)?;
            system::write(self.system, format!("{STATE}/{name}"), &bytes, mode)?;
        }
        // Retire the former interpreter script only after both replacement files are available.
        system::remove(self.system, format!("{STATE}/home.cjs"))?;
        Ok(())
    }
    pub fn install(&self, app: &Path) -> Result<()> {
        let physical = fs::canonicalize(self.system.path(app)).map_err(|e| e.to_string())?;
        let base = self.system.path(Path::new("/"));
        let app = if base == Path::new("/") {
            physical
        } else {
            Path::new("/").join(
                physical
                    .strip_prefix(base)
                    .map_err(|_| "Installation escaped test root")?,
            )
        };
        let mut manifest = system::json(self.system, app.join("appinfo.json"))?;
        if manifest["id"] != APP || manifest["type"] != "native" || manifest["main"] != "astro" {
            return Err("Enable this from an installed native Astro package".into());
        }
        let previous = load(self.system)?;
        let stock = if self.mounted() {
            previous
                .as_ref()
                .map(|c| c.stock.clone())
                .ok_or("Missing saved stock manifest")?
        } else {
            system::json(self.system, format!("{}/appinfo.json", self.target))?
        };
        let release =
            String::from_utf8(self.read("/etc/starfish-release")?).map_err(|e| e.to_string())?;
        let method = method(&release, &self.id)?;
        if self.foreign_mount()? {
            return Err("Another integration is mounted over LG Home".into());
        }
        if self.exists(HOOK) && !hook_owned(self.system) {
            return Err("64-astro-home is already in use".into());
        }
        if self.exists("/var/lib/webosbrew/init.d/65-astro") {
            return Err(
                "Disable the standalone cold-boot hook with autostart.sh disable first".into(),
            );
        }
        if stock["id"] != self.id || stock["main"] == "astro" {
            return Err("Could not identify the original LG Home manifest".into());
        }
        self.close_normal()?;
        system::mkdir(self.system, STAGE)?;
        let mut links = vec![
            "astro".to_owned(),
            "assets".into(),
            "lib".into(),
            "splash".into(),
        ];
        for key in ["icon", "largeIcon"] {
            if let Some(name) = manifest[key].as_str() {
                if Path::new(name).is_absolute()
                    || Path::new(name)
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err("Invalid package artwork path".into());
                }
                links.push(name.into());
            }
        }
        links.sort();
        links.dedup();
        for name in links {
            let link = Path::new(STAGE).join(&name);
            let tmp = link.with_file_name(format!(
                "{}.new",
                link.file_name()
                    .ok_or("Invalid stage link")?
                    .to_string_lossy()
            ));
            system::remove(self.system, &tmp)?;
            system::link(self.system, app.join(&name), &tmp)?;
            fs::rename(self.system.path(&tmp), self.system.path(&link))
                .map_err(|e| e.to_string())?;
        }
        manifest["id"] = json!(self.id);
        manifest["title"] = json!("Home");
        manifest["visible"] = json!(false);
        manifest["appCategory"] = json!(["home"]);
        system::write_json(self.system, format!("{STAGE}/appinfo.json"), &manifest)?;
        system::write(
            self.system,
            format!("{STAGE}/.astro-home"),
            format!("{APP}\n").as_bytes(),
            0o644,
        )?;
        self.install_scripts(&app)?;
        let mut config = Config {
            enabled: false,
            method: method.into(),
            app_dir: app.to_string_lossy().into_owned(),
            stock,
            home_id: self.id.clone(),
            extra: previous.map_or_else(BTreeMap::new, |c| c.extra),
        };
        system::write_json(self.system, CONFIG, &config)?;
        system::mkdir(self.system, "/var/lib/webosbrew")?;
        system::mkdir(self.system, "/var/lib/webosbrew/init.d")?;
        if !hook_owned(self.system) {
            system::link(self.system, format!("{STATE}/home.sh"), HOOK)?;
        }
        self.apply(&config, true)?;
        config.enabled = true;
        system::write_json(self.system, CONFIG, &config)
    }
}
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
pub fn home_process(
    args: &[String],
    executable: Option<&Path>,
    app: &str,
    id: &str,
    system: &impl System,
) -> bool {
    let main = args.first().map(String::as_str).unwrap_or("");
    let native = main == format!("/usr/bin/{id}");
    let flutter = Path::new(main)
        .file_name()
        .is_some_and(|name| name == "flutter-client")
        && args.windows(2).any(|pair| pair[0] == "-i" && pair[1] == id);
    let astro = executable.is_some_and(|path| {
        Path::new(path.to_str().unwrap_or("").trim_end_matches(" (deleted)"))
            == system.path(&Path::new(app).join("astro"))
    });
    let metadata = args
        .get(1)
        .and_then(|arg| serde_json::from_str::<Value>(arg).ok());
    native || flutter || (astro && metadata.is_some_and(|m| m["nid"] == id || m["appId"] == id))
}
/// The takeover stages a Home manifest that SAM rereads through releaseBlockSystemApp; it is
/// only verified on starfish 10.x (webOS 25).
pub fn method(release: &str, id: &str) -> Result<&'static str> {
    target(id)?;
    let version = release
        .split("release")
        .nth(1)
        .and_then(|s| s.trim_start().split('.').next())
        .and_then(|s| s.parse::<u32>().ok());
    if version == Some(10) {
        return Ok("manifest");
    }
    Err("This Home integration targets webOS 25 (10.x)".into())
}
pub fn hook_owned(system: &impl System) -> bool {
    fs::read_link(system.path(Path::new(HOOK)))
        .is_ok_and(|p| p == system.path(Path::new("/var/lib/astro-home/home.sh")))
}
pub fn load(system: &impl System) -> Result<Option<Config>> {
    if !system::exists(system, CONFIG) {
        return Ok(None);
    }
    let config: Config =
        serde_json::from_value(system::json(system, CONFIG)?).map_err(|e| e.to_string())?;
    config.validate()?;
    Ok(Some(config))
}
pub fn dispatch(
    system: &impl System,
    command: &str,
    app: Option<&str>,
    selected: Option<&str>,
) -> Result<Value> {
    system::root(system)?;
    let saved = load(system)?;
    let id = selected
        .filter(|id| !id.is_empty())
        .or_else(|| saved.as_ref().map(|c| c.home_id.as_str()))
        .unwrap_or(HOME);
    let home = Home::new(system, id)?;
    match command {
        "status" => Ok(
            json!({"enabled":saved.as_ref().is_some_and(|c|c.enabled),"method":saved.as_ref().map(|c|c.method.as_str()),"homeId":id,"homeKeyVerified":null,"availableTargets":([HOME].into_iter().filter(|id|home.exists(format!("/usr/palm/applications/{id}/appinfo.json"))).collect::<Vec<_>>()),"mounted":saved.is_some()&&home.mounted(),"bootHook":hook_owned(system),"appDir":saved.as_ref().map(|c|c.app_dir.as_str())}),
        ),
        "enable" => {
            home.install(Path::new(app.ok_or("Missing installation directory")?))?;
            Ok(json!({"returnValue":true}))
        }
        "apply" | "disable" | "restore" => {
            let mut config = saved.ok_or("Astro Home has not been enabled")?;
            if command == "apply" && config.enabled {
                home.apply(&config, false)?;
            } else {
                config.enabled = false;
                system::write_json(system, CONFIG, &config)?;
                home.restore(&config)?;
                if hook_owned(system) {
                    system::remove(system, HOOK)?;
                }
            }
            Ok(json!({"returnValue":true}))
        }
        "fallback" => {
            let config = saved.ok_or("Astro Home has not been enabled")?;
            if config.enabled {
                home.fallback(&config)?;
            }
            Ok(json!({"returnValue":true}))
        }
        _ => Err("Unknown Home integration command".into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Continue,
    /// Too many starts never became healthy: give LG Home back until reboot.
    Fallback,
}
/// Called by Astro as Home before it opens its window. A start counts as failed until
/// `record_healthy`; after FAILURE_LIMIT failed starts in a row, the next one falls back.
/// Only once per boot: the caller cannot observe whether the fallback succeeded, and an Astro
/// that exits on every start without one would leave the TV with no Home at all.
pub fn record_start(system: &impl System) -> Result<Start> {
    let failures = system::read(system, FAILURES, 16)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if failures > FAILURE_LIMIT {
        return Ok(Start::Continue);
    }
    system::write(
        system,
        FAILURES,
        format!("{}\n", failures + 1).as_bytes(),
        0o644,
    )?;
    Ok(if failures == FAILURE_LIMIT {
        Start::Fallback
    } else {
        Start::Continue
    })
}
/// Astro has been running long enough: its next start begins a new count.
pub fn record_healthy(system: &impl System) -> Result<()> {
    system::remove(system, FAILURES)
}
