//! macOS menu-bar host for the fucina daemon.
//!
//! Wraps the daemon inside an `NSApplication` with an `NSStatusItem` so the
//! process registers with LaunchServices and inherits the user session's
//! Local Network Privacy grant (what Terminal.app already has).
//!
//! Menu items:
//!   - Runner identity + instance URL (disabled, informational)
//!   - Open Log             — opens runner.log in the default viewer
//!   - Open Gitea           — opens the Gitea runner page in the browser
//!   - Launch at Login      — toggles the macOS Login Item state
//!   - Quit                 — NSApplication::terminate
//!
//! The Login-item toggle uses `osascript` to drive `System Events`; it's a
//! single shell-out per interaction which beats pulling in ServiceManagement
//! bindings just for this.

use anyhow::Result;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
};

// NSControlStateValue raw values (not re-exported in objc2-app-kit 0.2)
const STATE_ON: isize = 1;
const STATE_OFF: isize = 0;
use objc2_foundation::{MainThreadMarker, NSString};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;
use tracing::{error, info};

// ── Self-update ───────────────────────────────────────────────────────────────

static UPDATE_CHECKING: AtomicBool = AtomicBool::new(false);

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
}

/// Dispatch a closure to the main thread via GCD. Safe to call from any thread.
///
/// `dispatch_get_main_queue()` is a C macro that expands to `&_dispatch_main_q`;
/// there is no exported function symbol, so we reference the queue object directly.
unsafe fn dispatch_on_main<F: FnOnce() + Send + 'static>(f: F) {
    // libdispatch is re-exported by libSystem, which Rust links automatically on macOS.
    extern "C" {
        static _dispatch_main_q: std::ffi::c_void;
        fn dispatch_async_f(
            queue: *const std::ffi::c_void,
            context: *mut std::ffi::c_void,
            work: unsafe extern "C" fn(*mut std::ffi::c_void),
        );
    }
    unsafe extern "C" fn trampoline(ctx: *mut std::ffi::c_void) {
        let f = Box::from_raw(ctx as *mut Box<dyn FnOnce() + Send>);
        f();
    }
    let boxed = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce() + Send>));
    dispatch_async_f(&_dispatch_main_q, boxed as *mut _, trampoline);
}

/// Show a simple one-button NSAlert. Must be called on the main thread.
unsafe fn show_simple_alert(title: &str, body: &str) {
    use objc2::runtime::AnyClass;
    let Some(cls) = AnyClass::get("NSAlert") else {
        return;
    };
    let alert: Retained<AnyObject> = msg_send_id![cls, new];
    let _: () = msg_send![&alert, setMessageText: &*NSString::from_str(title)];
    let _: () = msg_send![&alert, setInformativeText: &*NSString::from_str(body)];
    let _: () = msg_send![&alert, addButtonWithTitle: &*NSString::from_str("OK")];
    let _: isize = msg_send![&alert, runModal];
}

/// Call the GitHub releases API and return the latest tag (e.g. "v0.2.9").
async fn fetch_latest_tag() -> anyhow::Result<String> {
    let release: GhRelease = reqwest::Client::builder()
        .user_agent(concat!("fucina/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get("https://api.github.com/repos/calibrae/fucina/releases/latest")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(release.tag_name)
}

/// Download the signed .pkg for `tag` to ~/Downloads and open it in Installer.app.
fn launch_pkg_installer(tag: String) {
    std::thread::spawn(move || {
        let ver = tag.trim_start_matches('v');
        let url = format!(
            "https://github.com/calibrae/fucina/releases/download/{tag}/fucina-{ver}.pkg"
        );
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                error!("self-update: runtime failed: {e}");
                return;
            }
        };
        let result: anyhow::Result<PathBuf> = rt.block_on(async {
            let bytes = reqwest::Client::builder()
                .user_agent(concat!("fucina/", env!("CARGO_PKG_VERSION")))
                .build()?
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            let path = PathBuf::from(home)
                .join("Downloads")
                .join(format!("fucina-{ver}.pkg"));
            tokio::fs::write(&path, &bytes).await?;
            Ok(path)
        });
        match result {
            Ok(path) => {
                info!("self-update: opening installer at {}", path.display());
                let _ = Command::new("/usr/bin/open").arg(&path).spawn();
            }
            Err(e) => {
                let msg = format!("{e:#}");
                error!("self-update: download failed: {msg}");
                unsafe {
                    dispatch_on_main(move || {
                        show_simple_alert("Download Failed", &msg);
                    });
                }
            }
        }
    });
}

use crate::config::Config;

/// Data the ObjC handler needs to act on menu selections.
pub struct HandlerIvars {
    log_path: PathBuf,
    instance_url: String,
    runner_url: String,
    app_path: String,
    login_item_name: String,
}

