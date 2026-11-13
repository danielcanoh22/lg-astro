//! Runs TV platform work (Luna calls, inventory, the root helper) on one worker thread.
//!
//! Blocking platform work never runs on the SDL/UI thread: [`Platform`] queues a [`Request`]
//! and the worker answers with a [`Response`]. TV calls originate from this native executable:
//! its own LS2 connection, or root `luna-send` and `astro-helper` commands it runs through
//! Homebrew Channel's `exec`. None of them impersonates an application identity. Desktop
//! builds answer from an in-memory demo inventory and never call a TV. This module also holds
//! the Home integration's launch, lifecycle, crash-loop and self-update helpers.
use crate::{
    app_artwork,
    model::{App, Input, Inventory, demo_inventory, valid_app_id},
};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::mpsc, thread};

#[path = "platform-install.rs"]
mod installation;

/// Astro's webOS app ID.
pub const APP_ID: &str = "com.danielcanoh22.astro";
/// Launch parameter Home adds when it opens `com.danielcanoh22.astro` for its TV input: that copy
/// then runs itself instead of redirecting to Home.
pub const LAUNCHED_BY_HOME: &str = "astroFromHome";

/// Whether webOS launch metadata (`argv[1]`) carries `LAUNCHED_BY_HOME`, at its top level or
/// under `params`.
pub fn launched_by_home(metadata: &Value) -> bool {
    metadata[LAUNCHED_BY_HOME] == true || metadata["params"][LAUNCHED_BY_HOME] == true
}

/// Whether webOS preloaded this process (`argv[1]` `"preload": "full"` or `"partial"`): sam starts
/// Home in the background so the Home key answers at once, and only a relaunch brings it forward.
pub fn preloaded(metadata: &Value) -> bool {
    match &metadata["preload"] {
        Value::String(mode) => !mode.is_empty(),
        Value::Bool(preload) => *preload,
        _ => false,
    }
}

/// Returns the `applicationManager/launch` payload for `id`; launching Astro itself adds
/// [`LAUNCHED_BY_HOME`] so the new copy runs instead of redirecting to Home.
pub(crate) fn launch_payload(id: &str) -> Value {
    if id == APP_ID {
        json!({"id": id, "params": {LAUNCHED_BY_HOME: true}})
    } else {
        json!({"id": id})
    }
}

/// Opens `com.danielcanoh22.astro` for its TV input. Whether Home is in front was already decided
/// by Home itself (`State::astro_input_due`), from its own window focus.
fn select_astro_input() -> Result<(), String> {
    call(
        "luna://com.webos.applicationManager/launch",
        launch_payload(APP_ID),
    )
    .map(|_| ())
}

/// The EIM input ID LG gives Astro once `astro-helper tv-input enable` registers it.
const ASTRO_INPUT: &str = "MVPD_IP-com.danielcanoh22.astro";
/// LG Home's app ID, which the staged Home integration runs Astro as.
pub const HOME_ID: &str = "com.webos.app.home";

/// While this file exists, the staged Home does not register with sam and is restarted per
/// Home key instead. Astro creates it when a relaunch did not bring it to the
/// front; delete it to try registering again.
#[cfg_attr(
    not(all(feature = "webos", target_os = "linux", target_arch = "arm")),
    allow(dead_code)
)]
pub const HOME_LIFECYCLE_OFF: &str = "/var/lib/astro-home/no-lifecycle";

/// Whether a relaunch actually brought the staged Home to the front. Only the compositor's
/// focus or remote input counts: SDL_ShowWindow emits Shown by itself, so Shown and Restored
/// say nothing about whether the raise reached the screen.
#[derive(Debug, Default)]
pub struct ForegroundCheck {
    deadline: Option<std::time::Instant>,
}

impl ForegroundCheck {
    /// How long a relaunch may take to gain focus or receive remote input.
    pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    /// A relaunch arrived. `in_front`: Astro was already focused and visible, so nothing to check.
    pub fn relaunched(&mut self, now: std::time::Instant, in_front: bool) {
        if !in_front {
            self.deadline = Some(now + Self::TIMEOUT);
        }
    }

    /// The compositor focused Astro, or remote input reached it.
    pub fn confirmed(&mut self) {
        self.deadline = None;
    }

    /// Whether a pending relaunch has failed by `now`. Reports each failure once.
    pub fn failed(&mut self, now: std::time::Instant) -> bool {
        let failed = self.deadline.is_some_and(|deadline| now >= deadline);
        if failed {
            self.deadline = None;
        }
        failed
    }
}

/// What sam asks of a registered Home, from one lifecycle reply. Only TV builds register.
#[cfg_attr(
    not(all(feature = "webos", target_os = "linux", target_arch = "arm")),
    allow(dead_code)
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Bring the running instance to the front instead of a restart.
    Relaunch,
    /// Exit.
    Close,
}

