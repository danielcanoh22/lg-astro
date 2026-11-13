use super::*;
use crate::system::{Output, System};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
const DIR: &str = "/media/developer/apps/usr/palm/applications/com.danielcanoh22.astro";
/// LG Home on webOS 25 is a Flutter app started by `flutter-client -i com.webos.app.home`.
const STOCK_MAIN: &str = "/usr/palm/applications/com.webos.app.home/";
const TARGET: &str = "/usr/palm/applications/com.webos.app.home";
struct Fixture {
    root: PathBuf,
    calls: RefCell<Vec<String>>,
    replies: RefCell<HashMap<String, VecDeque<Result<Value>>>>,
    mounts: RefCell<BTreeMap<String, String>>,
    front: RefCell<String>,
    normal: Cell<bool>,
    close_normal: Cell<bool>,
    uid: Cell<u32>,
    pauses: Cell<usize>,
    brief_pauses: Cell<usize>,
    fail_unmount: Cell<bool>,
    stuck: Cell<bool>,
}
impl Fixture {
    fn new() -> Self {
        Self::under(&std::env::temp_dir())
    }
    /// The root is canonical like the paths Home::install derives from it: macOS TMPDIR
    /// (/var/folders/...) is a symlinked parent of /private/var/folders/....
    fn under(parent: &Path) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = parent.join(format!(
            "astro-native-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let f = Self {
            root,
            calls: RefCell::new(vec![]),
            replies: RefCell::new(HashMap::new()),
            mounts: RefCell::new(BTreeMap::new()),
            front: RefCell::new("other.app".into()),
            normal: Cell::new(false),
            close_normal: Cell::new(true),
            uid: Cell::new(0),
            pauses: Cell::new(0),
            brief_pauses: Cell::new(0),
            fail_unmount: Cell::new(false),
            stuck: Cell::new(false),
        };
        for dir in [
            "/var/lib/astro-home/app",
            "/var/lib/webosbrew/init.d",
            "/var/run",
            "/proc/self",
        ] {
            fs::create_dir_all(f.p(dir)).unwrap();
        }
        f.put("/proc/self/mountinfo", b"");
        f.put("/etc/starfish-release", b"Rockhopper release 10.3.2-34");
        f.put_json(
            &format!("/usr/palm/applications/{}/appinfo.json", home::HOME),
            stock(home::HOME),
        );
        f.put_json(&format!("{DIR}/appinfo.json"),json!({"id":APP,"type":"native","main":"astro","icon":"app-icon.png","largeIcon":"app-icon-large.png","supportGIP":true}));
        f.put(&format!("{DIR}/astro"), b"native-astro");
        f.put(&format!("{DIR}/astro-helper"), b"native-helper");
        f.put(&format!("{DIR}/home.sh"), b"#!/bin/sh\n");
        f.put(&format!("{DIR}/splash.png"), b"default");
        for name in ["dunes", "forest", "prism"] {
            f.put(&format!("{DIR}/splash/{name}.png"), name.as_bytes());
        }
        f
    }
    fn p(&self, path: &str) -> PathBuf {
        self.path(Path::new(path))
    }
    fn put(&self, path: &str, bytes: &[u8]) {
        let p = self.p(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    fn put_json(&self, path: &str, value: Value) {
        self.put(path, value.to_string().as_bytes());
    }
    fn config(&self) -> home::Config {
        home::Config {
            enabled: true,
            method: "manifest".into(),
            app_dir: DIR.into(),
            stock: stock(home::HOME),
            home_id: home::HOME.into(),
            extra: BTreeMap::new(),
        }
    }
    fn reply(&self, method: &str, value: Result<Value>) {
        self.replies
            .borrow_mut()
            .entry(method.into())
            .or_default()
            .push_back(value);
    }
    fn sync_mounts(&self) {
        let data = self
            .mounts
            .borrow()
            .iter()
            .map(|(target, source)| format!("1 0 0:1 {source} {target} rw - ext4 /dev/test rw\n"))
            .collect::<String>();
        self.put("/proc/self/mountinfo", data.as_bytes());
    }
    fn mount(&self, source: &str, target: &str) {
        let p = self.p(target);
        let backup = p.with_file_name(format!(
            "{}.stock",
            p.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(&p, &backup).unwrap();
        if self.p(source).is_dir() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(self.p(source), &p).unwrap();
        } else {
            fs::hard_link(self.p(source), &p).unwrap();
        }
        self.mounts
            .borrow_mut()
            .insert(target.into(), source.into());
        self.sync_mounts();
    }
    fn unmount(&self, target: &str) {
        let p = self.p(target);
        fs::remove_file(&p).unwrap();
        let backup = p.with_file_name(format!(
            "{}.stock",
            p.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(backup, p).unwrap();
        self.mounts.borrow_mut().remove(target);
        self.sync_mounts();
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.p(path)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl System for Fixture {
    fn path(&self, p: &Path) -> PathBuf {
        self.root.join(p.strip_prefix("/").unwrap_or(p))
    }
    fn uid(&self) -> u32 {
        self.uid.get()
    }
    fn pause(&self) {
        self.pauses.set(self.pauses.get() + 1);
    }
    fn pause_briefly(&self) {
        self.brief_pauses.set(self.brief_pauses.get() + 1);
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        self.calls.borrow_mut().push(format!("kill {pid}"));
        if !self.stuck.get() {
            fs::remove_dir_all(self.p(&format!("/proc/{pid}"))).unwrap();
        }
        Ok(())
    }
    fn run(&self, program: &str, args: &[String], _: Duration, _: usize) -> Result<Output> {
        self.calls
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        if program == "/bin/mount" {
            self.mount(&args[1], &args[2]);
            return Ok(ok(json!({})));
        }
        if program == "/bin/umount" {
            if self.fail_unmount.replace(false) {
                return Ok(Output {
                    stdout: vec![],
                    stderr: b"busy".to_vec(),
                    success: false,
                });
            }
            self.unmount(args.last().unwrap());
            return Ok(ok(json!({})));
        }
        if program != "/usr/bin/luna-send" {
            panic!("Unexpected system command: {program}");
        }
        let uri = &args[args.len() - 2];
        let method = uri.rsplit('/').next().unwrap();
        let payload: Value = serde_json::from_str(args.last().unwrap()).unwrap();
        if let Some(reply) = self
            .replies
            .borrow_mut()
            .get_mut(method)
            .and_then(VecDeque::pop_front)
        {
            return reply.map(ok);
        }
        let reply = match method {
            "getForegroundAppInfo" => json!({"returnValue":true,"appId":*self.front.borrow()}),
            "launch" => {
                *self.front.borrow_mut() = payload["id"].as_str().unwrap().into();
                json!({"returnValue":true})
            }
            "running" => {
                json!({"returnValue":true,"running":if self.normal.get(){vec![json!({"id":APP})]}else{vec![]}})
            }
            "closeByAppId" => {
                if self.close_normal.get() {
                    self.normal.set(false);
                    json!({"returnValue":true})
                } else {
                    json!({"returnValue":false,"errorText":"refused"})
                }
            }
            "getAppInfo" => {
                let app = system::json(
                    self,
                    format!(
                        "/usr/palm/applications/{}/appinfo.json",
                        payload["id"].as_str().unwrap()
                    ),
                )?;
                json!({"returnValue":true,"appInfo":app})
            }
            "blockSystemApp" | "releaseBlockSystemApp" | "addDevice" | "deleteDevice" => {
                json!({"returnValue":true})
            }
            _ => panic!("Unexpected Luna method: {method}"),
        };
        Ok(ok(reply))
    }
}
fn ok(reply: Value) -> Output {
    Output {
        stdout: reply.to_string().into_bytes(),
        stderr: vec![],
        success: true,
    }
}
fn stock(id: &str) -> Value {
    json!({"id":id,"type":"flutter","main":format!("/usr/palm/applications/{id}/"),"nativeLifeCycleInterfaceVersion":2})
}

/// The original LG Home manifest is in place: no takeover is mounted over it.
fn stock_manifest_restored(f: &Fixture) -> bool {
    f.mounts.borrow().is_empty()
        && system::json(f, format!("{TARGET}/appinfo.json")).unwrap()["main"] == STOCK_MAIN
}
#[test]
fn home_takeover_accepts_only_lg_home_on_webos_25() {
    for release in ["Rockhopper release 10.1.0", "Rockhopper release 10.3.2-34"] {
        assert_eq!(home::method(release, home::HOME).unwrap(), "manifest");
    }
    for release in [
        "Rockhopper release 4.10.2",
        "Rockhopper release 5.6.2-21",
        "Rockhopper release 6.5.0",
        "Rockhopper release 7.3.0",
        "Rockhopper release 9.2.4-48 (ombre-okapi)",
        "Rockhopper release 11.2.0",
        "no version",
    ] {
        assert!(
            home::method(release, home::HOME)
                .unwrap_err()
                .contains("webOS 25 (10.x)"),
            "{release}"
        );
    }
    assert!(home::method("Rockhopper release 10.3.2-34", "com.webos.app.fullhome").is_err());
    assert!(home::Home::new(&Fixture::new(), "../other").is_err());
}
#[test]
fn enabling_on_another_release_changes_nothing() {
    let f = Fixture::new();
    f.put("/etc/starfish-release", b"Rockhopper release 9.2.4-48");
    assert!(
        home::dispatch(&f, "enable", Some(DIR))
            .unwrap_err()
            .contains("webOS 25 (10.x)")
    );
    assert!(f.calls().is_empty());
    assert!(!system::exists(&f, "/var/lib/astro-home/state.json"));
    assert!(stock_manifest_restored(&f));
}
#[test]
fn status_lists_only_lg_home_and_reports_the_manifest_mount() {
    let f = Fixture::new();
    let status = home::dispatch(&f, "status", None).unwrap();
    assert_eq!(status["availableTargets"], json!([home::HOME]));
    assert_eq!(status["mounted"], false);
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    let status = home::dispatch(&f, "status", None).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["method"], "manifest");
    assert_eq!(status["homeId"], home::HOME);
    assert_eq!(status["mounted"], true);
    assert_eq!(status["bootHook"], true);
    assert_eq!(status["appDir"], DIR);
}
#[test]
fn installation_under_a_symlinked_temporary_parent_stays_inside_the_root() {
    let real = std::env::temp_dir().join(format!("astro-native-real-{}", std::process::id()));
    let link = std::env::temp_dir().join(format!("astro-native-link-{}", std::process::id()));
    fs::create_dir_all(&real).unwrap();
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    {
        let f = Fixture::under(&link);
        home::dispatch(&f, "enable", Some(DIR)).unwrap();
        assert_eq!(home::load(&f).unwrap().unwrap().app_dir, DIR);
        home::dispatch(&f, "disable", None).unwrap();
    }
    fs::remove_file(&link).unwrap();
    fs::remove_dir_all(&real).unwrap();
}
#[test]
fn copied_recovery_binary_and_shell_survive_package_removal() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/astro-helper"), b"native-helper");
    fs::remove_dir_all(f.p(DIR)).unwrap();
    home::dispatch(&f, "disable", None).unwrap();
    assert!(stock_manifest_restored(&f));
}
#[test]
fn boot_apply_does_not_take_another_apps_screen() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    // A reboot drops the bind mount; the boot hook applies it again behind another app.
    f.unmount(TARGET);
    *f.front.borrow_mut() = "other.app".into();
    let launches = || f.calls().iter().filter(|s| s.contains("/launch ")).count();
    let before = launches();
    home::dispatch(&f, "apply", None).unwrap();
    assert!(!f.mounts.borrow().is_empty());
    assert_eq!(f.front.borrow().as_str(), "other.app");
    assert_eq!(launches(), before);
}
#[test]
fn foreign_mounts_are_left_untouched() {
    let f = Fixture::new();
    f.put_json(
        "/var/lib/other/home/appinfo.json",
        json!({"id":home::HOME,"main":"foreign"}),
    );
    f.mount("/var/lib/other/home", TARGET);
    let h = home::Home::new(&f, home::HOME).unwrap();
    assert!(
        h.apply(&f.config(), true)
            .unwrap_err()
            .contains("Another integration")
    );
    assert_eq!(
        system::json(&f, format!("{TARGET}/appinfo.json")).unwrap()["main"],
        "foreign"
    );
    assert!(!f.calls().iter().any(|s| s.contains("mount")));
}
#[test]
fn restoration_works_even_when_foreground_query_fails() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    f.reply("getForegroundAppInfo", Err("offline".into()));
    home::dispatch(&f, "disable", None).unwrap();
    assert!(stock_manifest_restored(&f));
}
#[test]
fn busy_mounts_use_lazy_unmount() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    f.fail_unmount.set(true);
    home::dispatch(&f, "disable", None).unwrap();
    assert!(
        f.calls()
            .iter()
            .any(|s| s == &format!("/bin/umount -l {TARGET}"))
    );
    assert!(stock_manifest_restored(&f));
}
#[test]
fn a_foreign_hook_is_rejected() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    home::dispatch(&f, "disable", None).unwrap();
    system::link(
        &f,
        "/other/home.sh",
        "/var/lib/webosbrew/init.d/64-astro-home",
    )
    .unwrap();
    assert!(
        home::dispatch(&f, "enable", Some(DIR))
            .unwrap_err()
            .contains("already in use")
    );
}
#[test]
fn manifest_takeover_rereads_staged_metadata_and_restores_the_stock_manifest() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    assert_eq!(
        system::json(&f, "/usr/palm/applications/com.webos.app.home/appinfo.json").unwrap()["main"],
        "astro"
    );
    home::dispatch(&f, "disable", None).unwrap();
    assert_eq!(
        system::json(&f, "/usr/palm/applications/com.webos.app.home/appinfo.json").unwrap()["main"],
        STOCK_MAIN
    );
}
#[test]
fn release_is_attempted_even_when_the_block_request_times_out() {
    let f = Fixture::new();
    f.reply("blockSystemApp", Err("timeout after block".into()));
    assert!(home::dispatch(&f, "enable", Some(DIR)).is_err());
    assert!(
        f.calls()
            .iter()
            .any(|s| s.contains("/releaseBlockSystemApp "))
    );
    assert!(
        !f.mounts
            .borrow()
            .contains_key("/usr/palm/applications/com.webos.app.home")
    );
}
#[test]
fn normal_tile_is_closed_by_app_id_and_an_exiting_instance_is_tolerated() {
    let f = Fixture::new();
    f.normal.set(true);
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    assert!(
        f.calls()
            .iter()
            .any(|s| s.contains("/closeByAppId ") && s.contains(APP))
    );
    assert!(!f.normal.get());
}
#[test]
fn normal_close_denials_and_stuck_instances_prevent_mounting() {
    let f = Fixture::new();
    f.normal.set(true);
    f.close_normal.set(false);
    assert!(home::dispatch(&f, "enable", Some(DIR)).is_err());
    assert!(f.mounts.borrow().is_empty());
}
#[test]
fn process_matching_never_kills_a_normal_astro_or_unrelated_native_app() {
    let f = Fixture::new();
    let args = |main: &str, meta: Value| vec![main.into(), meta.to_string()];
    let exe = f.p(&format!("{DIR}/astro"));
    let flutter = |id: &str| -> Vec<String> {
        vec![
            "/usr/bin/flutter-client".into(),
            "-i".into(),
            id.into(),
            json!({}).to_string(),
        ]
    };
    assert!(home::home_process(
        &flutter(home::HOME),
        None,
        DIR,
        home::HOME,
        &f
    ));
    assert!(!home::home_process(
        &flutter("com.webos.app.screensaver"),
        None,
        DIR,
        home::HOME,
        &f
    ));
    assert!(home::home_process(
        &args("astro", json!({"nid":home::HOME})),
        Some(&exe),
        DIR,
        home::HOME,
        &f
    ));
    assert!(!home::home_process(
        &args("astro", json!({"nid":APP})),
        Some(&exe),
        DIR,
        home::HOME,
        &f
    ));
    assert!(!home::home_process(
        &args("/usr/bin/com.webos.app.livetv", json!({})),
        None,
        DIR,
        home::HOME,
        &f
    ));
}

