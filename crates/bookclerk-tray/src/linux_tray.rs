//! StatusNotifierItem tray via `ksni` (no GTK).

use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};

use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, ToolTip, Tray};

use crate::client::{SharedTrayConfig, TrayConfig};
use crate::icon;

/// StatusNotifierItem tray that opens the UI, triggers scan, and copies the operator token.
pub struct BookclerkTray {
    /// Shared daemon listen URL and operator token used by menu actions.
    client: SharedTrayConfig,
    /// Embedded pixmap shown in the panel.
    icon: ksni::Icon,
    /// Oneshot sender that unblocks [`BookclerkTray::run`] when Hide tray is chosen (does not exit the daemon).
    quit_tx: Arc<Mutex<Option<SyncSender<()>>>>,
}

impl BookclerkTray {
    /// Builds a tray bound to `config` with the default Bookclerk pixmap.
    pub fn new(config: SharedTrayConfig) -> Self {
        Self {
            client: config,
            icon: icon::tray_icon(),
            quit_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawns the SNI host and blocks until Hide tray; the daemon process stays running.
    pub fn run(self) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        *self.quit_tx.lock().expect("quit lock") = Some(tx);
        let _handle = self.spawn()?;
        // Block until Quit tray — do not exit the daemon process.
        let _ = rx.recv();
        Ok(())
    }

    /// Runs `f` with the current tray config; a poisoned lock logs and yields `None`.
    fn with_client<R>(&self, f: impl FnOnce(&TrayConfig) -> R) -> Option<R> {
        match self.client.lock() {
            Ok(guard) => Some(f(&guard)),
            Err(err) => {
                tracing::error!(%err, "tray config lock poisoned");
                None
            }
        }
    }
}

impl Tray for BookclerkTray {
    fn id(&self) -> String {
        "bookclerk".into()
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
        if let Some(Err(err)) = self.with_client(TrayConfig::open_ui) {
            tracing::warn!(%err, "open UI failed");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let open = Arc::clone(&self.client);
        let scan = Arc::clone(&self.client);
        let token = Arc::clone(&self.client);
        let quit_tx = Arc::clone(&self.quit_tx);

        vec![
            StandardItem {
                label: "Open Bookclerk".into(),
                activate: Box::new(move |_| {
                    if let Ok(guard) = open.lock() {
                        if let Err(err) = guard.open_ui() {
                            tracing::warn!(%err, "open UI failed");
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Scan library".into(),
                activate: Box::new(move |_| {
                    if let Ok(guard) = scan.lock() {
                        if let Err(err) = guard.trigger_scan() {
                            tracing::warn!(%err, "scan failed");
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Copy sign-in link".into(),
                activate: Box::new(move |_| {
                    if let Ok(guard) = token.lock() {
                        guard.copy_sign_in_link();
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Hide tray".into(),
                icon_name: "window-close".into(),
                activate: Box::new(move |_| {
                    if let Ok(guard) = quit_tx.lock() {
                        if let Some(tx) = guard.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
