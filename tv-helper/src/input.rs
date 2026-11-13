//! Registers Astro as a TV input with LG's external input manager (`com.webos.service.eim`).
//!
//! Follows the registration and verification contract of upstream PR #124: an input is only added
//! on an explicit `enable` (Settings → System → Show as TV input), and a change counts only once
//! LG's own input list confirms it.
use crate::{
    APP, Result, accepted,
    system::{self, System},
};
use serde_json::{Value, json};
use std::path::Path;
fn eim(system: &impl System, method: &str, payload: Value) -> Result<Value> {
    accepted(system::luna(
        system,
        "com.webos.service.eim",
        method,
        &payload,
    )?)
}
fn listed(system: &impl System) -> Result<bool> {
    let reply = eim(system, "getAllInputStatus", json!({}))?;
    Ok(reply["devices"]
        .as_array()
        .ok_or("Invalid input inventory")?
        .iter()
        .any(|d| d["appId"] == APP))
}
/// Runs a TV-input `command` (`status`, `enable` or `disable`) and replies with whether Astro is
/// registered.
///
/// `enable` requires the installed manifest in `directory` to be Astro's and to declare
/// `supportGIP`. It is idempotent, and an added device that LG does not then list is deleted
/// again, so a firmware that ignores the registration is not left with a half-registered input.
///
/// # Errors
///
/// Fails when not run as root, for an unknown command, when a service call is refused, or when
/// the input list does not reflect the requested state afterwards. A failed rollback says so and
/// asks for `disable` to be retried.
pub fn configure(system: &impl System, command: &str, directory: &Path) -> Result<Value> {
    system::root(system)?;
    match command {
        "status" => return Ok(json!({"returnValue":true,"registered":listed(system)?})),
        "enable" => {
            let app = system::json(system, directory.join("appinfo.json"))?;
            if app["id"] != APP || app["supportGIP"] != true {
                return Err("Install the Astro package with supportGIP first".into());
            }
            if listed(system)? {
                return Ok(json!({"returnValue":true,"registered":true}));
            }
            let result=eim(system,"addDevice",json!({"appId":APP,"pigImage":"","mvpdIcon":"","label":"Astro","description":"Astro Home"})).and_then(|_|if listed(system)?{Ok(())}else{Err("This firmware did not list Astro as an input".into())});
            if let Err(error) = result {
                return match eim(system, "deleteDevice", json!({"appId":APP})) {
                    Ok(_) => Err(format!("{error}; registration removed")),
                    Err(cleanup) => {
                        Err(format!("{error}; cleanup failed: {cleanup}; retry disable"))
                    }
                };
            }
        }
        "disable" => {
            eim(system, "deleteDevice", json!({"appId":APP}))?;
            if listed(system)? {
                return Err("Astro is still registered as an input".into());
            }
        }
        _ => return Err("Unknown TV-input command".into()),
    }
    Ok(json!({"returnValue":true,"registered":command=="enable"}))
}