#[test]
fn icon_discovery_uses_live_installation_then_manifest_and_default_fallbacks() {
    let f = Fixture::new();
    let dir = "/mnt/otncabi/usr/palm/applications/sample";
    f.put_json(
        &format!("{dir}/appinfo.json"),
        json!({"id":"sample","largeIcon":"large.webp","icon":"normal.png"}),
    );
    f.put(&format!("{dir}/large.webp"), &vec![1; 512 * 1024 + 1]);
    f.put(&format!("{dir}/normal.png"), b"normal");
    f.put(&format!("{dir}/icon.png"), b"default");
    let source = files::IconSource {
        id: "sample".into(),
        directory: Some(dir.into()),
        large_icon: None,
        icon: None,
    };
    let reply = files::read_icons(&f, &source).unwrap();
    assert_eq!(reply["icons"], json!(["6e6f726d616c", "64656661756c74"]));
}
#[test]
fn icon_limits_are_aggregate_and_duplicate_paths_are_not_read_twice() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put_json(
        &format!("{dir}/appinfo.json"),
        json!({"id":"sample","largeIcon":"big.png","icon":"big.png"}),
    );
    f.put(&format!("{dir}/big.png"), &vec![7; 512 * 1024]);
    f.put(&format!("{dir}/icon.png"), b"not included");
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    let reply = files::read_icons(&f, &source).unwrap();
    assert_eq!(reply["icons"].as_array().unwrap().len(), 1);
    assert_eq!(reply["icons"][0].as_str().unwrap().len(), 1024 * 1024);
}
#[test]
fn icon_paths_cannot_escape_but_shell_punctuation_remains_literal() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    let name = "safe;$(touch no).png";
    f.put(&format!("{dir}/{name}"), b"safe");
    for icon in [
        "../../etc/a.png",
        "file:///etc/a.png",
        "https://evil/a.png",
        "bad\n.png",
    ] {
        let source = files::IconSource {
            id: "sample".into(),
            directory: None,
            large_icon: None,
            icon: Some(icon.into()),
        };
        assert_eq!(files::read_icons(&f, &source).unwrap()["icons"], json!([]));
    }
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: Some(name.into()),
    };
    assert_eq!(
        files::read_icons(&f, &source).unwrap()["icons"],
        json!(["73616665"])
    );
    assert!(f.calls().is_empty());
}
#[test]
fn invalid_ids_and_unrelated_manifests_cannot_select_private_artwork() {
    let f = Fixture::new();
    let mut s = files::IconSource {
        id: "../etc".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    assert!(files::read_icons(&f, &s).is_err());
    s.id = "sample".into();
    f.put_json(
        "/usr/palm/applications/sample/appinfo.json",
        json!({"id":"other","icon":"private.png"}),
    );
    f.put("/usr/palm/applications/sample/private.png", b"private");
    assert_eq!(files::read_icons(&f, &s).unwrap()["icons"], json!([]));
}
#[cfg(unix)]
#[test]
fn an_icon_linked_outside_the_application_roots_is_not_read() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put_json(
        &format!("{dir}/appinfo.json"),
        json!({"id":"sample","icon":"linked.png"}),
    );
    f.put("/var/private", b"private");
    std::os::unix::fs::symlink(f.p("/var/private"), f.p(&format!("{dir}/linked.png"))).unwrap();
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    assert_eq!(files::read_icons(&f, &source).unwrap()["icons"], json!([]));
}
#[cfg(unix)]
#[test]
fn an_icon_under_a_directory_linked_outside_the_application_roots_is_not_read() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put_json(
        &format!("{dir}/appinfo.json"),
        json!({"id":"sample","icon":"art/icon.png"}),
    );
    f.put("/var/private/icon.png", b"private");
    std::os::unix::fs::symlink(f.p("/var/private"), f.p(&format!("{dir}/art"))).unwrap();
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    assert_eq!(files::read_icons(&f, &source).unwrap()["icons"], json!([]));
}
#[cfg(unix)]
#[test]
fn a_manifest_linked_outside_the_application_roots_names_no_icons() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put_json(
        "/var/private.json",
        json!({"id":"sample","icon":"named.png"}),
    );
    f.put(&format!("{dir}/named.png"), b"named");
    std::os::unix::fs::symlink(
        f.p("/var/private.json"),
        f.p(&format!("{dir}/appinfo.json")),
    )
    .unwrap();
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    assert_eq!(files::read_icons(&f, &source).unwrap()["icons"], json!([]));
}
#[cfg(unix)]
#[test]
fn fifos_in_place_of_a_manifest_or_an_icon_are_skipped_without_blocking() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put(&format!("{dir}/real.png"), b"real");
    for name in ["appinfo.json", "icon.png"] {
        let made = std::process::Command::new("mkfifo")
            .arg(f.p(&format!("{dir}/{name}")))
            .status()
            .unwrap();
        assert!(made.success());
    }
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: Some("real.png".into()),
        icon: None,
    };
    assert_eq!(
        files::read_icons(&f, &source).unwrap()["icons"],
        json!(["7265616c"])
    );
}
#[cfg(unix)]
#[test]
fn an_icon_linked_inside_the_application_roots_is_still_read() {
    let f = Fixture::new();
    let dir = "/usr/palm/applications/sample";
    f.put_json(
        &format!("{dir}/appinfo.json"),
        json!({"id":"sample","icon":"linked.png"}),
    );
    f.put(&format!("{dir}/real.png"), b"real");
    std::os::unix::fs::symlink(
        f.p(&format!("{dir}/real.png")),
        f.p(&format!("{dir}/linked.png")),
    )
    .unwrap();
    let source = files::IconSource {
        id: "sample".into(),
        directory: None,
        large_icon: None,
        icon: None,
    };
    assert_eq!(
        files::read_icons(&f, &source).unwrap()["icons"],
        json!(["7265616c"])
    );
}