declare_class!(
    pub struct Handler;

    unsafe impl ClassType for Handler {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "FucinaMenuHandler";
    }

    impl DeclaredClass for Handler {
        type Ivars = HandlerIvars;
    }

    unsafe impl Handler {
        #[method(openLog:)]
        fn open_log(&self, _sender: Option<&AnyObject>) {
            let _ = Command::new("/usr/bin/open")
                .arg(&self.ivars().log_path)
                .spawn();
        }

        #[method(openRunner:)]
        fn open_runner(&self, _sender: Option<&AnyObject>) {
            let _ = Command::new("/usr/bin/open")
                .arg(&self.ivars().runner_url)
                .spawn();
        }

        #[method(openInstance:)]
        fn open_instance(&self, _sender: Option<&AnyObject>) {
            let _ = Command::new("/usr/bin/open")
                .arg(&self.ivars().instance_url)
                .spawn();
        }

        #[method(checkForUpdates:)]
        fn check_for_updates(&self, _sender: Option<&AnyObject>) {
            if UPDATE_CHECKING.swap(true, Ordering::Relaxed) {
                return; // already in progress
            }
            std::thread::spawn(|| {
                let current = concat!("v", env!("CARGO_PKG_VERSION"));
                let result = tokio::runtime::Runtime::new()
                    .map_err(anyhow::Error::from)
                    .and_then(|rt| rt.block_on(fetch_latest_tag()));
                UPDATE_CHECKING.store(false, Ordering::Relaxed);
                unsafe {
                    dispatch_on_main(move || match result {
                        Err(e) => show_simple_alert("Update Check Failed", &format!("{e:#}")),
                        Ok(ref tag) if tag == current => show_simple_alert(
                            "fucina is up to date",
                            &format!("You're running the latest version ({current})."),
                        ),
                        Ok(tag) => {
                            use objc2::runtime::AnyClass;
                            let install = {
                                let Some(cls) = AnyClass::get("NSAlert") else {
                                    return;
                                };
                                let alert: Retained<AnyObject> = msg_send_id![cls, new];
                                let _: () = msg_send![&alert, setMessageText:
                                    &*NSString::from_str(&format!("Update available: {tag}"))];
                                let _: () = msg_send![&alert, setInformativeText:
                                    &*NSString::from_str(&format!(
                                        "You're running {current}. Install {tag} now?\n\
                                         The signed installer will open automatically."
                                    ))];
                                let _: () = msg_send![&alert, addButtonWithTitle:
                                    &*NSString::from_str("Install")];
                                let _: () = msg_send![&alert, addButtonWithTitle:
                                    &*NSString::from_str("Later")];
                                let r: isize = msg_send![&alert, runModal];
                                r == 1000 // NSAlertFirstButtonReturn
                            };
                            if install {
                                launch_pkg_installer(tag);
                            }
                        }
                    });
                }
            });
        }

        #[method(toggleLogin:)]
        fn toggle_login(&self, sender: Option<&AnyObject>) {
            let enabled = login_item_enabled(&self.ivars().login_item_name);
            let new_state = !enabled;
            if new_state {
                set_login_item(&self.ivars().app_path, true);
            } else {
                set_login_item(&self.ivars().login_item_name, false);
            }
            if let Some(sender) = sender {
                unsafe {
                    let _: () = msg_send![
                        sender,
                        setState: if new_state { STATE_ON } else { STATE_OFF }
                    ];
                }
            }
        }
    }
);

impl Handler {
    fn new(mtm: MainThreadMarker, ivars: HandlerIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send_id![super(this), init] }
    }
}

fn login_item_enabled(name: &str) -> bool {
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to get the name of every login item")
        .output()
        .ok();
    match out {
        Some(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split([',', '\n'])
                .any(|entry| entry.trim().eq_ignore_ascii_case(name))
        }
        None => false,
    }
}

fn set_login_item(path_or_name: &str, enabled: bool) {
    let script = if enabled {
        format!(
            "tell application \"System Events\" to make login item at end with properties {{path:\"{}\", hidden:true}}",
            path_or_name
        )
    } else {
        format!(
            "tell application \"System Events\" to delete login item \"{}\"",
            path_or_name
        )
    };
    let _ = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output();
}