impl Lifecycle {
    /// Reads the event of one lifecycle reply, or `None` when it needs no action.
    ///
    /// Lifecycle v1 (`registerNativeApp`) names the event in `message`.
    #[cfg_attr(
        not(all(feature = "webos", target_os = "linux", target_arch = "arm")),
        allow(dead_code)
    )]
    pub fn from_reply(reply: &Value) -> Option<Self> {
        match reply.get("message").and_then(Value::as_str)? {
            "relaunch" => Some(Self::Relaunch),
            "close" => Some(Self::Close),
            _ => None,
        }
    }
}

/// Directory holding assets/, lib/ and splash/: the installed package next to the executable.
///
/// The result is computed once per process.
///
/// # Errors
///
/// Returns why the executable's directory is not Astro's installed package.
pub fn install_dir() -> Result<std::path::PathBuf, String> {
    static DIRECTORY: std::sync::OnceLock<Result<std::path::PathBuf, String>> =
        std::sync::OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            installation::find_directory(&executable)
        })
        .clone()
}

/// Opens the staged Home instead of this copy when the Home integration is active.
///
/// Returns `true` when Home was launched and this process should exit.
///
/// # Errors
///
/// Returns the Luna error when the Home launch fails.
pub fn redirect_to_home() -> Result<bool, String> {
    // The root-only integration mounts this marker with its native Home
    // manifest. A normal tile launch then opens that same instance instead of
    // starting a second renderer writing the same preferences.
    let marked = |file| std::fs::read_to_string(file).is_ok_and(|marker| marker.trim() == APP_ID);
    if marked("/usr/palm/applications/com.webos.app.home/.astro-home") {
        thread::spawn(|| {
            call(
                "luna://com.webos.applicationManager/launch",
                json!({"id": HOME_ID}),
            )
        })
        .join()
        .map_err(|_| "Home launch worker failed".to_string())??;
        return Ok(true);
    }
    Ok(false)
}

/// Work for the platform worker.
pub enum Request {
    /// Reads the app and input inventory.
    Refresh,
    /// Reads only the inputs, for periodic status refreshes.
    RefreshInputs,
    /// Launches an app by ID.
    Launch(String),
    /// Removes an app through the root helper, sent once and confirmed against the inventory.
    Uninstall(String),
    /// Points LG's launch splash at this background; answers nothing.
    SyncSplash(String),
    /// Read (`None`) or change Astro's TV input registration.
    TvInput(Option<bool>),
    /// Open `com.danielcanoh22.astro` for its TV input, without the launch notice.
    SelectAstroInput,
    /// Lists, launches or closes a recent app; `generation` lets the UI drop stale replies.
    Recent {
        generation: u64,
        command: crate::recents::Command,
    },
}
/// A finished [`Request`], sent back to the SDL thread.
pub enum Response {
    /// The inventory, with a notice when only the inputs failed to refresh.
    Inventory(Result<(Inventory, Option<String>), String>),
    Inputs(Result<Vec<Input>, String>),
    Launched(Result<(), String>),
    Uninstalled {
        id: String,
        result: Result<(), String>,
    },
    AstroInputSelected(Result<(), String>),
    TvInput {
        request: Option<bool>,
        result: Result<bool, String>,
    },
    Recent {
        generation: u64,
        command: crate::recents::Command,
        result: Result<Option<Vec<crate::recents::Entry>>, String>,
    },
}

/// Handle to the platform worker.
pub struct Platform {
    sender: mpsc::Sender<Request>,
    pub receiver: mpsc::Receiver<Response>,
    pub webos: bool,
    splash_enabled: bool,
}