#[test]
fn splash_sync_accepts_only_the_developer_installation() {
    let f = Fixture::new();
    for other in [
        "/media/cryptofs/apps/usr/palm/applications/com.danielcanoh22.astro",
        "/usr/palm/applications/com.danielcanoh22.astro",
    ] {
        f.put_json(
            &format!("{other}/appinfo.json"),
            json!({"id":APP,"type":"native","main":"astro"}),
        );
        f.put(&format!("{other}/splash/forest.png"), b"forest");
        assert_eq!(
            files::sync_splash(&f, Path::new(other), "forest").unwrap_err(),
            "Invalid Astro installation directory"
        );
        assert!(!f.p(&format!("{other}/splash.png")).exists());
    }
    files::sync_splash(&f, Path::new(DIR), "forest").unwrap();
}

#[test]
fn splash_updates_tile_and_only_the_enabled_own_manifest_stage() {
    let f = Fixture::new();
    f.put_json(
        "/var/lib/astro-home/state.json",
        json!({"enabled":true,"method":"manifest","appDir":DIR}),
    );
    f.put("/var/lib/astro-home/app/splash.png", b"old");
    files::sync_splash(&f, Path::new(DIR), "forest").unwrap();
    assert_eq!(f.read(&format!("{DIR}/splash.png")), b"forest");
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"forest");
    let before = fs::metadata(f.p(&format!("{DIR}/splash.png")))
        .unwrap()
        .modified()
        .unwrap();
    files::sync_splash(&f, Path::new(DIR), "forest").unwrap();
    assert_eq!(
        fs::metadata(f.p(&format!("{DIR}/splash.png")))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}
