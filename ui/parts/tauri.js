// The two ways the page talks to the Rust side.
//
//   invoke("command_name", {args})  calls a #[tauri::command] function and awaits its result
//   listen("run-event", handler)    receives events the Rust side emits while agents work
//
// Both come from window.__TAURI__, which Tauri puts on the page itself (see
// "withGlobalTauri" in src-tauri/tauri.conf.json). Everything else in the page
// imports them from here, so there is one place to look for them.

export const { invoke } = window.__TAURI__.core;
export const { listen } = window.__TAURI__.event;

// Desktop notifications. The plugin is optional: if it is not there (an older
// build, or a browser), these do nothing rather than throwing.
const notification = window.__TAURI__.notification;

/// Whether we may notify, asking macOS the first time.
///
/// macOS shows its own "Allow notifications?" box the first time an app asks,
/// and never again: after that the answer lives in System Settings. So the
/// moment we ask matters, and we ask when a run starts rather than at launch.
export async function canNotify() {
  if (!notification) return false;
  try {
    if (await notification.isPermissionGranted()) return true;
    return (await notification.requestPermission()) === "granted";
  } catch (error) {
    return false;
  }
}

/// Send one if we are allowed. Returns whether it went out, which the Settings
/// window uses to tell the user where they stand.
export async function notify(title, body) {
  if (!(await canNotify())) return false;
  notification.sendNotification({ title, body });
  return true;
}