/// Surface the Local Network Privacy prompt using two documented triggers:
/// 1. `[[NSProcessInfo processInfo] hostName]` — unexpectedly but reliably
///    fires the prompt per Apple DTS Quinn on the developer forums.
/// 2. An `NSNetServiceBrowser` kept alive for the life of the app to hold
///    the grant "live".
fn trigger_local_network_prompt(
    mtm: MainThreadMarker,
) -> Option<Retained<objc2::runtime::AnyObject>> {
    use objc2::runtime::AnyClass;

    // (1) hostName — cheap, known prompt trigger
    let proc_cls = AnyClass::get("NSProcessInfo")?;
    unsafe {
        let pi: *mut objc2::runtime::AnyObject = msg_send![proc_cls, processInfo];
        if !pi.is_null() {
            let hn: *mut objc2::runtime::AnyObject = msg_send![pi, hostName];
            if !hn.is_null() {
                info!("touched NSProcessInfo.hostName to trigger Local Network prompt");
            }
        }
    }

    // (2) Bonjour browse — keep it alive as an ongoing LN activity
    let browser_cls = AnyClass::get("NSNetServiceBrowser")?;
    let browser: Retained<objc2::runtime::AnyObject> = unsafe { msg_send_id![browser_cls, new] };
    let service_type = NSString::from_str("_http._tcp.");
    let domain = NSString::from_str("local.");
    unsafe {
        let _: () = msg_send![
            &browser,
            searchForServicesOfType: &*service_type,
            inDomain: &*domain
        ];
    }
    let _ = mtm;
    info!("NSNetServiceBrowser searching _http._tcp");
    Some(browser)
}

pub fn run(config: Config) -> Result<()> {
    let mtm = MainThreadMarker::new().expect("macos_menu::run must be invoked on the main thread");

    // Fire a Bonjour browse so macOS surfaces the Local Network permission
    // prompt attributed to this bundle (BSD-socket connects don't trigger it).
    let _ln_browser = trigger_local_network_prompt(mtm);

    // Daemon on worker thread. Wait a few seconds before touching the
    // network so NSApp has time to register with LaunchServices and any
    // Local Network Privacy prompt has a chance to surface on a stable
    // bundle identity.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker_cfg = config.clone();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                error!("failed to build tokio runtime: {e:#}");
                return;
            }
        };
        if let Err(e) = rt.block_on(crate::run_daemon(worker_cfg, shutdown_rx)) {
            error!("daemon exited with error: {e:#}");
        }
    });

    // NSApplication
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Handler — use the real log file we set up in main::log_file_path
    let log_path = crate::log_file_path().unwrap_or_else(|| PathBuf::from("/tmp/fucina.log"));
    let handler = Handler::new(
        mtm,
        HandlerIvars {
            log_path: log_path.clone(),
            instance_url: config.instance.clone(),
            runner_url: format!("{}/-/admin/actions/runners", config.instance),
            app_path: "/Applications/Fucina.app".to_string(),
            login_item_name: "Fucina".to_string(),
        },
    );

    // Status bar
    let status_bar = unsafe { NSStatusBar::systemStatusBar() };
    let status_item: Retained<NSStatusItem> = unsafe { status_bar.statusItemWithLength(-1.0) };

    let title = NSString::from_str("🔨");
    unsafe {
        let button: *mut AnyObject = msg_send![&status_item, button];
        if !button.is_null() {
            let _: () = msg_send![button, setTitle: &*title];
        }
    }

    // Menu
    let menu = NSMenu::new(mtm);

    let info = NSMenuItem::new(mtm);
    unsafe {
        let _: () =
            msg_send![&info, setTitle: &*NSString::from_str(&format!("fucina — {}", config.name))];
        let _: () = msg_send![&info, setEnabled: false];
    }
    menu.addItem(&info);

    let inst = add_item(
        mtm,
        &menu,
        &format!("→ {}", config.instance),
        sel!(openInstance:),
        &handler,
    );
    let _ = inst; // prevent unused warning

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    add_item(mtm, &menu, "Open Log", sel!(openLog:), &handler);
    add_item(
        mtm,
        &menu,
        "Open Gitea Runners",
        sel!(openRunner:),
        &handler,
    );
    add_item(
        mtm,
        &menu,
        "Check for Updates\u{2026}",
        sel!(checkForUpdates:),
        &handler,
    );

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let login_item = add_item(mtm, &menu, "Launch at Login", sel!(toggleLogin:), &handler);
    if login_item_enabled(&handler.ivars().login_item_name) {
        unsafe {
            let _: () = msg_send![&login_item, setState: STATE_ON];
        }
    }

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Quit"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    menu.addItem(&quit);

    unsafe { status_item.setMenu(Some(&menu)) };
    let _keep_status = status_item;
    let _keep_handler = handler;

    info!("macOS menu-bar host ready");

    unsafe { app.run() };

    let _ = shutdown_tx.send(true);
    info!("waiting for worker to drain...");
    let _ = worker.join();
    Ok(())
}

fn add_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &str,
    action: Sel,
    target: &Retained<Handler>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    unsafe {
        let _: () = msg_send![&item, setTarget: &**target];
    }
    menu.addItem(&item);
    item
}
