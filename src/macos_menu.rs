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
use tokio::sync::watch;
use tracing::{error, info};

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

pub fn run(config: Config) -> Result<()> {
    let mtm = MainThreadMarker::new()
        .expect("macos_menu::run must be invoked on the main thread");

    // Daemon on worker thread
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker_cfg = config.clone();
    let worker = std::thread::spawn(move || {
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

    // Handler
    let log_path = std::env::current_dir()
        .unwrap_or_default()
        .join("runner.log");
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
    let status_item: Retained<NSStatusItem> =
        unsafe { status_bar.statusItemWithLength(-1.0) };

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
        let _: () = msg_send![&info, setTitle: &*NSString::from_str(&format!("fucina — {}", config.name))];
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

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let login_item = add_item(
        mtm,
        &menu,
        "Launch at Login",
        sel!(toggleLogin:),
        &handler,
    );
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