impl Platform {
    /// Starts the worker; `webos` selects TV services over the desktop demo, and
    /// `splash_enabled` allows updating LG's launch splash.
    pub fn new(webos: bool, splash_enabled: bool) -> Self {
        let (sender, requests) = mpsc::channel();
        let (responses, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut splash_background = None;
            let mut desktop_inventory = demo_inventory();
            let mut desktop_tv_input = false;
            let mut desktop_recents = if webos {
                vec![]
            } else {
                crate::recents::demo()
            };
            while let Ok(request) = requests.recv() {
                let response = match request {
                    Request::Recent {
                        generation,
                        command,
                    } => {
                        let result = if webos {
                            recent_call(&command).and_then(|reply| {
                                if command == crate::recents::Command::List {
                                    crate::recents::normalize(&reply).map(Some)
                                } else {
                                    Ok(None)
                                }
                            })
                        } else {
                            // Isolated in-memory history; desktop actions never call a TV.
                            match &command {
                                crate::recents::Command::List => Ok(Some(desktop_recents.clone())),
                                crate::recents::Command::Close(id) => {
                                    desktop_recents.retain(|entry| &entry.app.id != id);
                                    Ok(None)
                                }
                                crate::recents::Command::Launch(_) => Ok(None),
                            }
                        };
                        Response::Recent {
                            generation,
                            command,
                            result,
                        }
                    }
                    Request::Refresh => Response::Inventory(if webos {
                        inventory()
                    } else {
                        Ok((desktop_inventory.clone(), None))
                    }),
                    Request::RefreshInputs => Response::Inputs(if webos {
                        input_inventory()
                    } else {
                        Ok(desktop_inventory.inputs.clone())
                    }),
                    Request::Uninstall(id) => {
                        // Validate again at the worker boundary, not just in the UI.
                        let result = uninstall_request(&id).and_then(|(_, _)| {
                            if webos {
                                let command =
                                    helper_command("uninstall-app", std::slice::from_ref(&id))?;
                                #[cfg(all(
                                    feature = "webos",
                                    target_os = "linux",
                                    target_arch = "arm"
                                ))]
                                let result = crate::luna::call_with_timeout(
                                    "luna://org.webosbrew.hbchannel.service/exec",
                                    &json!({"command": command}),
                                    std::time::Duration::from_secs(20),
                                );
                                #[cfg(not(all(
                                    feature = "webos",
                                    target_os = "linux",
                                    target_arch = "arm"
                                )))]
                                let result = raw_call(
                                    "luna://org.webosbrew.hbchannel.service/exec",
                                    &json!({"command": command}),
                                );
                                let result = result
                                    .and_then(checked_response)
                                    .and_then(|envelope| command_reply(&envelope))
                                    .map(|_| ());
                                if let Err(error) = result {
                                    // A lost completion reply does not imply the mutation
                                    // failed. Reconcile against complete inventory reads,
                                    // never by issuing the uninstall a second time.
                                    let mut confirmed = false;
                                    for attempt in 0..3 {
                                        if attempt > 0 {
                                            thread::sleep(std::time::Duration::from_millis(500));
                                        }
                                        let check = inventory_read(
                                            "luna://com.webos.applicationManager/listApps",
                                            "apps",
                                        )
                                        .and_then(|apps| inventory_confirms_removal(&apps, &id));
                                        match check {
                                            Ok(true) => {
                                                confirmed = true;
                                                break;
                                            }
                                            Ok(false) => {}
                                            Err(read_error) => {
                                                eprintln!(
                                                    "Astro uninstall inventory: {read_error}"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                    if confirmed { Ok(()) } else { Err(error) }
                                } else {
                                    Ok(())
                                }
                            } else {
                                desktop_inventory.apps.retain(|app| app.id != id);
                                desktop_recents.retain(|entry| entry.app.id != id);
                                Ok(())
                            }
                        });
                        Response::Uninstalled { id, result }
                    }
                    Request::Launch(id) => Response::Launched(if webos {
                        if crate::scroll_preview::is_placeholder(&id) {
                            Err("Scroll preview cards cannot be launched".into())
                        } else if !valid_app_id(&id) {
                            Err("Invalid application identifier".into())
                        } else {
                            call(
                                "luna://com.webos.applicationManager/launch",
                                launch_payload(&id),
                            )
                            .map(|_| ())
                        }
                    } else {
                        Ok(())
                    }),
                    Request::SelectAstroInput => Response::AstroInputSelected(if webos {
                        select_astro_input()
                    } else {
                        Ok(())
                    }),
                    Request::TvInput(request) => Response::TvInput {
                        request,
                        result: if webos {
                            tv_input(request)
                        } else {
                            // Desktop previews keep the registration in memory, never a TV's.
                            if let Some(enable) = request {
                                desktop_tv_input = enable;
                            }
                            Ok(desktop_tv_input)
                        },
                    },
                    Request::SyncSplash(background) => {
                        if webos
                            && splash_enabled
                            && splash_background.as_ref() != Some(&background)
                        {
                            match sync_splash(&background) {
                                Ok(()) => splash_background = Some(background),
                                Err(error) => eprintln!("Astro splash: {error}"),
                            }
                        }
                        continue;
                    }
                };
                if responses.send(response).is_err() {
                    break;
                }
                crate::wake::wake();
            }
        });
        Self {
            sender,
            receiver,
            webos,
            splash_enabled,
        }
    }

    /// Queues a request; its response arrives on `receiver`.
    pub fn send(&self, request: Request) {
        let _ = self.sender.send(request);
    }

    /// Whether this run updates LG's launch splash: TV runs with the installed package only.
    pub fn updates_splash(&self) -> bool {
        self.webos && self.splash_enabled
    }

    /// Queues a launch splash update when this run updates the splash.
    ///
    /// An upload's splash image must already be prepared by the wallpaper worker.
    pub fn sync_splash(&self, background: &str) {
        if self.updates_splash() {
            self.send(Request::SyncSplash(background.to_owned()));
        }
    }
}

