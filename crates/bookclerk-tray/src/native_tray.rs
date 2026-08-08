//! Windows / macOS tray via `tray-icon` + `winit` (default features off — no GTK).

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use crate::client::{SharedTrayConfig, TrayConfig};
use crate::icon;

#[derive(Debug)]
enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
}

pub struct BookclerkTray {
    config: SharedTrayConfig,
}

impl BookclerkTray {
    pub fn new(config: SharedTrayConfig) -> Self {
        Self { config }
    }

    pub fn run(self) -> anyhow::Result<()> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        let proxy = event_loop.create_proxy();
        TrayIconEvent::set_event_handler(Some({
            let proxy = proxy.clone();
            move |event| {
                let _ = proxy.send_event(UserEvent::Tray(event));
            }
        }));
        MenuEvent::set_event_handler(Some({
            let proxy = proxy.clone();
            move |event| {
                let _ = proxy.send_event(UserEvent::Menu(event));
            }
        }));

        let mut app = App {
            client: self.config,
            tray_icon: None,
            open_id: None,
            scan_id: None,
            token_id: None,
            quit_id: None,
        };
        event_loop.run_app(&mut app)?;
        Ok(())
    }
}

struct App {
    client: SharedTrayConfig,
    tray_icon: Option<TrayIcon>,
    open_id: Option<tray_icon::menu::MenuId>,
    scan_id: Option<tray_icon::menu::MenuId>,
    token_id: Option<tray_icon::menu::MenuId>,
    quit_id: Option<tray_icon::menu::MenuId>,
}

impl App {
    fn ensure_tray(&mut self) -> anyhow::Result<()> {
        if self.tray_icon.is_some() {
            return Ok(());
        }

        let open_i = MenuItem::new("Open Bookclerk", true, None);
        let scan_i = MenuItem::new("Scan library", true, None);
        let token_i = MenuItem::new("Print operator token", true, None);
        let quit_i = MenuItem::new("Hide tray", true, None);

        let menu = Menu::new();
        menu.append(&open_i)?;
        menu.append(&scan_i)?;
        menu.append(&token_i)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_i)?;

        self.open_id = Some(open_i.id().clone());
        self.scan_id = Some(scan_i.id().clone());
        self.token_id = Some(token_i.id().clone());
        self.quit_id = Some(quit_i.id().clone());

        let icon = icon::tray_icon_rgba()?;
        self.tray_icon = Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Bookclerk")
                .with_icon(icon)
                .build()?,
        );

        #[cfg(target_os = "macos")]
        if let Some(rl) = objc2_core_foundation::CFRunLoop::main() {
            rl.wake_up();
        }

        Ok(())
    }

    fn with_client<R>(&self, f: impl FnOnce(&TrayConfig) -> R) {
        match self.client.lock() {
            Ok(guard) => {
                f(&guard);
            }
            Err(err) => eprintln!("bookclerk: tray config lock poisoned: {err}"),
        }
    }

    fn open_ui(&self) {
        self.with_client(|cfg| {
            if let Err(err) = cfg.open_ui() {
                eprintln!("bookclerk: open UI failed: {err}");
            }
        });
    }

    fn scan(&self) {
        self.with_client(|cfg| {
            if let Err(err) = cfg.trigger_scan() {
                eprintln!("bookclerk: scan failed: {err}");
            }
        });
    }

    fn print_token(&self) {
        self.with_client(TrayConfig::print_operator_token);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init {
            if let Err(err) = self.ensure_tray() {
                eprintln!("bookclerk: failed to create tray icon: {err}");
            }
        }
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.open_ui(),
            UserEvent::Tray(_) => {}
            UserEvent::Menu(event) => {
                if Some(&event.id) == self.open_id.as_ref() {
                    self.open_ui();
                } else if Some(&event.id) == self.scan_id.as_ref() {
                    self.scan();
                } else if Some(&event.id) == self.token_id.as_ref() {
                    self.print_token();
                } else if Some(&event.id) == self.quit_id.as_ref() {
                    self.tray_icon.take();
                    event_loop.exit();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}