#[test]
fn other_or_disabled_stages_are_never_written() {
    for c in [
        json!({"enabled":false,"appDir":DIR}),
        json!({"enabled":true,"appDir":"/other/install"}),
    ] {
        let f = Fixture::new();
        f.put_json("/var/lib/astro-home/state.json", c);
        f.put("/var/lib/astro-home/app/splash.png", b"old");
        files::sync_splash(&f, Path::new(DIR), "prism").unwrap();
        assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"old");
    }
}
#[test]
fn missing_or_invalid_splash_sources_preserve_previous_images() {
    let f = Fixture::new();
    for background in ["../forest", "bad/name", "missing"] {
        assert!(files::sync_splash(&f, Path::new(DIR), background).is_err());
        assert_eq!(f.read(&format!("{DIR}/splash.png")), b"default");
    }
    assert!(files::sync_splash(&f, Path::new("/tmp/com.danielcanoh22.astro"), "forest").is_err());
}
#[test]
fn failed_atomic_write_cleans_its_private_temporary_file() {
    let f = Fixture::new();
    fs::create_dir_all(f.p("/directory/destination")).unwrap();
    assert!(system::write(&f, "/directory/destination", b"bytes", 0o644).is_err());
    assert!(fs::read_dir(f.p("/directory")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')
    }));
}
#[test]
fn stage_uses_persistent_selection_and_retains_manual_background_over_rotation() {
    let f = Fixture::new();
    f.put_json(
        &format!("/media/developer/{APP}/preferences.json"),
        json!({"background":"prism","splash_background":"forest"}),
    );
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"forest");
}
#[test]
fn null_manual_selection_falls_back_to_saved_background() {
    let f = Fixture::new();
    f.put_json(
        &format!("/media/developer/{APP}/preferences.json"),
        json!({"background":"prism","splash_background":null}),
    );
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"prism");
}
#[test]
fn malformed_missing_and_unavailable_preferences_use_package_splash() {
    for preferences in [
        b"not-json".as_slice(),
        b"{}",
        b"{\"background\":\"missing\"}",
        b"{\"background\":\"../bad\"}",
    ] {
        let f = Fixture::new();
        f.put(
            &format!("/media/developer/{APP}/preferences.json"),
            preferences,
        );
        files::stage_splash(&f, Path::new(DIR)).unwrap();
        assert!(matches!(
            f.read("/var/lib/astro-home/app/splash.png").as_slice(),
            b"dunes" | b"default"
        ));
    }
}
#[test]
fn symlink_storage_directories_are_not_used_as_root_preferences() {
    let f = Fixture::new();
    f.put_json("/other/preferences.json", json!({"background":"prism"}));
    system::link(&f, "/other", format!("/media/developer/{APP}")).unwrap();
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"dunes");
}

