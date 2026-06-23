// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Deskoryn tray UI — a self-contained iced client for the `deskorynd` daemon.
//!
//! Pure Rust, no system webview. The app holds no networking of its own beyond
//! the local control channel ([`ipc`]); it can also launch/stop the daemon
//! ([`daemon`]). Status is polled on a timer (the control channel is
//! request/response, no push).

mod daemon;
mod ipc;

use std::sync::Arc;
use std::time::Duration;

use daemon::{BinInfo, Lifecycle, PairState, ProcMgr};
use iced::widget::{
    button, checkbox, column, container, horizontal_rule, row, scrollable, text, text_input,
    Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme};
use ipc::{Feature, PeerStatus, UiEvent, UiRequest};

pub fn main() -> iced::Result {
    iced::application("Deskoryn", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Light)
        .window_size(iced::Size::new(820.0, 600.0))
        .run_with(App::new)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Status,
    Connection,
    Arranger,
    Devices,
    Transfers,
    Settings,
}

struct Features {
    input: bool,
    clipboard: bool,
    audio: bool,
}

struct App {
    screen: Screen,
    proc: Arc<ProcMgr>,

    reachable: bool,
    device_name: String,
    peers: Vec<PeerStatus>,
    active: bool,
    port: u16,
    transfers: Vec<(String, f32, u64)>,

    life: Lifecycle,
    bin: BinInfo,
    manual_peer: String,
    port_input: String,
    bin_input: String,
    features: Features,
    toast: Option<String>,

    // Pairing
    pairing: PairState,
    pair_listen: bool,
    pair_addr: String,
}

