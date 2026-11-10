//! Updating Quarry from its GitHub releases.
//!
//! Release builds check for a newer release shortly after launch and once a
//! day while running (set `QUARRY_UPDATE_CHECK=0` to turn that off, or `=1`
//! to turn it on in a debug build); Quarry > Check for Updates… checks now.
//! A newer release opens a prompt with its notes. Update downloads the build,
//! checks it and puts it in place of the installed Quarry, which then offers
//! to restart. Where Quarry can't replace itself (a folder it can't write to,
//! a `cargo run` build) the prompt opens the release page instead.

mod install;
mod release;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global, IntoElement,
    ParentElement as _, SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
    px,
};
use semver::Version;
use ureq::Agent;

use install::{Installation, Progress};
use release::Release;

gpui_kit::actions!(quarry, [CheckForUpdates]);

const FIRST_CHECK: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

pub enum State {
    Idle,
    Checking {
        /// Asked for from the menu, so "you're up to date" and errors are
        /// worth telling.
        manual: bool,
    },
    Downloading {
        release: Release,
        progress: Arc<Progress>,
    },
    /// Installed and waiting for a restart.
    Installed {
        release: Release,
        path: PathBuf,
    },
}

pub enum UpdateEvent {
    /// A release newer than this build. `installation` is where Quarry would
    /// install it, or why it can't install it itself.
    Available {
        release: Release,
        installation: Result<Installation, String>,
    },
    Installed(Release),
    Notice(SharedString),
    Failed(SharedString),
}

pub struct Updater {
    state: State,
    /// The executable as it was at launch: on Linux, `current_exe` names
    /// the replaced file once an update lands.
    exe: Option<PathBuf>,
    agent: Agent,
    /// The release the user chose to skip. Automatic checks don't offer it.
    skipped: Option<Version>,
    task: Option<Task<()>>,
    ticker: Option<Task<()>>,
    schedule: Option<Task<()>>,
}

impl EventEmitter<UpdateEvent> for Updater {}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

pub fn init(cx: &mut App) {
    let updater = cx.new(|cx| {
        let mut updater = Updater {
            state: State::Idle,
            exe: std::env::current_exe().ok(),
            agent: release::agent(),
            skipped: read_skipped(),
            task: None,
            ticker: None,
            schedule: None,
        };
        if automatic_checks() {
            updater.schedule = Some(cx.spawn(async move |this, cx| {
                let mut wait = FIRST_CHECK;
                loop {
                    cx.background_executor().timer(wait).await;
                    if this
                        .update(cx, |this: &mut Updater, cx| this.check(false, cx))
                        .is_err()
                    {
                        break;
                    }
                    wait = CHECK_EVERY;
                }
            }));
        }
        updater
    });
    cx.set_global(GlobalUpdater(updater));
    cx.on_action(|_: &CheckForUpdates, cx| {
        Updater::global(cx).update(cx, |updater, cx| updater.check(true, cx))
    });
}

/// Whether to check without being asked: in release builds unless
/// `QUARRY_UPDATE_CHECK` says otherwise.
fn automatic_checks() -> bool {
    let setting = std::env::var("QUARRY_UPDATE_CHECK").unwrap_or_default();
    match setting.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => false,
        "1" | "true" | "on" | "yes" => true,
        _ => !cfg!(debug_assertions),
    }
}

