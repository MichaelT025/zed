use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Task, WeakEntity, actions};
use http_client::{AsyncBody, HttpRequestExt as _, RedirectPolicy, Request};
use release_channel::{AppCommitSha, ReleaseChannel};
use serde::Deserialize;
use settings::{RegisterSetting, Settings};
use smol::fs::File;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use util::ResultExt as _;
use util::command::new_command;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

const RELEASES_URL: &str = "https://api.github.com/repos/MichaelT025/zed/releases/latest";
const INSTALLER_ASSET_NAME: &str = "ZedFork-Setup-x86_64.exe";
const USER_AGENT: &str = "Zed-fork-updater";
const INITIAL_DELAY: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

actions!(
    fork_updater,
    [
        /// Checks the fork's GitHub releases for a newer Zed fork build.
        CheckForUpdates,
    ]
);

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    target_commitish: String,
    published_at: DateTime<Utc>,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct ForkAutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
impl Settings for ForkAutoUpdateSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap_or(true))
    }
}

#[derive(Clone, Debug, Default)]
enum Status {
    #[default]
    Idle,
    Checking,
    Downloading,
    Staged {
        tag: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckKind {
    Automatic,
    Manual,
}

impl CheckKind {
    fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

#[derive(Default)]
struct ForkUpdater {
    status: Status,
    poll_task: Option<Task<()>>,
    pending_check: Option<Task<()>>,
}

#[derive(Default)]
struct GlobalForkUpdater(Option<Entity<ForkUpdater>>);

impl Global for GlobalForkUpdater {}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &CheckForUpdates, cx: &mut App| {
        let Some(updater) = ForkUpdater::get(cx) else {
            log::error!("fork updater: not initialized, cannot check for fork updates");
            return;
        };
        updater.update(cx, |updater, cx| updater.check(CheckKind::Manual, cx));
    });

    let updater = cx.new(|_| ForkUpdater::default());
    cx.set_global(GlobalForkUpdater(Some(updater.clone())));

    if !cfg!(target_os = "windows") {
        log::info!("fork updater: automatic checks are only supported on Windows");
        return;
    }
    if *release_channel::RELEASE_CHANNEL != ReleaseChannel::Dev {
        log::info!("fork updater: automatic checks are only enabled on the dev channel");
        return;
    }
    if AppCommitSha::try_global(cx).is_none() {
        log::info!("fork updater: automatic checks need the commit sha of this build");
        return;
    }
    match helper_path() {
        Some(path) if path.exists() => {}
        _ => {
            log::info!("fork updater: automatic checks need an installed build");
            return;
        }
    }

    updater.update(cx, |updater, cx| updater.start_polling(cx));
    log::info!("fork updater: polling the fork's GitHub releases for new builds");
}

impl ForkUpdater {
    fn get(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalForkUpdater>()?.0.clone()
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INITIAL_DELAY).await;
            loop {
                let polling_enabled = this
                    .update(cx, |_, cx| ForkAutoUpdateSetting::get_global(cx).0)
                    .log_err();
                if polling_enabled == Some(true) {
                    this.update(cx, |this, cx| this.check(CheckKind::Automatic, cx))
                        .log_err();
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        }));
    }

    fn check(&mut self, check_kind: CheckKind, cx: &mut Context<Self>) {
        if check_kind.is_manual() && self.poll_task.is_none() {
            log::info!("fork updater: running a one-off check, automatic polling is disabled");
        }
        if self.pending_check.is_some() {
            log::info!("fork updater: a check is already in progress");
            return;
        }
        if let Status::Staged { tag } = &self.status {
            if check_kind.is_manual() {
                log::info!("fork updater: {tag} is already staged for the next restart");
            }
            return;
        }

        self.status = Status::Checking;
        self.pending_check = Some(cx.spawn(async move |this, cx| {
            let result = run_check(this.clone(), check_kind, cx).await;
            if let Err(error) = result {
                log::error!("fork updater: update check failed: {error:#}");
                this.update(cx, |this, cx| {
                    this.status = Status::Idle;
                    if check_kind.is_manual() {
                        show_check_notification(
                            format!("Couldn't check for Zed fork updates: {error}"),
                            cx,
                        );
                    }
                })
                .log_err();
            }
            this.update(cx, |this, _| this.pending_check = None)
                .log_err();
        }));
    }
}