#[derive(Clone, Debug)]
enum Message {
    Nav(Screen),
    Tick,
    StatusLoaded(Result<Vec<UiEvent>, String>),
    LifecycleLoaded(Lifecycle),
    BinLoaded(BinInfo),
    ManualPeerChanged(String),
    PortChanged(String),
    BinInputChanged(String),
    StartDaemon,
    StopDaemon,
    DaemonActed(Result<(), String>),
    SetBin,
    ResetBin,
    SetFeature(Feature, bool),
    Forget(String),
    Acted(Result<Vec<UiEvent>, String>),
    // Pairing
    PairRoleListen(bool),
    PairAddrChanged(String),
    PairStart,
    PairStarted(Result<(), String>),
    PairPoll,
    PairStateLoaded(PairState),
    PairRespond(bool),
    PairClear,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let app = App {
            screen: Screen::Status,
            proc: Arc::new(ProcMgr::default()),
            reachable: false,
            device_name: String::new(),
            peers: Vec::new(),
            active: false,
            port: 0,
            transfers: Vec::new(),
            life: Lifecycle::default(),
            bin: BinInfo { path: None, source: "none", exists: false },
            manual_peer: String::new(),
            port_input: String::new(),
            bin_input: String::new(),
            features: Features { input: true, clipboard: true, audio: false },
            toast: None,
            pairing: PairState::Idle,
            pair_listen: true,
            pair_addr: String::new(),
        };
        let proc = app.proc.clone();
        // Load the persisted binary override, then do a first refresh.
        let boot = Task::perform(async move { proc.load_override().await }, |_| Message::Tick);
        (app, boot)
    }

    fn refresh(&self) -> Task<Message> {
        Task::batch([
            Task::perform(ipc::request(UiRequest::Status), Message::StatusLoaded),
            Task::perform(self.proc.clone().lifecycle(), Message::LifecycleLoaded),
            Task::perform(self.proc.clone().bin_info(), Message::BinLoaded),
        ])
    }

    fn subscription(&self) -> Subscription<Message> {
        let status = iced::time::every(Duration::from_secs(2)).map(|_| Message::Tick);
        // While a pairing is live, poll its state quickly so the SAS code and
        // outcome show promptly.
        if self.pairing != PairState::Idle {
            Subscription::batch([
                status,
                iced::time::every(Duration::from_millis(300)).map(|_| Message::PairPoll),
            ])
        } else {
            status
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Nav(s) => {
                self.screen = s;
                Task::none()
            }
            Message::Tick => self.refresh(),
            Message::StatusLoaded(Ok(events)) => {
                self.reachable = true;
                self.transfers.clear();
                for ev in &events {
                    match ev {
                        UiEvent::Status { device_name, peers, active, port } => {
                            self.device_name = device_name.clone();
                            self.peers = peers.clone();
                            self.active = *active;
                            self.port = *port;
                        }
                        UiEvent::TransferProgress { name, fraction, bytes_per_sec, .. } => {
                            self.transfers.push((name.clone(), *fraction, *bytes_per_sec));
                        }
                        UiEvent::Notice { text, .. } => self.toast = Some(text.clone()),
                        UiEvent::PairingPrompt { .. } => {}
                    }
                }
                Task::none()
            }
            Message::StatusLoaded(Err(_)) => {
                self.reachable = false;
                self.peers.clear();
                self.port = 0; // don't show a port we can't currently confirm
                Task::none()
            }
            Message::LifecycleLoaded(l) => {
                self.life = l;
                Task::none()
            }
            Message::BinLoaded(b) => {
                if self.bin_input.is_empty() {
                    if let Some(p) = &b.path {
                        self.bin_input = p.clone();
                    }
                }
                self.bin = b;
                Task::none()
            }
            Message::ManualPeerChanged(s) => {
                self.manual_peer = s;
                Task::none()
            }
            Message::PortChanged(s) => {
                // Keep only digits so the field always parses as a port.
                self.port_input = s.chars().filter(|c| c.is_ascii_digit()).collect();
                Task::none()
            }
            Message::BinInputChanged(s) => {
                self.bin_input = s;
                Task::none()
            }
            Message::StartDaemon => {
                let connect = (!self.manual_peer.trim().is_empty()).then(|| self.manual_peer.clone());
                let port = self.port_input.parse::<u16>().ok().filter(|p| *p != 0);
                // Clear the previous daemon's port so we don't flash a stale value
                // before the new daemon reports its actual one.
                self.port = 0;
                Task::perform(self.proc.clone().start(connect, port), Message::DaemonActed)
            }
            Message::StopDaemon => {
                self.port = 0;
                Task::perform(self.proc.clone().stop(), Message::DaemonActed)
            }
            Message::DaemonActed(r) => {
                self.toast = Some(match r {
                    Ok(()) => "daemon updated".into(),
                    Err(e) => e,
                });
                // Refresh now, then a couple of quick follow-ups so the running
                // state + listening port appear within ~1s of the daemon binding,
                // rather than waiting for the next steady (2s) poll.
                Task::batch([
                    Task::perform(self.proc.clone().lifecycle(), Message::LifecycleLoaded),
                    Task::perform(
                        async { tokio::time::sleep(Duration::from_millis(600)).await },
                        |_| Message::Tick,
                    ),
                    Task::perform(
                        async { tokio::time::sleep(Duration::from_millis(1500)).await },
                        |_| Message::Tick,
                    ),
                ])
            }
            Message::SetBin => {
                let p = Some(self.bin_input.clone());
                Task::perform(self.proc.clone().set_bin(p), Message::BinLoaded)
            }
            Message::ResetBin => {
                self.bin_input.clear();
                Task::perform(self.proc.clone().set_bin(None), Message::BinLoaded)
            }
            Message::SetFeature(f, enabled) => {
                match f {
                    Feature::InputSharing => self.features.input = enabled,
                    Feature::ClipboardSync => self.features.clipboard = enabled,
                    Feature::AudioForward => self.features.audio = enabled,
                }
                Task::perform(
                    ipc::request(UiRequest::SetFeature { feature: f, enabled }),
                    Message::Acted,
                )
            }
            Message::Forget(device) => Task::perform(
                ipc::request(UiRequest::Forget { device }),
                Message::Acted,
            ),
            Message::Acted(r) => {
                if let Err(e) = &r {
                    self.toast = Some(e.clone());
                }
                Task::perform(ipc::request(UiRequest::Status), Message::StatusLoaded)
            }
            Message::PairRoleListen(b) => {
                self.pair_listen = b;
                Task::none()
            }
            Message::PairAddrChanged(s) => {
                self.pair_addr = s;
                Task::none()
            }
            Message::PairStart => {
                let listen = self.pair_listen;
                let addr = (!listen).then(|| self.pair_addr.clone());
                Task::perform(self.proc.clone().pair_start(listen, addr), Message::PairStarted)
            }
            Message::PairStarted(Ok(())) => {
                Task::perform(self.proc.clone().pair_state(), Message::PairStateLoaded)
            }
            Message::PairStarted(Err(e)) => {
                self.toast = Some(e);
                Task::none()
            }
            Message::PairPoll => {
                Task::perform(self.proc.clone().pair_state(), Message::PairStateLoaded)
            }
            Message::PairStateLoaded(s) => {
                self.pairing = s;
                Task::none()
            }
            Message::PairRespond(accept) => {
                Task::perform(self.proc.clone().pair_respond(accept), |_| Message::PairPoll)
            }
            Message::PairClear => {
                self.pairing = PairState::Idle;
                // Clear the subprocess, then refresh the device list (a new
                // device may have just landed in the trust store).
                Task::batch([
                    Task::perform(self.proc.clone().pair_clear(), |_| Message::PairPoll),
                    Task::perform(ipc::request(UiRequest::Status), Message::StatusLoaded),
                ])
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let content = match self.screen {
            Screen::Status => self.view_status(),
            Screen::Connection => self.view_connection(),
            Screen::Arranger => self.view_arranger(),
            Screen::Devices => self.view_devices(),
            Screen::Transfers => self.view_transfers(),
            Screen::Settings => self.view_settings(),
        };
        let body = scrollable(container(content).padding(20).width(Length::Fill));
        row![self.sidebar(), body].into()
    }

    // --- sidebar ------------------------------------------------------------

    fn sidebar(&self) -> Element<'_, Message> {
        let nav = |label: &str, screen: Screen, current: Screen| {
            let b = button(text(label.to_string()).size(14))
                .width(Length::Fill)
                .on_press(Message::Nav(screen));
            if screen == current {
                b
            } else {
                b.style(button::text)
            }
        };
        let dot = if !self.reachable {
            "○ daemon offline"
        } else if self.peers.iter().any(|p| p.connected) {
            if self.active { "● connected" } else { "◐ connected" }
        } else {
            "○ searching…"
        };
        column![
            text("Deskoryn").size(20),
            Space::with_height(8),
            nav("Status", Screen::Status, self.screen),
            nav("Connection", Screen::Connection, self.screen),
            nav("Arrange monitors", Screen::Arranger, self.screen),
            nav("Devices", Screen::Devices, self.screen),
            nav("Transfers", Screen::Transfers, self.screen),
            nav("Settings", Screen::Settings, self.screen),
            Space::with_height(Length::Fill),
            text(dot.to_string()).size(12),
            text(self.toast.clone().unwrap_or_default()).size(11),
        ]
        .spacing(4)
        .padding(12)
        .width(Length::Fixed(190.0))
        .into()
    }

    // --- screens ------------------------------------------------------------

    fn view_status(&self) -> Element<'_, Message> {
        let mut col = column![text(format!(
            "This workspace{}",
            if self.device_name.is_empty() { String::new() } else { format!(" — {}", self.device_name) }
        ))
        .size(18)]
        .spacing(10);

        if !self.reachable {
            col = col.push(text("The deskorynd daemon isn't reachable.").size(13));
            col = col.push(text("Start it from the Connection tab.").size(12));
            return col.into();
        }
        let connected = self.peers.iter().filter(|p| p.connected).count();
        col = col.push(text(format!("{connected} of {} devices connected", self.peers.len())).size(13));
        if self.peers.is_empty() {
            col = col.push(text("No paired devices yet.").size(13));
        }
        for p in &self.peers {
            let meta = if p.connected {
                p.latency_ms.map(|ms| format!("{ms} ms")).unwrap_or_else(|| "connected".into())
            } else {
                "offline".into()
            };
            col = col.push(row![
                text(if p.connected { "●" } else { "○" }),
                text(p.name.clone()).width(Length::Fill),
                text(meta),
            ].spacing(10));
        }
        col = col.push(horizontal_rule(1));
        col = col.push(text("Ctrl+Alt+L lock cursor here · Ctrl+Alt+S switch machine").size(12));
        col.into()
    }

    fn view_connection(&self) -> Element<'_, Message> {
        let running = self.life.running;

        // Daemon lifecycle. The daemon is symmetric: it listens AND auto-connects
        // to paired peers found on the LAN via mDNS — no client/server split.
        let state = if running {
            if self.port != 0 {
                format!("● running · listening on port {}", self.port)
            } else {
                "● running".to_string()
            }
        } else {
            "○ stopped".to_string()
        };
        let start = if running || !self.bin.exists {
            button("Start daemon")
        } else {
            button("Start daemon").on_press(Message::StartDaemon)
        };
        let stop = if running {
            button("Stop").on_press(Message::StopDaemon).style(button::danger)
        } else {
            button("Stop").style(button::danger)
        };

        // Optional advanced fields, locked while running (they're start-time args).
        let peer_field = text_input("auto-discovered via mDNS (optional)", &self.manual_peer);
        let peer_field = if running { peer_field } else { peer_field.on_input(Message::ManualPeerChanged) };
        let port_field = text_input("auto (optional)", &self.port_input);
        let port_field = if running { port_field } else { port_field.on_input(Message::PortChanged) };

        let daemon = column![
            text("Daemon").size(18),
            text(state).size(13),
            text("Paired devices on the LAN connect automatically (mDNS). Both ends just run.").size(12),
            row![start, stop].spacing(8),
            Space::with_height(6),
            text("Advanced").size(13),
            row![text("Manual peer").width(Length::Fixed(110.0)), peer_field].spacing(8).align_y(Alignment::Center),
            text("Only needed on networks without mDNS (host:port).").size(11),
            row![text("Listen port").width(Length::Fixed(110.0)), port_field].spacing(8).align_y(Alignment::Center),
            text("Leave empty for an OS-assigned port (advertised via mDNS).").size(11),
        ]
        .spacing(8);

        // Binary resolution.
        let bin_state = if self.bin.exists {
            format!("resolved ({})", self.bin.source)
        } else {
            "not found".into()
        };
        let binp = column![
            text("deskorynd binary").size(18),
            text(bin_state).size(13),
            row![
                text_input("/usr/bin/deskorynd", &self.bin_input).on_input(Message::BinInputChanged),
                button("Set").on_press(Message::SetBin),
                button("Auto").on_press(Message::ResetBin),
            ].spacing(8),
        ]
        .spacing(8);

        column![
            daemon,
            horizontal_rule(1),
            binp,
            horizontal_rule(1),
            text("Pairing moves here next (it drives a `deskorynd pair` subprocess).").size(12),
        ]
        .spacing(16)
        .into()
    }

    fn view_arranger(&self) -> Element<'_, Message> {
        column![
            text("Arrange monitors").size(18),
            text("The draggable monitor canvas is the next piece of the iced port.").size(13),
            text("Until then, edit the layout via `deskorynd arrange` on the CLI.").size(12),
        ]
        .spacing(10)
        .into()
    }

    fn view_devices(&self) -> Element<'_, Message> {
        let mut col = column![text("Devices").size(18)].spacing(10);
        if self.peers.is_empty() {
            col = col.push(text("No trusted devices yet.").size(13));
        }
        for p in &self.peers {
            col = col.push(row![
                text(if p.connected { "●" } else { "○" }),
                text(p.name.clone()).width(Length::Fill),
                text(p.address.clone().unwrap_or_default()).size(12),
                button("Forget").style(button::danger).on_press(Message::Forget(p.name.clone())),
            ].spacing(10).align_y(Alignment::Center));
        }
        col = col.push(horizontal_rule(1));
        col = col.push(self.pairing_view());
        col.into()
    }

    fn pairing_view(&self) -> Element<'_, Message> {
        let panel = match &self.pairing {
            PairState::Idle => {
                let wait = button("Wait for a peer")
                    .on_press(Message::PairRoleListen(true))
                    .style(if self.pair_listen { button::primary } else { button::secondary });
                let connect = button("Connect to a peer")
                    .on_press(Message::PairRoleListen(false))
                    .style(if self.pair_listen { button::secondary } else { button::primary });
                let mut c = column![
                    text("Pair a new device").size(16),
                    text("Confirm the same 6-digit code shows on both machines.").size(12),
                    row![wait, connect].spacing(8),
                ]
                .spacing(8);
                if !self.pair_listen {
                    c = c.push(
                        text_input("192.168.1.42:7345", &self.pair_addr)
                            .on_input(Message::PairAddrChanged),
                    );
                }
                if self.life.running {
                    c = c.push(text("Stop the daemon first — pairing uses the same port.").size(12));
                    c = c.push(button("Start pairing")); // disabled while the daemon runs
                } else {
                    c = c.push(button("Start pairing").on_press(Message::PairStart));
                }
                c
            }
            PairState::Waiting => column![
                text("Pairing").size(16),
                text("Waiting for a peer to connect…").size(14),
                button("Cancel").on_press(Message::PairClear),
            ]
            .spacing(8),
            PairState::Connecting => column![
                text("Pairing").size(16),
                text("Connecting…").size(14),
                button("Cancel").on_press(Message::PairClear),
            ]
            .spacing(8),
            PairState::Prompt { sas, peer } => column![
                text(if peer.is_empty() {
                    "Pair with this device?".to_string()
                } else {
                    format!("Pair with “{peer}”?")
                })
                .size(16),
                text("Confirm this code matches on BOTH machines:").size(13),
                text(sas.clone()).size(40),
                text("If they differ, someone may be intercepting — don't continue.").size(12),
                row![
                    button("They don't match").on_press(Message::PairRespond(false)).style(button::danger),
                    button("Confirm").on_press(Message::PairRespond(true)).style(button::primary),
                ]
                .spacing(8),
            ]
            .spacing(10),
            PairState::Done { ok } => column![
                text(if *ok { "Paired ✓" } else { "Pairing aborted" }).size(16),
                button("Close").on_press(Message::PairClear),
            ]
            .spacing(8),
            PairState::Error(msg) => column![
                text(format!("Pairing error: {msg}")).size(14),
                button("Close").on_press(Message::PairClear),
            ]
            .spacing(8),
        };
        panel.into()
    }

    fn view_transfers(&self) -> Element<'_, Message> {
        let mut col = column![text("Transfers").size(18)].spacing(10);
        if self.transfers.is_empty() {
            col = col.push(text("No active transfers.").size(13));
            col = col.push(
                text("Copy files on one machine and paste on the other.").size(12),
            );
        }
        for (name, fraction, bps) in &self.transfers {
            let pct = (fraction * 100.0).round() as u32;
            col = col.push(row![
                text(name.clone()).width(Length::Fill),
                text(format!("{pct}% · {} KB/s", bps / 1024)),
            ].spacing(10));
        }
        col.into()
    }

    fn view_settings(&self) -> Element<'_, Message> {
        let toggle = |label: &str, on: bool, f: Feature| {
            checkbox(label.to_string(), on).on_toggle(move |b| Message::SetFeature(f, b))
        };
        column![
            text("Features").size(18),
            toggle("Input sharing", self.features.input, Feature::InputSharing),
            toggle("Clipboard sync", self.features.clipboard, Feature::ClipboardSync),
            toggle("Audio forwarding", self.features.audio, Feature::AudioForward),
            horizontal_rule(1),
            text("Input, Clipboard, Files, Network, Startup groups mirror config.toml;").size(12),
            text("editing them lives in config.toml until the daemon exposes config over IPC.").size(12),
        ]
        .spacing(10)
        .into()
    }
}