const UPLOAD: &str = "custom-0123456789abcdef0123456789abcdef";

/// The start of a PNG: signature and IHDR dimensions, followed by `marker`.
fn splash_png(width: u32, height: u32, marker: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend(marker);
    bytes
}

fn storage() -> String {
    format!("/media/developer/{APP}")
}

fn upload_preferences(f: &Fixture) {
    f.put_json(
        &format!("{}/preferences.json", storage()),
        json!({"background":"prism","splash_background":UPLOAD}),
    );
}

fn enabled_stage(f: &Fixture) {
    f.put_json(
        "/var/lib/astro-home/state.json",
        json!({"enabled":true,"method":"manifest","appDir":DIR}),
    );
    f.put("/var/lib/astro-home/app/splash.png", b"old");
}

#[test]
fn uploaded_splash_is_copied_from_storage_to_the_package_and_enabled_stage() {
    let f = Fixture::new();
    enabled_stage(&f);
    upload_preferences(&f);
    let image = splash_png(1920, 1080, b"upload");
    f.put(
        &format!("{}/wallpapers/{UPLOAD}/splash.png", storage()),
        &image,
    );
    files::sync_splash(&f, Path::new(DIR), UPLOAD).unwrap();
    assert_eq!(f.read(&format!("{DIR}/splash.png")), image);
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), image);
}