async fn run_check(
    this: WeakEntity<ForkUpdater>,
    check_kind: CheckKind,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some(commit_sha) = cx.update(|cx| AppCommitSha::try_global(cx)) else {
        anyhow::bail!("this build has no commit sha, so fork updates cannot be checked");
    };

    let release = fetch_latest_release(cx).await?;
    let build_time = running_build_time().await;

    if !is_update_available(&release, &commit_sha.full(), build_time) {
        if check_kind.is_manual() {
            let message = format!("Zed fork is up to date ({})", commit_sha.short());
            cx.update(|cx| show_check_notification(message, cx));
        } else {
            log::info!("fork updater: already up to date ({})", commit_sha.short());
        }
        this.update(cx, |this, _| this.status = Status::Idle)?;
        return Ok(());
    }

    let Some(helper_path) = helper_path() else {
        anyhow::bail!("could not determine the path to the auto update helper");
    };
    if !helper_path.exists() {
        let message = format!(
            "Zed fork update {} is available, but this isn't an installed build, so it won't be installed.",
            release.tag_name
        );
        if check_kind.is_manual() {
            cx.update(|cx| show_check_notification(message, cx));
        } else {
            log::info!("fork updater: {message}");
        }
        this.update(cx, |this, _| this.status = Status::Idle)?;
        return Ok(());
    }

    let installer_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == INSTALLER_ASSET_NAME)
        .with_context(|| {
            format!(
                "release {} does not contain {INSTALLER_ASSET_NAME}",
                release.tag_name
            )
        })?;

    let installer_path = installer_download_path(&release.tag_name);
    if let Some(parent_directory) = installer_path.parent() {
        smol::fs::create_dir_all(parent_directory)
            .await
            .with_context(|| format!("failed to create {}", parent_directory.display()))?;
    }

    this.update(cx, |this, _| this.status = Status::Downloading)?;
    download_installer(&installer_asset.browser_download_url, &installer_path, cx).await?;
    install_installer(&installer_path).await?;

    let tag = release.tag_name.clone();
    let release_url = release.html_url.clone();
    this.update(cx, |this, _| {
        this.status = Status::Staged { tag: tag.clone() }
    })?;
    cx.update(|cx| show_staged_notification(tag, release_url, helper_path, cx));
    Ok(())
}

async fn fetch_latest_release(cx: &mut AsyncApp) -> Result<GitHubRelease> {
    let http_client = cx.update(|cx| cx.http_client());
    let request = Request::get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())
        .context("failed to build the latest fork release request")?;

    let mut response = http_client
        .send(request)
        .await
        .context("failed to request the latest fork release")?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub returned {} for the latest fork release",
        response.status()
    );

    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("failed to read the latest fork release")?;
    serde_json::from_str(&body).context("failed to parse the latest fork release")
}

async fn download_installer(
    download_url: &str,
    installer_path: &Path,
    cx: &mut AsyncApp,
) -> Result<()> {
    let http_client = cx.update(|cx| cx.http_client());
    let request = Request::get(download_url)
        .header("User-Agent", USER_AGENT)
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())
        .context("failed to build the fork installer request")?;

    let mut response = http_client
        .send(request)
        .await
        .context("failed to download the fork installer")?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download the fork installer: {}",
        response.status()
    );

    let mut installer_file = File::create(installer_path)
        .await
        .with_context(|| format!("failed to create {}", installer_path.display()))?;
    let body = response.body_mut();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = body
            .read(&mut buffer)
            .await
            .context("failed to read the fork installer download")?;
        if bytes_read == 0 {
            break;
        }
        installer_file
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to write the fork installer")?;
    }
    installer_file
        .flush()
        .await
        .context("failed to flush the fork installer")?;
    log::info!("fork updater: downloaded installer to {:?}", installer_path);
    Ok(())
}

async fn install_installer(installer_path: &Path) -> Result<()> {
    let mut command = new_command(installer_path);
    command
        .arg("/verysilent")
        .arg("/update=true")
        .arg("/MERGETASKS=!desktopicon");
    let output = command
        .output()
        .await
        .context("failed to start the fork installer")?;
    anyhow::ensure!(
        output.status.success(),
        "the fork installer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn running_build_time() -> Option<DateTime<Utc>> {
    let executable_path = env::current_exe().ok()?;
    let modified = smol::fs::metadata(executable_path)
        .await
        .ok()?
        .modified()
        .ok()?;
    Some(DateTime::<Utc>::from(modified))
}

fn is_update_available(
    release: &GitHubRelease,
    running_commit_sha: &str,
    running_build_time: Option<DateTime<Utc>>,
) -> bool {
    let target_commitish = release.target_commitish.as_str();
    let is_commit_sha = target_commitish.len() == 40
        && target_commitish
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if !is_commit_sha {
        return false;
    }
    if target_commitish.eq_ignore_ascii_case(running_commit_sha) {
        return false;
    }
    // A locally installed build may be newer than the latest published release,
    // in which case installing that release would be a downgrade.
    match running_build_time {
        Some(build_time) => release.published_at > build_time,
        None => true,
    }
}

fn helper_path() -> Option<PathBuf> {
    Some(
        env::current_exe()
            .ok()?
            .parent()?
            .join("tools")
            .join("auto_update_helper.exe"),
    )
}

fn installer_download_path(tag: &str) -> PathBuf {
    let sanitized_tag: String = tag
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .collect();
    env::temp_dir()
        .join("zed-fork-update")
        .join(format!("ZedFork-Setup-{sanitized_tag}.exe"))
}

struct ForkUpdateNotification;

struct ForkUpdateCheckNotification;

fn show_staged_notification(tag: String, release_url: String, helper_path: PathBuf, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<ForkUpdateNotification>(),
        cx,
        move |cx| {
            let tag = tag.clone();
            let release_url = release_url.clone();
            let helper_path = helper_path.clone();
            cx.new(|cx| {
                MessageNotification::new(
                    format!("Zed fork {tag} is ready. It will be installed when Zed restarts."),
                    cx,
                )
                .primary_message("Restart to Update")
                .primary_on_click(move |_window, cx| {
                    cx.set_restart_path(helper_path.clone());
                    cx.restart();
                })
                .more_info_message("View Release")
                .more_info_url(release_url)
                .show_suppress_button(false)
            })
        },
    );
}