impl Updater {
    pub fn global(cx: &App) -> Entity<Updater> {
        cx.global::<GlobalUpdater>().0.clone()
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Look for a newer release. A `manual` check also reports that there
    /// is none, or why it couldn't tell, and offers a skipped release again.
    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        match &mut self.state {
            State::Idle => {}
            State::Checking { manual: asked } => {
                *asked |= manual;
                return;
            }
            State::Downloading { release, .. } => {
                if manual {
                    let message = format!("Quarry {} is downloading.", release.version);
                    cx.emit(UpdateEvent::Notice(message.into()));
                }
                return;
            }
            State::Installed { release, .. } => {
                if manual {
                    cx.emit(UpdateEvent::Installed(release.clone()));
                }
                return;
            }
        }
        self.state = State::Checking { manual };
        cx.notify();
        let agent = self.agent.clone();
        let exe = self.exe.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let latest = release::latest(&agent)?;
                    if latest.version <= release::current_version() {
                        return anyhow::Ok(None);
                    }
                    let installation = match &exe {
                        Some(exe) => Installation::find(exe),
                        None => Err("Quarry couldn't find its own executable.".into()),
                    };
                    Ok(Some((latest, installation)))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.checked(found, cx));
        }));
    }

    fn checked(
        &mut self,
        found: anyhow::Result<Option<(Release, Result<Installation, String>)>>,
        cx: &mut Context<Self>,
    ) {
        let manual = matches!(self.state, State::Checking { manual: true });
        self.state = State::Idle;
        self.task = None;
        cx.notify();
        match found {
            Ok(Some((release, installation))) => {
                if manual || self.skipped.as_ref() != Some(&release.version) {
                    cx.emit(UpdateEvent::Available {
                        release,
                        installation,
                    });
                }
            }
            Ok(None) => {
                if manual {
                    let message = format!(
                        "Quarry {} is the latest version.",
                        release::current_version()
                    );
                    cx.emit(UpdateEvent::Notice(message.into()));
                }
            }
            Err(err) => {
                log::warn!("checking for updates: {err:#}");
                if manual {
                    let message = format!("Couldn't check for updates: {err:#}");
                    cx.emit(UpdateEvent::Failed(message.into()));
                }
            }
        }
    }

    /// Download `release` and put it in place of `installation`.
    pub fn install(
        &mut self,
        release: Release,
        installation: Installation,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.state, State::Idle) {
            return;
        }
        let progress = Arc::new(Progress::default());
        self.state = State::Downloading {
            release: release.clone(),
            progress: progress.clone(),
        };
        cx.notify();
        let agent = self.agent.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn({
                    let release = release.clone();
                    async move {
                        install::install(&installation, &release, &agent, &progress)
                            .map(|()| installation.path().to_path_buf())
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                this.ticker = None;
                match installed {
                    Ok(path) => {
                        this.state = State::Installed {
                            release: release.clone(),
                            path,
                        };
                        cx.emit(UpdateEvent::Installed(release));
                    }
                    Err(err) => {
                        log::warn!("installing {}: {err:#}", release.version);
                        this.state = State::Idle;
                        let message =
                            format!("Couldn't update to Quarry {}: {err:#}", release.version);
                        cx.emit(UpdateEvent::Failed(message.into()));
                    }
                }
                cx.notify();
            });
        }));
        // Keep the download's progress moving in the status bar.
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let downloading = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        matches!(this.state, State::Downloading { .. })
                    })
                    .unwrap_or(false);
                if !downloading {
                    break;
                }
            }
        }));
    }

    /// Stop offering `version` on automatic checks, across launches.
    pub fn skip(&mut self, version: Version) {
        if let Some(path) = skipped_path() {
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, version.to_string()));
            if let Err(err) = written {
                log::warn!("saving the skipped update to {}: {err}", path.display());
            }
        }
        self.skipped = Some(version);
    }
}

/// Relaunch into the installed update.
pub fn restart(cx: &mut App) {
    if let State::Installed { path, .. } = Updater::global(cx).read(cx).state() {
        let path = path.clone();
        cx.set_restart_path(path);
    }
    cx.restart();
}

fn skipped_path() -> Option<PathBuf> {
    let folder = if cfg!(target_os = "macos") {
        "Quarry"
    } else {
        "quarry"
    };
    Some(dirs::config_dir()?.join(folder).join("skipped-update"))
}

fn read_skipped() -> Option<Version> {
    let text = std::fs::read_to_string(skipped_path()?).ok()?;
    Version::parse(text.trim()).ok()
}