/// Builds the shell command that runs the packaged `astro-helper` with `operation`.
///
/// Homebrew remains the privilege boundary for normal app jails. Each argument is shell-quoted
/// independently; there is no interpreter source and no untrusted command name.
fn helper_command(operation: &str, arguments: &[String]) -> Result<String, String> {
    let executable = install_dir()?.join("astro-helper");
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut parts = vec![quote(&executable.to_string_lossy()), quote(operation)];
    parts.extend(arguments.iter().map(|argument| quote(argument)));
    Ok(parts.join(" "))
}

/// Quote for the exec service's shell and systemd's ExecStart environment expansion. Shell
/// quoting alone does not protect `$` variables inside the embedded installer script.
/// Doubling dollars does not need `--expand-environment=no`, which older systemd lacks.
/// Leave `%` untouched: systemd-run passes ExecStart over D-Bus, not through unit-file parsing.
fn update_argument(value: &str) -> String {
    let escaped = value.replace('$', "$$");
    format!("'{}'", escaped.replace('\'', "'\\''"))
}

/// Hands a downloaded, verified update to `tv/self-update.sh`, run as root through Homebrew
/// Channel's exec in its own systemd unit: exec ends its children when the call returns, and the
/// install closes Astro.
///
/// # Errors
///
/// Returns an error when the package directory is unknown, the exec call fails, or
/// `systemd-run` exits non-zero. A transport error may still have started the unit.
pub fn start_update(
    release: &crate::update::Release,
    attempt: &str,
    ipk: &std::path::Path,
) -> Result<(), String> {
    let directory = install_dir()?;
    let command = format!(
        "systemd-run --unit=astro-update-{} --collect /bin/sh -c {} self-update {} {} {} {} {}",
        attempt,
        update_argument(include_str!("../tv/self-update.sh")),
        update_argument(&ipk.to_string_lossy()),
        update_argument(&release.sha256),
        update_argument(&release.version),
        update_argument(&directory.to_string_lossy()),
        update_argument(attempt),
    );
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({"command": command}),
    )?;
    // systemd-run prints nothing on stdout once the unit started; only its exit status counts.
    // A transport error can be ambiguous: the worker preserves the IPK and observes the result.
    for key in ["returnCode", "exitCode"] {
        if let Some(code) = envelope.get(key)
            && code != 0
            && code != "0"
        {
            let stderr = envelope.get("stderrString").and_then(Value::as_str);
            return Err(format!(
                "The installer could not be started ({key}={code}): {}",
                output_excerpt(stderr.unwrap_or(""))
            ));
        }
    }
    Ok(())
}

/// Reads the installer's result for `attempt`, or the latest result for `None`.
///
/// Read only the root-owned result; the caller matches its unguessable attempt ID. Returns
/// `Ok(None)` while no result exists.
///
/// # Errors
///
/// Returns an error for a malformed attempt ID, a failed exec call or an unparsable result.
pub fn update_result_for(
    attempt: Option<&str>,
) -> Result<Option<crate::update::InstallResult>, String> {
    let file = match attempt {
        Some(id) if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) => {
            format!("result.{id}.json")
        }
        Some(_) => return Err("Invalid update attempt".into()),
        None => "result.json".into(),
    };
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({
            "command": format!("cat /var/lib/astro-update/{file} 2>/dev/null || true")
        }),
    )?;
    let output = envelope
        .get("stdoutString")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if output.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(output)
        .map(Some)
        .map_err(|e| format!("Invalid installer result: {e}"))
}

/// Counts this start of Astro as Home. After repeated starts that never became healthy, it starts
/// the fallback to LG Home until reboot and returns true: the caller exits. The fallback runs in
/// its own systemd unit because restoring LG Home stops this process.
pub fn home_fallback_due() -> bool {
    #[cfg(all(feature = "webos", unix))]
    {
        use astro_tv_helper::home;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        match home::record_start(&astro_tv_helper::system::Tv) {
            Ok(home::Start::Continue) => {}
            Ok(home::Start::Fallback) => {
                eprintln!(
                    "Astro Home: {} starts failed; giving LG Home back until reboot",
                    home::FAILURE_LIMIT
                );
                // The installed package's script and helper, not the copies in
                // /var/lib/astro-home: only `enable` refreshes those, so they can predate
                // `fallback` after a Homebrew Channel or manual upgrade.
                let started = install_dir().and_then(|directory| {
                    std::process::Command::new("systemd-run")
                        .args(["--unit=astro-home-fallback", "--collect", "/bin/sh"])
                        .arg(directory.join("home.sh"))
                        .arg("fallback")
                        .status()
                        .map_err(|error| error.to_string())
                });
                match started {
                    Ok(status) if status.success() => return true,
                    // Keep running: exiting without a fallback would only start Astro again.
                    Ok(status) => eprintln!("Astro Home: fallback not started ({status})"),
                    Err(error) => eprintln!("Astro Home: fallback not started: {error}"),
                }
            }
            Err(error) => eprintln!("Astro Home: start not counted: {error}"),
        }
    }
    false
}

