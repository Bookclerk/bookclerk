//! StatusNotifierItem tray via `ksni` (no GTK).

use std::sync::{Arc, Mutex};

use bookclerk_config::Config;
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, ToolTip, Tray};

use crate::daemon::DaemonHandle;
use crate::icon;

pub struct BookclerkTray {
    daemon: Arc<Mutex<DaemonHandle>>,
    config: Arc<Config>,
    icon: ksni::Icon,
}

impl BookclerkTray {
    pub fn new(daemon: DaemonHandle, config: Config) -> Self {
        Self {
            daemon: Arc::new(Mutex::new(daemon)),
            config: Arc::new(config),
            icon: icon::tray_icon(),
        }
    }

    pub fn run(self) -> anyhow::Result<()> {
        let _handle = self.spawn()?;
        // Keep the process (and any daemon we spawned) alive until Quit.
        loop {
            std::thread::park();
        }
    }
}

impl Tray for BookclerkTray {
    fn id(&self) -> String {
        "bookclerk-tray".into()
    }

    fn title(&self) -> String {
        "Bookclerk".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Bookclerk".into(),
            description: "Open the library web UI in your browser".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Ok(daemon) = self.daemon.lock() {
            if let Err(err) = daemon.open_ui() {
                eprintln!("bookclerk-tray: open UI failed: {err}");
            }
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let daemon_open = Arc::clone(&self.daemon);
        let daemon_scan = Arc::clone(&self.daemon);
        let config_scan = Arc::clone(&self.config);
        let daemon_token = Arc::clone(&self.daemon);
        let config_token = Arc::clone(&self.config);
        let daemon_quit = Arc::clone(&self.daemon);

        vec![
            StandardItem {
                label: "Open Bookclerk".into(),
                activate: Box::new(move |_| {
                    if let Ok(daemon) = daemon_open.lock() {
                        if let Err(err) = daemon.open_ui() {
                            eprintln!("bookclerk-tray: open UI failed: {err}");
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Scan library".into(),
                activate: Box::new(move |_| {
                    if let Ok(daemon) = daemon_scan.lock() {
                        if let Err(err) = daemon.trigger_scan(&config_scan) {
                            eprintln!("bookclerk-tray: scan failed: {err}");
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Print operator token".into(),
                activate: Box::new(move |_| {
                    if let Ok(daemon) = daemon_token.lock() {
                        daemon.print_operator_token(&config_token);
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(move |_| {
                    if let Ok(mut daemon) = daemon_quit.lock() {
                        daemon.shutdown();
                    }
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