fn show_check_notification(message: String, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<ForkUpdateCheckNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(|cx| MessageNotification::new(message, cx).show_suppress_button(false))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn sample_release(target_commitish: &str, published_at: DateTime<Utc>) -> GitHubRelease {
        GitHubRelease {
            tag_name: "v1.0.0".to_string(),
            target_commitish: target_commitish.to_string(),
            published_at,
            html_url: "https://github.com/MichaelT025/zed/releases/tag/v1.0.0".to_string(),
            assets: Vec::new(),
        }
    }

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp should be in range")
    }

    #[test]
    fn newer_release_with_different_commit_is_available() {
        let build_time = timestamp(1_700_000_000);
        let release = sample_release(OTHER_SHA, timestamp(1_700_003_600));
        assert!(is_update_available(&release, RUNNING_SHA, Some(build_time)));
    }

    #[test]
    fn release_for_the_running_commit_is_not_available() {
        let build_time = timestamp(1_700_000_000);
        let release = sample_release(&RUNNING_SHA.to_ascii_uppercase(), timestamp(1_700_003_600));
        assert!(!is_update_available(
            &release,
            RUNNING_SHA,
            Some(build_time)
        ));
        assert!(!is_update_available(&release, RUNNING_SHA, None));
    }

    #[test]
    fn release_with_branch_target_is_not_available() {
        let build_time = timestamp(1_700_000_000);
        let release = sample_release("main", timestamp(1_700_003_600));
        assert!(!is_update_available(
            &release,
            RUNNING_SHA,
            Some(build_time)
        ));
    }

    #[test]
    fn release_published_before_the_running_build_is_not_available() {
        let build_time = timestamp(1_700_003_600);
        let release = sample_release(OTHER_SHA, timestamp(1_700_000_000));
        assert!(!is_update_available(
            &release,
            RUNNING_SHA,
            Some(build_time)
        ));
    }

    #[test]
    fn unknown_build_time_allows_a_different_commit() {
        let release = sample_release(OTHER_SHA, timestamp(1_700_000_000));
        assert!(is_update_available(&release, RUNNING_SHA, None));
    }

    #[test]
    fn latest_release_json_ignores_unknown_fields() {
        let json = serde_json::json!({
            "url": "https://api.github.com/repos/MichaelT025/zed/releases/1",
            "assets_url": "https://api.github.com/repos/MichaelT025/zed/releases/1/assets",
            "upload_url": "https://uploads.github.com/repos/MichaelT025/zed/releases/1/assets{?name,label}",
            "id": 1,
            "author": { "login": "MichaelT025", "id": 42, "site_admin": false },
            "draft": false,
            "prerelease": false,
            "created_at": "2024-05-01T00:00:00Z",
            "published_at": "2024-05-01T00:05:00Z",
            "tag_name": "v0.190.0",
            "target_commitish": "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c",
            "name": "Zed Dev",
            "body": "Notes",
            "html_url": "https://github.com/MichaelT025/zed/releases/tag/v0.190.0",
            "assets": [
                {
                    "url": "https://api.github.com/repos/MichaelT025/zed/releases/assets/1",
                    "id": 1,
                    "name": "ZedFork-Setup-x86_64.exe",
                    "label": "",
                    "content_type": "application/octet-stream",
                    "state": "uploaded",
                    "size": 1234,
                    "download_count": 7,
                    "created_at": "2024-05-01T00:01:00Z",
                    "browser_download_url": "https://github.com/MichaelT025/zed/releases/download/v0.190.0/ZedFork-Setup-x86_64.exe"
                }
            ]
        });

        let parsed: GitHubRelease =
            serde_json::from_value(json).expect("sample release should deserialize");

        assert_eq!(parsed.tag_name, "v0.190.0");
        assert_eq!(
            parsed.target_commitish,
            "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c"
        );
        assert_eq!(parsed.published_at, timestamp(1_714_521_900));
        assert_eq!(
            parsed.html_url,
            "https://github.com/MichaelT025/zed/releases/tag/v0.190.0"
        );
        assert_eq!(parsed.assets.len(), 1);
        assert_eq!(parsed.assets[0].name, INSTALLER_ASSET_NAME);
        assert_eq!(
            parsed.assets[0].browser_download_url,
            "https://github.com/MichaelT025/zed/releases/download/v0.190.0/ZedFork-Setup-x86_64.exe"
        );
    }

    #[test]
    fn installer_download_path_sanitizes_the_tag() {
        let path = installer_download_path("v1.0.0/../evil");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("ZedFork-Setup-v1.0.0..evil.exe")
        );
    }
}