/// How long Astro must keep running after its first frame for a start as Home to count as
/// healthy. Short, because an unregistered Home restarts on every Home key press.
pub const HOME_HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Astro ran long enough as Home: its next start begins a new failure count.
pub fn record_home_healthy() {
    #[cfg(all(feature = "webos", unix))]
    thread::spawn(|| {
        if let Err(error) = astro_tv_helper::home::record_healthy(&astro_tv_helper::system::Tv) {
            eprintln!("Astro Home: health not recorded: {error}");
        }
    });
}

/// Points LG's launch splash at `background` through the root helper.
///
/// # Errors
///
/// Returns the helper's error, or an error when the splash it reports differs.
fn sync_splash(background: &str) -> Result<(), String> {
    let directory = install_dir()?;
    // The executable/manifest Home already runs privileged. Avoid starting another process or
    // queuing a Homebrew call behind its startup; file work still stays on the platform worker.
    #[cfg(all(feature = "webos", unix))]
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return astro_tv_helper::files::sync_splash(
            &astro_tv_helper::system::Tv,
            &directory,
            background,
        )
        .map(|_| ());
    }
    let command = helper_command(
        "sync-splash",
        &[json!({"directory":directory,"background":background}).to_string()],
    )?;
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({"command": command}),
    )?;
    let saved = inventory_payload(&envelope, "background")?;
    if saved.as_str() != Some(background) {
        return Err("Splash background was not updated".into());
    }
    Ok(())
}

/// Reads or changes whether LG lists Astro as a TV input, through the packaged native helper
/// (`astro-helper tv-input`). It verifies LG's input list itself and rolls back an unlisted add.
fn tv_input(request: Option<bool>) -> Result<bool, String> {
    let command = helper_command(
        "tv-input",
        &[match request {
            None => "status",
            Some(true) => "enable",
            Some(false) => "disable",
        }
        .to_owned()],
    )?;
    let payload = json!({"command": command});
    // Up to three EIM calls of at most 4 s each: longer than the default Luna timeout.
    #[cfg(all(feature = "webos", target_os = "linux", target_arch = "arm"))]
    let envelope = crate::luna::call_with_timeout(
        "luna://org.webosbrew.hbchannel.service/exec",
        &payload,
        std::time::Duration::from_secs(20),
    );
    #[cfg(not(all(feature = "webos", target_os = "linux", target_arch = "arm")))]
    let envelope = raw_call("luna://org.webosbrew.hbchannel.service/exec", &payload);
    inventory_payload(&envelope.and_then(checked_response)?, "registered")?
        .as_bool()
        .ok_or_else(|| "Invalid TV input status".into())
}

/// Reports whether Astro offers to uninstall `id`.
///
/// Protect launcher identities and vendor system namespaces, never titles.
/// Conservatively protects vendor apps even when removable, since the firmware
/// reports no reliable removability flag.
pub fn can_uninstall(id: &str) -> bool {
    astro_tv_helper::removable(id) && !crate::scroll_preview::is_placeholder(id)
}

/// Returns the Luna removal call the root helper makes for `id`.
///
/// # Errors
///
/// Fails when `id` is protected (see [`can_uninstall`]).
pub fn uninstall_request(id: &str) -> Result<(&'static str, Value), String> {
    if !can_uninstall(id) {
        return Err("This application is protected from deletion".into());
    }
    // The elevated helper subscribes to normal remove and waits for status 31.
    // Homebrew's uninstall uses dev/remove, which rejects Content Store apps.
    // Send once: an ambiguous timeout must never automatically retry a mutation.
    Ok((
        "luna://com.webos.appInstallService/remove",
        json!({"id": id, "subscribe": true}),
    ))
}

