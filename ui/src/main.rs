// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Deskoryn tray UI - a self-contained iced client for the `deskorynd` daemon.
//!
//! Pure Rust, no system webview. The app holds no networking of its own beyond
//! the local control channel ([`ipc`]); it can also launch/stop the daemon
//! ([`daemon`]). Status is polled on a timer (the control channel is
//! request/response, no push).

mod arranger;
mod daemon;
mod ipc;

use std::sync::Arc;
use std::time::Duration;

use daemon::{BinInfo, Lifecycle, ProcMgr};
use arranger::MonTile;
use iced::widget::{
    button, checkbox, column, container, horizontal_rule, row, scrollable, text, text_input,
    Canvas, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme};
use ipc::{DiscoveredPeer, Feature, PeerStatus, UiEvent, UiRequest};

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
    local_addrs: Vec<String>, // this device's own dialable ip:port (for the manual fallback)
    transfers: Vec<(String, f32, u64)>,

    life: Lifecycle,
    bin: BinInfo,
    manual_peer: String,
    port_input: String,
    bin_input: String,
    features: Features,
    toast: Option<String>,

    // Pairing (driven by the daemon over IPC; we poll PairStatus)
    pair_phase: String, // idle | discoverable | connecting | prompt | paired | aborted | error
    pair_sas: String,
    pair_peer: String,
    pair_addr: String,
    nearby: Vec<DiscoveredPeer>, // devices on the LAN currently accepting pairing

    // Monitor arranger
    arrangement: Vec<MonTile>,
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
    PairAddrChanged(String),
    PairStart,
    PairPoll,
    PairLoaded(Result<Vec<UiEvent>, String>),
    PairRespond(bool),
    PairClear,
    PairDial(String),
    DiscoveredLoaded(Result<Vec<UiEvent>, String>),
    // Monitor arranger
    ArrMoved { idx: usize, x: i32, y: i32 },
    ArrAlignTops,
    ArrRevert,
    ArrApply,
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
            local_addrs: Vec::new(),
            transfers: Vec::new(),
            life: Lifecycle::default(),
            bin: BinInfo { path: None, source: "none", exists: false },
            manual_peer: String::new(),
            port_input: String::new(),
            bin_input: String::new(),
            features: Features { input: true, clipboard: true, audio: false },
            toast: None,
            pair_phase: "idle".into(),
            pair_sas: String::new(),
            pair_peer: String::new(),
            pair_addr: String::new(),
            nearby: Vec::new(),
            arrangement: arranger::starter(),
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
            Task::perform(ipc::request(UiRequest::DiscoveredPeers), Message::DiscoveredLoaded),
        ])
    }

    fn subscription(&self) -> Subscription<Message> {
        let status = iced::time::every(Duration::from_secs(2)).map(|_| Message::Tick);
        // While a pairing is live, poll its state quickly so the SAS code and
        // outcome show promptly.
        if self.pair_phase != "idle" {
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
                        UiEvent::Status { device_name, peers, active, port, addrs } => {
                            self.device_name = device_name.clone();
                            self.peers = peers.clone();
                            self.active = *active;
                            self.port = *port;
                            self.local_addrs = addrs.clone();
                        }
                        UiEvent::TransferProgress { name, fraction, bytes_per_sec, .. } => {
                            self.transfers.push((name.clone(), *fraction, *bytes_per_sec));
                        }
                        UiEvent::Notice { text, .. } => self.toast = Some(text.clone()),
                        UiEvent::Pairing { .. } | UiEvent::Discovered { .. } => {}
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
            Message::PairAddrChanged(s) => {
                self.pair_addr = s;
                Task::none()
            }
            Message::PairStart => {
                // Make this device discoverable (the other side dials it).
                // Optimistic phase so the fast PairStatus poll starts immediately.
                self.pair_phase = "discoverable".into();
                Task::perform(
                    ipc::request(UiRequest::Pair { addr: String::new() }),
                    Message::PairLoaded,
                )
            }
            Message::PairPoll => {
                Task::perform(ipc::request(UiRequest::PairStatus), Message::PairLoaded)
            }
            Message::PairLoaded(Ok(events)) => {
                // Surface any warning (e.g. firewall blocking inbound) as a toast.
                if let Some(UiEvent::Notice { text, .. }) =
                    events.iter().find(|e| matches!(e, UiEvent::Notice { .. }))
                {
                    self.toast = Some(text.clone());
                }
                if let Some(UiEvent::Pairing { phase, sas, peer }) =
                    events.into_iter().find(|e| matches!(e, UiEvent::Pairing { .. }))
                {
                    // Success is terminal: the daemon has already added the peer
                    // to the trust store, so drop straight back to the idle
                    // "Start pairing" view and refresh the device list - no
                    // "Close" step for the user to dismiss.
                    if phase == "paired" {
                        self.pair_phase = "idle".into();
                        self.pair_sas.clear();
                        self.pair_peer.clear();
                        if !peer.is_empty() {
                            self.toast = Some(format!("Paired with {peer}"));
                        }
                        // Reset the daemon's terminal "done" snapshot too, then
                        // refresh the (now larger) trusted-device list.
                        return Task::batch([
                            Task::perform(
                                ipc::request(UiRequest::PairCancel),
                                Message::PairLoaded,
                            ),
                            self.refresh(),
                        ]);
                    }
                    // Don't let a racy/stale "idle" snapshot (the dial reply can
                    // arrive before the handshake updates state) cancel an active
                    // flow - returning to idle is driven locally by PairClear.
                    if phase != "idle" {
                        self.pair_phase = phase;
                        self.pair_sas = sas;
                        self.pair_peer = peer;
                    }
                }
                Task::none()
            }
            Message::PairLoaded(Err(e)) => {
                self.toast = Some(e);
                Task::none()
            }
            Message::PairRespond(accept) => Task::perform(
                ipc::request(UiRequest::PairConfirm { accept }),
                Message::PairLoaded,
            ),
            Message::PairDial(addr) => {
                self.pair_phase = "connecting".into(); // start the fast poll now
                Task::perform(ipc::request(UiRequest::Pair { addr }), Message::PairLoaded)
            }
            Message::DiscoveredLoaded(Ok(events)) => {
                if let Some(UiEvent::Discovered { peers }) =
                    events.into_iter().find(|e| matches!(e, UiEvent::Discovered { .. }))
                {
                    self.nearby = peers;
                }
                Task::none()
            }
            Message::DiscoveredLoaded(Err(_)) => {
                self.nearby.clear();
                Task::none()
            }
            Message::PairClear => {
                self.pair_phase = "idle".into();
                self.pair_sas.clear();
                self.pair_peer.clear();
                Task::batch([
                    Task::perform(ipc::request(UiRequest::PairCancel), Message::PairLoaded),
                    Task::perform(ipc::request(UiRequest::Status), Message::StatusLoaded),
                ])
            }
            Message::ArrMoved { idx, x, y } => {
                if let Some(t) = self.arrangement.get_mut(idx) {
                    t.x = x;
                    t.y = y;
                }
                Task::none()
            }
            Message::ArrAlignTops => {
                let top = self.arrangement.iter().map(|t| t.y).min().unwrap_or(0);
                for t in &mut self.arrangement {
                    t.y = top;
                }
                Task::none()
            }
            Message::ArrRevert => {
                self.arrangement = arranger::starter();
                Task::none()
            }
            Message::ArrApply => {
                let layout = arranger::to_virtual_desktop(&self.arrangement);
                Task::perform(ipc::request(UiRequest::SetLayout { layout }), Message::Acted)
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
            "daemon offline"
        } else if self.peers.iter().any(|p| p.connected) {
            if self.active { "connected" } else { "connected (paused)" }
        } else {
            "searching..."
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
            if self.device_name.is_empty() { String::new() } else { format!(" - {}", self.device_name) }
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
                text(if p.connected { "*" } else { "o" }),
                text(p.name.clone()).width(Length::Fill),
                text(meta),
            ].spacing(10));
        }
        col = col.push(horizontal_rule(1));
        col = col.push(text("Ctrl+Alt+L lock cursor here - Ctrl+Alt+S switch machine").size(12));
        col.into()
    }

    fn view_connection(&self) -> Element<'_, Message> {
        let running = self.life.running;

        // Daemon lifecycle. The daemon is symmetric: it listens AND auto-connects
        // to paired peers found on the LAN via mDNS - no client/server split.
        let state = if running {
            if self.port != 0 {
                format!("running - listening on port {}", self.port)
            } else {
                "running".to_string()
            }
        } else {
            "stopped".to_string()
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
        ]
        .spacing(16)
        .into()
    }

    fn view_arranger(&self) -> Element<'_, Message> {
        let canvas = Canvas::new(arranger::Arranger { tiles: &self.arrangement })
            .width(Length::Fill)
            .height(Length::Fixed(320.0));

        // Virtual-desktop extent, for the readout.
        let (l, t, r, b) = self.arrangement.iter().fold(
            (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
            |(l, t, r, b), m| (l.min(m.x), t.min(m.y), r.max(m.x + m.w), b.max(m.y + m.h)),
        );
        let extent = if self.arrangement.is_empty() {
            "-".to_string()
        } else {
            format!("{} displays - {} x {} virtual", self.arrangement.len(), r - l, b - t)
        };

        column![
            text("Arrange monitors").size(18),
            text("Drag a display to match your physical desk; touching edges become cursor-crossing boundaries.").size(12),
            container(canvas)
                .width(Length::Fill)
                .style(|_: &Theme| container::Style {
                    border: iced::Border { width: 1.0, color: iced::Color::from_rgb8(0xd0, 0xd5, 0xdc), radius: 6.0.into() },
                    ..container::Style::default()
                }),
            text(extent).size(12),
            row![
                button("Auto-align tops").on_press(Message::ArrAlignTops),
                button("Revert").on_press(Message::ArrRevert),
                Space::with_width(Length::Fill),
                button("Apply").on_press(Message::ArrApply).style(button::primary),
            ]
            .spacing(8),
            text("Apply pushes SetLayout; the daemon-side handler for it is still a stub (see roadmap).").size(11),
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
                text(if p.connected { "*" } else { "o" }),
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
        let panel = match self.pair_phase.as_str() {
            "prompt" => column![
                text(if self.pair_peer.is_empty() {
                    "Pair with this device?".to_string()
                } else {
                    format!("Pair with '{}'?", self.pair_peer)
                })
                .size(16),
                text("Confirm this code matches on BOTH machines:").size(13),
                text(self.pair_sas.clone()).size(40),
                text("If they differ, someone may be intercepting - don't continue.").size(12),
                row![
                    button("They don't match").on_press(Message::PairRespond(false)).style(button::danger),
                    button("Confirm").on_press(Message::PairRespond(true)).style(button::primary),
                ]
                .spacing(8),
            ]
            .spacing(10),
            "discoverable" => {
                let mut c = column![
                    text("Pairing").size(16),
                    text("Waiting for a peer to connect...").size(14),
                    text("On the other device, pick this one from its nearby list, or Connect by address:").size(12),
                ]
                .spacing(8);
                if self.local_addrs.is_empty() {
                    c = c.push(text("This device: (no routable address found)").size(14));
                } else {
                    c = c.push(text("This device:").size(13));
                    for a in &self.local_addrs {
                        c = c.push(text(a.clone()).size(14));
                    }
                }
                c.push(button("Cancel").on_press(Message::PairClear))
            }
            "connecting" => column![
                text("Pairing").size(16),
                text("Connecting...").size(14),
                button("Cancel").on_press(Message::PairClear),
            ]
            .spacing(8),
            // "paired" is handled in `update`: on success we drop straight back
            // to the idle view (the device is already trusted), so it never
            // renders its own panel here.
            "aborted" => column![
                text("Pairing aborted").size(16),
                button("Close").on_press(Message::PairClear),
            ]
            .spacing(8),
            "error" => column![
                text(format!("Pairing error: {}", self.pair_peer)).size(14),
                button("Close").on_press(Message::PairClear),
            ]
            .spacing(8),
            // idle
            _ => {
                let mut c = column![text("Pair a new device").size(16)].spacing(8);

                // Pairing runs on the daemon's live endpoint, so it must be up
                // (same requirement for discoverable and connect-by-address).
                if !self.reachable {
                    c = c.push(text("Start the daemon first (Connection tab) to pair.").size(12));
                    c = c.push(button("Start pairing"));
                    return c.into();
                }

                // Primary: make this device discoverable. The other side picks it
                // from its nearby list (or connects by address below).
                c = c.push(
                    text("Make this device discoverable; on the other one, pick it from the nearby list. Then confirm the same 6-digit code on both.").size(12),
                );
                c = c.push(button("Start pairing").on_press(Message::PairStart).style(button::primary));

                // Auto-discovered peers currently accepting pairing.
                let nearby: Vec<_> = self.nearby.iter().filter(|p| !p.trusted).collect();
                if !nearby.is_empty() {
                    c = c.push(horizontal_rule(1));
                    c = c.push(text("Nearby - waiting to pair:").size(13));
                    for p in nearby {
                        c = c.push(
                            row![
                                text(format!("* {}", p.name)).width(Length::Fill),
                                text(p.device.clone()).size(11),
                                button("Pair").on_press(Message::PairDial(p.addr.clone())).style(button::primary),
                            ]
                            .spacing(10)
                            .align_y(Alignment::Center),
                        );
                    }
                }

                // Fallback: connect by address (for LANs where mDNS can't find it).
                c = c.push(horizontal_rule(1));
                c = c.push(text("Connect by address (if not auto-discovered):").size(12));
                let connect = if self.pair_addr.trim().is_empty() {
                    button("Connect")
                } else {
                    button("Connect").on_press(Message::PairDial(self.pair_addr.clone()))
                };
                c = c.push(
                    row![
                        text_input("192.168.1.42:7345", &self.pair_addr).on_input(Message::PairAddrChanged),
                        connect,
                    ]
                    .spacing(8),
                );
                c
            }
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
                text(format!("{pct}% - {} KB/s", bps / 1024)),
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
