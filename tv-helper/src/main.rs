//! `astro-helper`, the command-line entry point of the root TV helper (see [`astro_tv_helper`]).
//!
//! Each command prints one JSON reply on stdout and exits 0, or prints
//! `Astro TV helper: <error>` on stderr and exits 1. `uninstall-app` instead reports a failed
//! removal in its JSON reply (see [`uninstall::remove`]). Every command requires an effective
//! user of root.
//!
//! - `home COMMAND [APP_DIR]`: the optional Home integration (`status`, `enable`, `apply`,
//!   `disable`, `restore`, `fallback`), normally run through `tv/home.sh`; see
//!   [`home::dispatch`].
//! - `read-app-icon JSON`: an [`files::IconSource`] in, the hex-encoded icon files of that
//!   installed app out, for Astro's artwork worker in its jail.
//! - `sync-splash JSON`: `{"directory", "background"}`; copies the selected background over the
//!   package's `splash.png` (see [`files::sync_splash`]).
//! - `uninstall-app ID`: removes an app and waits for the installer to confirm it (see
//!   [`uninstall::remove`]).
//! - `tv-input COMMAND [APP_DIR]`: `status`, `enable` or `disable` Astro as a TV input (see
//!   [`input::configure`]). `APP_DIR` defaults to the executable's directory.
//!
//! Host builds compile every command for the tests but refuse to run them: only an ARM webOS
//! build with the `webos` feature touches the system.

// The webOS SDK's libc lacks `getauxval`; Astro's shim provides it in ARM builds.
#[cfg(all(feature = "webos", target_os = "linux", target_arch = "arm"))]
#[path = "../../src/webos-compat.rs"]
mod webos_compat;
use astro_tv_helper::{
    files, home, input,
    system::{System, Tv},
    uninstall,
};
use serde_json::Value;
fn run() -> astro_tv_helper::Result<Value> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help") {
        println!(
            "astro-helper: home COMMAND [APP_DIR] | read-app-icon JSON | sync-splash JSON | uninstall-app ID | tv-input COMMAND [APP_DIR]"
        );
        return Ok(serde_json::json!({"returnValue":true}));
    }
    // Host builds compile all operations for fixtures, but cannot mutate a developer machine.
    if !cfg!(all(
        feature = "webos",
        target_os = "linux",
        target_arch = "arm"
    )) {
        return Err("TV helper commands require an ARM webOS build".into());
    }
    let arg = |i| {
        args.get(i)
            .map(String::as_str)
            .ok_or_else(|| "Missing helper argument".to_string())
    };
    let sys = Tv;
    match arg(0)? {
        "read-app-icon" => {
            astro_tv_helper::system::root(&sys)?;
            let source = serde_json::from_str(arg(1)?).map_err(|e| e.to_string())?;
            files::read_icons(&sys, &source)
        }
        "sync-splash" => {
            astro_tv_helper::system::root(&sys)?;
            let request: Value = serde_json::from_str(arg(1)?).map_err(|e| e.to_string())?;
            files::sync_splash(
                &sys,
                std::path::Path::new(request["directory"].as_str().ok_or("Missing directory")?),
                request["background"].as_str().ok_or("Missing background")?,
            )
        }
        "uninstall-app" => Ok(uninstall::remove(arg(1)?, sys.uid())),
        "home" => home::dispatch(&sys, arg(1)?, args.get(2).map(String::as_str)),
        "tv-input" => {
            let directory = args
                .get(2)
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
                })
                .ok_or("Cannot determine installation")?;
            input::configure(&sys, arg(1)?, &directory)
        }
        _ => Err("Unknown TV helper command".into()),
    }
}
fn main() {
    match run() {
        Ok(reply) => println!("{reply}"),
        Err(error) => {
            eprintln!("Astro TV helper: {error}");
            std::process::exit(1);
        }
    }
}