/// Reports whether an unfiltered `listApps` reply no longer contains `id`.
///
/// Use the unfiltered inventory: a hidden/system app must not look uninstalled
/// merely because it would be excluded from Astro's visible catalogue.
///
/// # Errors
///
/// Fails for an oversized list or any entry without a valid ID, which proves nothing.
pub fn inventory_confirms_removal(apps: &Value, id: &str) -> Result<bool, String> {
    let apps = apps
        .as_array()
        .filter(|apps| apps.len() <= 1000)
        .ok_or("Invalid app inventory while confirming deletion")?;
    let mut present = false;
    for app in apps {
        let app_id = app
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_app_id(id))
            .ok_or("Incomplete app inventory while confirming deletion")?;
        present |= app_id == id;
    }
    Ok(!present)
}

/// Makes one native Luna call; desktop builds always fail.
fn raw_call(uri: &str, payload: &Value) -> Result<Value, String> {
    #[cfg(all(feature = "webos", target_os = "linux", target_arch = "arm"))]
    let response = crate::luna::call(uri, payload);
    #[cfg(not(all(feature = "webos", target_os = "linux", target_arch = "arm")))]
    let response: Result<Value, String> = {
        let _ = (uri, payload);
        Err("Native Luna services are only available in an ARM webOS build".into())
    };
    response
}

/// Makes a Luna call that must answer `returnValue: true`, logging failures with the URI.
fn call(uri: &str, payload: Value) -> Result<Value, String> {
    raw_call(uri, &payload)
        .and_then(checked_response)
        .map_err(|error| {
            let error = format!("{uri}: {error}");
            eprintln!("Astro TV service: {error}");
            error
        })
}

/// Passes a Luna reply with `returnValue: true`; otherwise returns its `errorText`.
fn checked_response(value: Value) -> Result<Value, String> {
    if value.get("returnValue").and_then(Value::as_bool) != Some(true) {
        return Err(value
            .get("errorText")
            .and_then(Value::as_str)
            .unwrap_or("The TV denied this action")
            .chars()
            .take(240)
            .collect());
    }
    Ok(value)
}

/// Reads `field` from a root `luna-send -n 1` call of `uri`, run through Homebrew's exec.
fn inventory_read(uri: &str, field: &str) -> Result<Value, String> {
    // Fixed inventory commands use Homebrew's elevated service. A non-root exec answers
    // with a Luna-shaped refusal instead of a misleading empty reply.
    let command = format!(
        "if [ \"$(id -u)\" = 0 ]; then /usr/bin/luna-send -n 1 -w 4000 {uri} '{{}}'; else printf '%s\\n' '{{\"returnValue\":false,\"errorText\":\"Homebrew Channel needs root access\"}}'; fi"
    );
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({"command": command}),
    )?;
    inventory_payload(&envelope, field).map_err(|error| {
        let error = format!("{uri}: {error}");
        eprintln!("Astro TV inventory: {error}");
        error
    })
}

/// Reads an app's installed icon files through the root helper, decoding its hex reply.
///
/// # Errors
///
/// Fails on desktop builds, on a helper error, or when the reply is malformed or exceeds
/// [`app_artwork::MAX_FILE`] in total.
pub fn read_app_icons(source: &app_artwork::Source) -> Result<Vec<Vec<u8>>, String> {
    if !cfg!(feature = "webos") {
        return Err("Installed artwork is only read on TV builds".into());
    }
    let command = helper_command(
        "read-app-icon",
        &[serde_json::to_string(source).map_err(|e| e.to_string())?],
    )?;
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({"command": command}),
    )?;
    let icons = inventory_payload(&envelope, "icons")?;
    let mut remaining = app_artwork::MAX_FILE;
    icons
        .as_array()
        .ok_or("Invalid installed icon reply")?
        .iter()
        .map(|value| {
            let hex = value.as_str().ok_or("Invalid installed icon data")?;
            if hex.len() % 2 != 0 || hex.len() / 2 > remaining {
                return Err("Installed icon reply exceeds its size limit".into());
            }
            remaining -= hex.len() / 2;
            hex.as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    let digit = |b: u8| {
                        (b as char)
                            .to_digit(16)
                            .ok_or("Invalid installed icon encoding")
                    };
                    Ok((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
                })
                .collect()
        })
        .collect()
}

/// Flattens command output to one line of at most 320 characters for error messages.
fn output_excerpt(output: &str) -> String {
    output
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .take(320)
        .collect()
}

/// Returns `field` from the Luna reply inside an exec envelope (see [`command_reply`]).
fn inventory_payload(envelope: &Value, field: &str) -> Result<Value, String> {
    let response = command_reply(envelope)?;
    response
        .get(field)
        .cloned()
        .ok_or_else(|| format!("Missing TV inventory field: {field}"))
}