#[test]
fn unsafe_or_invalid_uploaded_splashes_preserve_previous_images() {
    let good = splash_png(1920, 1080, b"upload");
    let folder = format!("{}/wallpapers/{UPLOAD}", storage());
    type Setup<'a> = Box<dyn Fn(&Fixture) + 'a>;
    let cases: Vec<(&str, Setup<'_>)> = vec![
        (
            "missing image",
            Box::new(|f| fs::create_dir_all(f.p(&folder)).unwrap()),
        ),
        (
            "not a PNG",
            Box::new(|f| f.put(&format!("{folder}/splash.png"), b"not a png image file")),
        ),
        (
            "wrong size",
            Box::new(|f| {
                f.put(
                    &format!("{folder}/splash.png"),
                    &splash_png(1280, 720, b"upload"),
                )
            }),
        ),
        (
            "symlinked image",
            Box::new(|f| {
                f.put("/other/splash.png", &good);
                fs::create_dir_all(f.p(&folder)).unwrap();
                system::link(f, "/other/splash.png", format!("{folder}/splash.png")).unwrap();
            }),
        ),
        (
            "symlinked upload folder",
            Box::new(|f| {
                f.put("/other/splash.png", &good);
                fs::create_dir_all(f.p(&format!("{}/wallpapers", storage()))).unwrap();
                system::link(f, "/other", &folder).unwrap();
            }),
        ),
        (
            "symlinked gallery",
            Box::new(|f| {
                f.put(&format!("/other/{UPLOAD}/splash.png"), &good);
                system::link(f, "/other", format!("{}/wallpapers", storage())).unwrap();
            }),
        ),
        (
            "symlinked storage",
            Box::new(|f| {
                fs::remove_dir_all(f.p(&storage())).unwrap();
                f.put_json(
                    "/other/preferences.json",
                    json!({"splash_background":UPLOAD}),
                );
                f.put(&format!("/other/wallpapers/{UPLOAD}/splash.png"), &good);
                system::link(f, "/other", storage()).unwrap();
            }),
        ),
    ];
    for (name, setup) in cases {
        let f = Fixture::new();
        enabled_stage(&f);
        upload_preferences(&f);
        setup(&f);
        assert!(
            files::sync_splash(&f, Path::new(DIR), UPLOAD).is_err(),
            "{name}"
        );
        assert_eq!(f.read(&format!("{DIR}/splash.png")), b"default", "{name}");
        assert_eq!(
            f.read("/var/lib/astro-home/app/splash.png"),
            b"old",
            "{name}"
        );
    }
    for id in [
        "custom-0123456789abcdef0123456789abcde",
        "custom-0123456789ABCDEF0123456789ABCDEF",
        "custom-../../../../../../../../../../xx",
    ] {
        let f = Fixture::new();
        upload_preferences(&f);
        if !id.contains("..") {
            f.put(&format!("{}/wallpapers/{id}/splash.png", storage()), &good);
        }
        assert!(files::sync_splash(&f, Path::new(DIR), id).is_err(), "{id}");
        assert_eq!(f.read(&format!("{DIR}/splash.png")), b"default", "{id}");
    }
}

#[test]
fn stage_uses_the_manual_upload_splash_from_storage() {
    let f = Fixture::new();
    upload_preferences(&f);
    let image = splash_png(1920, 1080, b"upload");
    f.put(
        &format!("{}/wallpapers/{UPLOAD}/splash.png", storage()),
        &image,
    );
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), image);
}

#[test]
fn stage_uses_the_package_splash_when_the_upload_splash_is_unusable() {
    let f = Fixture::new();
    upload_preferences(&f);
    f.put(
        &format!("{}/wallpapers/{UPLOAD}/splash.png", storage()),
        b"not a png image file",
    );
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"default");
}

#[test]
fn the_package_directory_is_never_storage_for_uploaded_splashes() {
    let f = Fixture::new();
    enabled_stage(&f);
    f.put_json(
        &format!("{DIR}/preferences.json"),
        json!({"splash_background":UPLOAD}),
    );
    f.put(
        &format!("{DIR}/wallpapers/{UPLOAD}/splash.png"),
        &splash_png(1920, 1080, b"package"),
    );
    assert!(files::sync_splash(&f, Path::new(DIR), UPLOAD).is_err());
    assert_eq!(f.read(&format!("{DIR}/splash.png")), b"default");
    // The package-local preferences are not read either: the default background is staged.
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"dunes");
}

#[test]
fn only_developer_storage_provides_preferences_and_uploaded_splashes() {
    let f = Fixture::new();
    enabled_stage(&f);
    let image = splash_png(1920, 1080, b"other");
    for other in [
        format!("/media/developer/temp/{APP}"),
        format!("/media/internal/.{APP}"),
    ] {
        f.put_json(
            &format!("{other}/preferences.json"),
            json!({"background":"prism","splash_background":UPLOAD}),
        );
        f.put(&format!("{other}/wallpapers/{UPLOAD}/splash.png"), &image);
    }
    assert_eq!(
        files::sync_splash(&f, Path::new(DIR), UPLOAD).unwrap_err(),
        "Uploaded background unavailable"
    );
    assert_eq!(f.read(&format!("{DIR}/splash.png")), b"default");
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"dunes");
    f.put_json(
        &format!("{}/preferences.json", storage()),
        json!({"background":"forest"}),
    );
    files::stage_splash(&f, Path::new(DIR)).unwrap();
    assert_eq!(f.read("/var/lib/astro-home/app/splash.png"), b"forest");
}