/// Show what the updater reports in `window`.
pub fn show(event: &UpdateEvent, window: &mut Window, cx: &mut App) {
    match event {
        UpdateEvent::Available {
            release,
            installation,
        } => prompt_update(release.clone(), installation.clone(), window, cx),
        UpdateEvent::Installed(release) => prompt_restart(release.clone(), window, cx),
        UpdateEvent::Notice(message) => {
            window.push_notification(Notification::info(message.clone()), cx)
        }
        UpdateEvent::Failed(message) => {
            window.push_notification(Notification::error(message.clone()), cx)
        }
    }
}

fn prompt_update(
    release: Release,
    installation: Result<Installation, String>,
    window: &mut Window,
    cx: &mut App,
) {
    let updater = Updater::global(cx);
    let current = release::current_version();
    window.open_dialog(cx, move |dialog, _, cx| {
        let muted = cx.theme().muted_foreground;
        let notes = release.notes.trim().to_owned();
        let skip = {
            let (updater, version) = (updater.clone(), release.version.clone());
            move |_: &_, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                updater.update(cx, |updater, _| updater.skip(version.clone()));
            }
        };
        let update = {
            let (updater, release, installation) =
                (updater.clone(), release.clone(), installation.clone());
            move |_: &_, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                match &installation {
                    Ok(installation) => updater.update(cx, |updater, cx| {
                        updater.install(release.clone(), installation.clone(), cx)
                    }),
                    Err(_) => cx.open_url(&release.page),
                }
            }
        };
        dialog
            .title(format!("Quarry {} is available", release.version))
            .w(px(560.))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("You have {current}.")),
                    )
                    .when(!notes.is_empty(), |this| {
                        this.child(
                            div()
                                .h(px(220.))
                                .p_3()
                                .rounded(cx.theme().radius)
                                .border_1()
                                .border_color(cx.theme().border)
                                .text_sm()
                                .child(
                                    TextView::markdown("update-notes", notes)
                                        .scrollable(true)
                                        .selectable(true),
                                ),
                        )
                    })
                    .when_some(installation.as_ref().err(), |this, reason| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{reason} Download opens the release page.")),
                        )
                    }),
            )
            .footer(
                DialogFooter::new()
                    .w_full()
                    .child(
                        Button::new("update-skip")
                            .label("Skip This Version")
                            .ghost()
                            .on_click(skip),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("update-later")
                            .label("Later")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("update-now")
                            .label(if installation.is_ok() {
                                "Update"
                            } else {
                                "Download"
                            })
                            .primary()
                            .on_click(update),
                    ),
            )
    });
}

fn prompt_restart(release: Release, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        dialog
            .title(format!("Quarry {} is installed", release.version))
            .w(px(440.))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Restart Quarry to start using it. Restarting closes the connection \
                         and clears the editor.",
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("restart-later")
                            .label("Later")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("restart-now")
                            .label("Restart")
                            .primary()
                            .on_click(|_, _, cx| restart(cx)),
                    ),
            )
    });
}

/// The status bar's update item: progress while downloading, a restart
/// button once installed.
pub fn status_item(cx: &App) -> Option<AnyElement> {
    let muted = cx.theme().muted_foreground;
    match Updater::global(cx).read(cx).state() {
        State::Idle | State::Checking { manual: false } => None,
        State::Checking { manual: true } => Some(
            div()
                .text_color(muted)
                .child("Checking for updates…")
                .into_any_element(),
        ),
        State::Downloading { release, progress } => {
            let percent = progress
                .fraction()
                .map(|f| format!(" {:.0}%", (f * 100.).min(100.)))
                .unwrap_or_default();
            Some(
                div()
                    .text_color(muted)
                    .child(format!("Downloading Quarry {}…{percent}", release.version))
                    .into_any_element(),
            )
        }
        State::Installed { release, .. } => Some(
            Button::new("restart-to-update")
                .label(format!("Restart to update to {}", release.version))
                .xsmall()
                .primary()
                .on_click(|_, _, cx| restart(cx))
                .into_any_element(),
        ),
    }
}