/// Extracts and checks the Luna reply a command printed inside a Homebrew exec envelope.
///
/// # Errors
///
/// Fails for a non-zero exit, empty or oversized output, output without a Luna reply
/// object, or a reply without `returnValue: true`; the message includes output excerpts.
fn command_reply(envelope: &Value) -> Result<Value, String> {
    let stdout = envelope
        .get("stdoutString")
        .and_then(Value::as_str)
        .unwrap_or("");
    let stderr = envelope
        .get("stderrString")
        .and_then(Value::as_str)
        .unwrap_or("");
    let failure = |reason: &str| {
        format!(
            "{reason}; stdout: {}; stderr: {}",
            output_excerpt(stdout),
            output_excerpt(stderr),
        )
    };
    for key in ["returnCode", "exitCode"] {
        if let Some(code) = envelope.get(key)
            && code != 0
            && code != "0"
        {
            return Err(failure(&format!(
                "Homebrew inventory command failed ({key}={code})"
            )));
        }
    }
    if stdout.trim().is_empty() {
        return Err(failure("Homebrew returned no inventory"));
    }
    if stdout.len() > 2 * 1024 * 1024 {
        return Err("Homebrew inventory exceeds the 2 MiB limit".into());
    }
    // Parse an entire JSON object, including pretty-printed multi-line replies.
    // A stream parser permits CLI timing text after that object; a preamble can
    // precede its opening brace. Never mistake a nested one-line app for a reply.
    let start = stdout
        .find('{')
        .ok_or_else(|| failure("Homebrew inventory contains no JSON object"))?;
    let response = serde_json::Deserializer::from_str(&stdout[start..])
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| failure("Homebrew inventory contains no JSON reply"))?
        .map_err(|error| {
            failure(&format!(
                "Homebrew inventory JSON could not be parsed: {error}"
            ))
        })?;
    if !response.is_object() || !response.get("returnValue").is_some_and(Value::is_boolean) {
        return Err(failure("Homebrew inventory JSON is not a Luna response"));
    }
    checked_response(response)
}

/// Runs a recent-apps command as a root `luna-send -n 1` through Homebrew's exec.
fn recent_call(command: &crate::recents::Command) -> Result<Value, String> {
    let (uri, payload) = command.request()?;
    // The native process uses its own authenticated Homebrew connection, as
    // inventory does. Fixed operations only; no app-ID impersonation or retries
    // of mutations after an ambiguous timeout. LG owns close/keep-alive policy.
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let command = format!(
        "if [ \"$(id -u)\" = 0 ]; then /usr/bin/luna-send -n 1 -w 4000 {uri} {}; else printf '%s\\n' '{{\"returnValue\":false,\"errorText\":\"Homebrew Channel needs root access\"}}'; fi",
        quote(&payload.to_string())
    );
    let envelope = call(
        "luna://org.webosbrew.hbchannel.service/exec",
        json!({"command": command}),
    )?;
    command_reply(&envelope)
}

/// Reads apps and inputs; an input failure keeps the apps and becomes a notice.
fn inventory() -> Result<(Inventory, Option<String>), String> {
    let apps = normalize_apps(&inventory_read(
        "luna://com.webos.applicationManager/listApps",
        "apps",
    )?)?;
    let inputs = input_inventory();
    match inputs {
        Ok(inputs) => {
            eprintln!(
                "Astro TV inventory: {} apps, {} inputs",
                apps.len(),
                inputs.len()
            );
            Ok((Inventory { apps, inputs }, None))
        }
        Err(error) => Ok((
            Inventory {
                apps,
                inputs: vec![],
            },
            Some(format!("Inputs could not be refreshed: {error}")),
        )),
    }
}

/// Reads the inputs from EIM's `getAllInputStatus`.
fn input_inventory() -> Result<Vec<Input>, String> {
    inventory_read("luna://com.webos.service.eim/getAllInputStatus", "devices")
        .and_then(|raw| normalize_inputs(&raw))
}

/// Returns a trimmed title without control characters, at most 120 characters, or
/// `fallback` when nothing is left.
fn title(value: Option<&str>, fallback: &str) -> String {
    let clean: String = value
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if clean.trim().is_empty() {
        fallback.into()
    } else {
        clean.trim().into()
    }
}