#[test]
fn storage_under_a_symlinked_system_directory_still_provides_uploaded_splashes() {
    let f = Fixture::new();
    fs::rename(f.p("/media/developer"), f.p("/real-developer")).unwrap();
    system::link(&f, "/real-developer", "/media/developer").unwrap();
    let image = splash_png(1920, 1080, b"developer");
    upload_preferences(&f);
    f.put(
        &format!("/real-developer/{APP}/wallpapers/{UPLOAD}/splash.png"),
        &image,
    );
    files::sync_splash(&f, Path::new(DIR), UPLOAD).unwrap();
    assert_eq!(f.read(&format!("{DIR}/splash.png")), image);
}

#[cfg(target_os = "linux")]
#[test]
fn opened_files_are_located_by_the_kernel_not_by_the_requested_path() {
    let f = Fixture::new();
    f.put("/real/splash.png", b"image");
    system::link(&f, "/real", "/linked").unwrap();
    let real = f.p("/real/splash.png");
    let file = fs::File::open(f.p("/linked/splash.png")).unwrap();
    assert!(!files::opened_at(&file, &f.p("/linked/splash.png")));
    assert!(files::opened_at(&file, &real));
}

#[test]
fn uninstall_requires_matching_completion_not_just_acceptance() {
    let mut s = uninstall::Stream::default();
    assert!(!s.feed(br#"{"returnValue":true}"#, false, "sample").unwrap());
    assert!(s.feed(br#"{"returnValue":true,"statusValue":31,"id":"sample","details":{"packageId":"sample"}}"#,false,"sample").unwrap());
    assert_eq!(s.replies.len(), 2);
}
#[test]
fn uninstall_parser_handles_split_pretty_unicode_and_braces_in_strings() {
    let text = "{\n\"returnValue\":true,\"text\":\"Ä {quoted}\\\"\"}\n{\"statusValue\":31,\"id\":\"sample\"}";
    let mut s = uninstall::Stream::default();
    let mut done = false;
    for byte in text.bytes() {
        done = s.feed(&[byte], false, "sample").unwrap();
    }
    assert!(done);
    assert_eq!(s.replies.len(), 2);
}
#[test]
fn uninstall_rejects_failures_wrong_identity_and_resource_exhaustion() {
    for reply in [
        json!({"returnValue":false}),
        json!({"statusValue":25}),
        json!({"details":{"errorCode":1}}),
        json!({"statusValue":31,"id":"other"}),
        json!({"statusValue":31,"id":"sample","details":{"packageId":"other"}}),
    ] {
        assert!(uninstall::completed(&reply, "sample").is_err());
    }
    let mut s = uninstall::Stream::default();
    assert!(s.feed(&vec![0; 65537], true, "sample").is_err());
    let mut s = uninstall::Stream::default();
    for _ in 0..64 {
        s.feed(br#"{}"#, false, "sample").unwrap();
    }
    assert!(s.feed(br#"{}"#, false, "sample").is_err());
}
#[test]
fn uninstall_protects_system_apps_and_does_not_spawn_when_denied() {
    for id in [
        APP,
        "com.webos.app.home",
        "org.webosbrew.hbchannel",
        "bad/id",
        "",
    ] {
        assert!(!uninstall::allowed(id));
        assert_eq!(uninstall::remove(id, 0)["returnValue"], false);
    }
    assert!(uninstall::allowed("org.example.app"));
    assert_eq!(uninstall::remove("sample", 1000)["returnValue"], false);
}
#[test]
fn uninstall_accepts_only_ids_the_app_itself_accepts() {
    for id in ["a..b", &"a".repeat(200)] {
        assert!(!uninstall::allowed(id), "{id}");
        assert_eq!(uninstall::remove(id, 0)["returnValue"], false);
    }
    assert!(uninstall::allowed("org.example.app"));
}
#[test]
fn input_verification_failure_rolls_back_and_existing_registration_is_preserved() {
    let f = Fixture::new();
    f.reply(
        "getAllInputStatus",
        Ok(json!({"returnValue":true,"devices":[]})),
    );
    f.reply(
        "getAllInputStatus",
        Ok(json!({"returnValue":true,"devices":[]})),
    );
    assert!(
        input::configure(&f, "enable", Path::new(DIR))
            .unwrap_err()
            .contains("registration removed")
    );
    assert!(f.calls().iter().any(|s| s.contains("/deleteDevice ")));
    let f = Fixture::new();
    f.reply(
        "getAllInputStatus",
        Ok(json!({"returnValue":true,"devices":[{"appId":APP}]})),
    );
    assert_eq!(
        input::configure(&f, "enable", Path::new(DIR)).unwrap()["registered"],
        true
    );
    assert!(!f.calls().iter().any(|s| s.contains("/addDevice ")));
}
#[test]
fn root_mutations_reject_non_root_callers() {
    let f = Fixture::new();
    f.uid.set(1000);
    assert!(home::dispatch(&f, "enable", Some(DIR)).is_err());
    assert!(input::configure(&f, "enable", Path::new(DIR)).is_err());
    assert!(f.calls().is_empty());
}
#[test]
fn process_runner_enforces_combined_output_and_deadline() {
    let out = system::run_bounded(
        "/bin/sh",
        &["-c".into(), "printf good; printf error >&2".into()],
        Duration::from_secs(2),
        32,
    )
    .unwrap();
    assert_eq!(out.stdout, b"good");
    assert_eq!(out.stderr, b"error");
    assert!(
        system::run_bounded(
            "/bin/sh",
            &["-c".into(), "printf 123456789".into()],
            Duration::from_secs(2),
            3
        )
        .is_err()
    );
    assert!(
        system::run_bounded("/bin/sleep", &["2".into()], Duration::from_millis(30), 32).is_err()
    );
}

#[test]
fn restoring_a_background_home_never_opens_over_another_app() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    *f.front.borrow_mut() = "other.app".into();
    let launches = || f.calls().iter().filter(|s| s.contains("/launch ")).count();
    let before = launches();
    home::dispatch(&f, "disable", None).unwrap();
    assert_eq!(f.front.borrow().as_str(), "other.app");
    assert_eq!(launches(), before);
    assert!(stock_manifest_restored(&f));
}
#[test]
fn a_stuck_home_process_is_reported_after_unmount_recovery() {
    let f = Fixture::new();
    f.put(
        "/proc/777/cmdline",
        b"/usr/bin/flutter-client\0-i\0com.webos.app.home\0{}\0",
    );
    f.stuck.set(true);
    let error = home::dispatch(&f, "enable", Some(DIR)).unwrap_err();
    assert!(error.contains("Restore also failed"));
    assert!(stock_manifest_restored(&f));
    // apply and its restore each wait the full 5 s budget in 200 ms steps.
    assert_eq!(f.brief_pauses.get(), 2 * 25);
    assert_eq!(system::BRIEF_PAUSE * 25, Duration::from_secs(5));
}
#[test]
fn accepted_close_waits_boundedly_and_a_close_race_can_succeed() {
    let f = Fixture::new();
    f.normal.set(true);
    f.reply("closeByAppId", Ok(json!({"returnValue":true})));
    assert!(
        home::dispatch(&f, "enable", Some(DIR))
            .unwrap_err()
            .contains("Close Astro")
    );
    assert_eq!(f.pauses.get(), 5);
    assert!(f.mounts.borrow().is_empty());
    let f = Fixture::new();
    f.reply(
        "running",
        Ok(json!({"returnValue":true,"running":[{"id":APP}]})),
    );
    f.reply("running", Ok(json!({"returnValue":true,"running":[]})));
    f.reply("closeByAppId", Ok(json!({"returnValue":false})));
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
}
#[test]
fn persistent_preferences_win_over_package_local_preferences_and_active_stages_refresh() {
    let f = Fixture::new();
    f.put_json(
        &format!("{DIR}/preferences.json"),
        json!({"background":"prism"}),
    );
    f.put_json(
        &format!("/media/developer/{APP}/preferences.json"),
        json!({"background":"forest"}),
    );
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    assert_eq!(
        f.read("/usr/palm/applications/com.webos.app.home/splash.png"),
        b"forest"
    );
    f.put_json(
        &format!("/media/developer/{APP}/preferences.json"),
        json!({"background":"prism"}),
    );
    home::dispatch(&f, "apply", None).unwrap();
    assert_eq!(
        f.read("/usr/palm/applications/com.webos.app.home/splash.png"),
        b"prism"
    );
}
#[test]
fn unhealthy_starts_fall_back_once_after_the_limit_and_health_starts_a_new_count() {
    let f = Fixture::new();
    for _ in 0..home::FAILURE_LIMIT {
        assert_eq!(home::record_start(&f).unwrap(), home::Start::Continue);
    }
    assert_eq!(home::record_start(&f).unwrap(), home::Start::Fallback);
    // One attempt per boot: if that fallback failed, Astro must keep starting normally.
    assert_eq!(home::record_start(&f).unwrap(), home::Start::Continue);
    assert_eq!(home::record_start(&f).unwrap(), home::Start::Continue);
    home::record_healthy(&f).unwrap();
    assert_eq!(home::record_start(&f).unwrap(), home::Start::Continue);
    assert_eq!(f.read(home::FAILURES), b"1\n");
    f.put(home::FAILURES, b"garbage");
    assert_eq!(home::record_start(&f).unwrap(), home::Start::Continue);
    assert_eq!(f.read(home::FAILURES), b"1\n");
}
#[test]
fn fallback_restores_lg_home_until_the_next_apply() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    assert!(!f.mounts.borrow().is_empty());
    let launches = || f.calls().iter().filter(|s| s.contains("/launch ")).count();
    let before = launches();
    // A crashing Astro is not in front, yet LG Home must open for the remote.
    *f.front.borrow_mut() = "other.app".into();
    home::dispatch(&f, "fallback", None).unwrap();
    assert!(stock_manifest_restored(&f));
    assert_eq!(launches(), before + 1);
    // Only until reboot: the integration and its boot hook stay.
    assert_eq!(
        system::json(&f, "/var/lib/astro-home/state.json").unwrap()["enabled"].as_bool(),
        Some(true)
    );
    assert!(system::exists(
        &f,
        "/var/lib/webosbrew/init.d/64-astro-home"
    ));
    f.put(home::FAILURES, b"3\n");
    home::dispatch(&f, "apply", None).unwrap();
    assert!(!f.mounts.borrow().is_empty());
    assert!(!system::exists(&f, home::FAILURES));
}
#[test]
fn fallback_changes_nothing_once_the_integration_is_disabled() {
    let f = Fixture::new();
    home::dispatch(&f, "enable", Some(DIR)).unwrap();
    home::dispatch(&f, "disable", None).unwrap();
    let calls = f.calls().len();
    home::dispatch(&f, "fallback", None).unwrap();
    assert_eq!(f.calls().len(), calls);
    assert!(f.mounts.borrow().is_empty());
}