/// Turns a `listApps` reply into the visible app list, sorted by title.
///
/// Skips Astro, LG Home, input apps, hidden apps (Live TV excepted), vendor apps not marked
/// visible, invalid IDs and duplicates.
///
/// # Errors
///
/// Fails when the reply is not an array of at most 1000 entries.
pub fn normalize_apps(raw: &Value) -> Result<Vec<App>, String> {
    let entries = raw
        .as_array()
        .filter(|a| a.len() <= 1000)
        .ok_or("Invalid app inventory")?;
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_app_id(id))
        else {
            continue;
        };
        let live = id == "com.webos.app.livetv";
        let visible = entry.get("visible");
        if id == APP_ID
            || id == HOME_ID
            || id.starts_with("com.webos.app.hdmi")
            || id.starts_with("com.webos.app.externalinput.")
            || (!live && (visible == Some(&json!(false)) || visible == Some(&json!("false"))))
            || (!live && astro_tv_helper::vendor_app(id) && visible != Some(&json!(true)))
            || !seen.insert(id)
        {
            continue;
        }
        let mut app = App::new(id, &title(entry.get("title").and_then(Value::as_str), id));
        app.icon = Some(app_artwork::Source::new(id, entry));
        apps.push(app);
    }
    apps.sort_by_key(|a| a.title.to_lowercase());
    Ok(apps)
}

/// Turns a `getAllInputStatus` device list into HDMI inputs plus Astro's own registered input.
///
/// Entries whose app ID, input ID or port disagree with their HDMI number, or that appear
/// twice, are skipped.
///
/// # Errors
///
/// Fails when the reply is not an array of at most 128 entries.
pub fn normalize_inputs(raw: &Value) -> Result<Vec<Input>, String> {
    let entries = raw
        .as_array()
        .filter(|a| a.len() <= 128)
        .ok_or("Invalid input inventory")?;
    let mut inputs = Vec::new();
    for entry in entries {
        let Some(id) = entry.get("appId").and_then(Value::as_str) else {
            continue;
        };
        // Astro's own registered input (`astro-helper tv-input`). webOS 25 leaves it out of LG's
        // input picker, so this card is the way to select it and make it the TV's last input.
        if id == APP_ID {
            if entry.get("id").and_then(Value::as_str) == Some(ASTRO_INPUT)
                && !inputs.iter().any(|input: &Input| input.id == APP_ID)
            {
                inputs.push(Input {
                    id: APP_ID.into(),
                    title: "Astro".into(),
                    port: "Astro".into(),
                    connected: Some(true),
                    signal: None,
                });
            }
            continue;
        }
        let Some(number) = id
            .strip_prefix("com.webos.app.hdmi")
            .and_then(|s| s.parse::<u8>().ok())
            .filter(|n| *n > 0 && *n < 100)
        else {
            continue;
        };
        if id != format!("com.webos.app.hdmi{number}")
            || entries
                .iter()
                .filter(|e| e.get("appId").and_then(Value::as_str) == Some(id))
                .count()
                != 1
        {
            continue;
        }
        if entry
            .get("id")
            .is_some_and(|v| v != &format!("HDMI_{number}"))
        {
            continue;
        }
        if entry
            .get("port")
            .is_some_and(|v| v != &json!(number) && v != &number.to_string())
        {
            continue;
        }
        let port = format!("HDMI {number}");
        inputs.push(Input {
            id: id.into(),
            title: title(entry.get("label").and_then(Value::as_str), &port),
            port,
            signal: entry.get("hdmiSignalExist").and_then(Value::as_bool),
            // The TV can report `connected: true` for every existing port;
            // `hdmiPlugIn` is the actual cable state where it is provided.
            connected: entry
                .get("hdmiPlugIn")
                .and_then(Value::as_bool)
                .or_else(|| entry.get("connected").and_then(Value::as_bool)),
        });
    }
    Ok(inputs)
}

#[cfg(test)]
mod update_argument_tests {
    use super::*;

    #[test]
    fn installer_variables_are_escaped_and_printf_formats_are_preserved() {
        let script = include_str!("../tv/self-update.sh");
        let argument = update_argument(script);
        assert!(argument.contains("$${#ATTEMPT}"));
        assert!(argument.contains("$$(date +%T)"));
        assert!(argument.contains("$$IPK"));
        assert!(argument.contains("%s"));
        assert!(!argument.contains("%%s"));
    }

    #[test]
    fn exec_shell_preserves_systemd_escapes_and_argument_boundaries() {
        for value in [
            include_str!("../tv/self-update.sh"),
            "path with 'quotes' and $VARIABLE ${BRACES} $(command) %n %% \\slashes",
        ] {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("printf '%s' {}", update_argument(value))])
                .output()
                .unwrap();
            assert!(output.status.success());
            let argument = String::from_utf8(output.stdout).unwrap();
            assert_eq!(argument, value.replace('$', "$$"));
            // systemd reduces each escaped dollar pair, leaving expansion to /bin/sh.
            assert_eq!(argument.replace("$$", "$"), value);
        }
    }
}

#[cfg(test)]
mod home_inventory_tests {
    use super::*;
    #[test]
    fn lg_home_is_hidden_from_application_inventory() {
        let raw = json!([{"id":HOME_ID,"title":"Home","visible":true}]);
        assert!(normalize_apps(&raw).unwrap().is_empty());
    }
}
