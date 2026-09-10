use super::*;
use gpui::{AppContext as _, Entity, TestAppContext};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

struct Source {
    results: Mutex<VecDeque<Result<Release>>>,
    calls: Arc<AtomicUsize>,
}

impl UpdateSource for Source {
    fn fetch_latest(&self) -> Result<Release> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.results.lock().unwrap().pop_front().unwrap()
    }
}

fn release(version: u64) -> Release {
    Release {
        version: Version::new(version, 0, 0),
        notes: Some("fixture notes".into()),
        asset: Asset {
            name: "update.tar.gz".into(),
            url: "http://unused.invalid".into(),
            size: 0,
        },
        signature: None,
        signature_url: None,
        sha256: None,
    }
}

fn updater(
    cx: &mut TestAppContext,
    results: Vec<Result<Release>>,
    config: EngineConfig,
) -> (Entity<Updater>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let source = Source {
        results: Mutex::new(results.into()),
        calls: calls.clone(),
    };
    (cx.new(|cx| Updater::new(source, config, cx)), calls)
}

fn observe(cx: &TestAppContext, updater: &Entity<Updater>) -> Rc<RefCell<Vec<UpdateStatus>>> {
    let states = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        cx.observe(updater, {
            let states = states.clone();
            move |entity, cx| states.borrow_mut().push(entity.read(cx).status().clone())
        })
        .detach();
    });
    states
}

#[gpui::test]
fn check_notifies_and_busy_requests_do_not_duplicate(cx: &mut TestAppContext) {
    let (updater, calls) = updater(
        cx,
        vec![
            Ok(release(2)),
            Ok(release(1)),
            Err(Error::Parse("broken".into())),
        ],
        EngineConfig::new(Version::new(1, 0, 0)),
    );
    let states = observe(cx, &updater);
    cx.read(|cx| assert_eq!(updater.read(cx).status(), &UpdateStatus::Idle));
    updater.update(cx, super::Updater::download_and_install);
    assert!(states.borrow().is_empty());
    updater.update(cx, super::Updater::check);
    assert_eq!(*states.borrow(), vec![UpdateStatus::Checking]);
    updater.update(cx, |u, cx| {
        u.check(cx);
        u.download_and_install(cx);
    });
    cx.run_until_parked();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cx.read(|cx| {
        assert_eq!(
            updater.read(cx).available().unwrap().notes.as_deref(),
            Some("fixture notes")
        );
        assert!(updater.read(cx).task.is_none());
    });

    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *states.borrow(),
        vec![
            UpdateStatus::Checking,
            UpdateStatus::Available(Version::new(2, 0, 0)),
            UpdateStatus::Checking,
            UpdateStatus::UpToDate,
            UpdateStatus::Checking,
            UpdateStatus::Errored("failed to parse release metadata: broken".into()),
        ]
    );
    // Preserve the existing API: unsuccessful/no-newer checks do not clear
    // the previously available release, and a finished task is released.
    cx.read(|cx| {
        assert_eq!(
            updater.read(cx).available().unwrap().version,
            Version::new(2, 0, 0)
        );
        assert!(updater.read(cx).task.is_none());
    });
}

#[gpui::test]
fn dropping_before_poll_cancels_the_check(cx: &mut TestAppContext) {
    let (updater, calls) = updater(
        cx,
        vec![Ok(release(2))],
        EngineConfig::new(Version::new(1, 0, 0)),
    );
    let weak = updater.downgrade();
    updater.update(cx, super::Updater::check);
    drop(updater);
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// The server thread performs only socket I/O. GPUI work stays on its seeded
// deterministic executor; no sleeps, live URLs, or application paths are used.
fn serve(body: Vec<u8>, content_length: bool) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/update.tar.gz", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\n").unwrap();
        if content_length {
            write!(stream, "Content-Length: {}\r\n", body.len()).unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(&body).unwrap();
    });
    (url, server)
}

#[cfg(target_os = "linux")]
fn tarball(dir: &std::path::Path) -> Vec<u8> {
    let payload = dir.join("payload");
    std::fs::create_dir(&payload).unwrap();
    std::fs::write(payload.join("demo"), b"new executable fixture").unwrap();
    let archive = dir.join("fixture.tar.gz");
    assert!(
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(payload)
            .arg("demo")
            .status()
            .unwrap()
            .success()
    );
    std::fs::read(archive).unwrap()
}

#[cfg(target_os = "linux")]
#[gpui::test]
async fn download_samples_progress_installs_and_releases_task(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("demo");
    std::fs::write(&root, b"old").unwrap();
    let body = tarball(dir.path());
    let length = body.len() as u64;
    let (url, server) = serve(body, true);
    let mut latest = release(3);
    latest.asset.url = url;
    let config = EngineConfig::new(Version::new(1, 0, 0))
        .download_dir(dir.path().join("downloads"))
        .install_root(&root);
    let (updater, calls) = updater(cx, vec![Ok(latest)], config);
    let states = observe(cx, &updater);
    // Exercise the busy guard during Installing, not just during Checking.
    cx.update(|cx| {
        cx.observe(&updater, |entity, cx| {
            if entity.read(cx).status() == &UpdateStatus::Installing {
                entity.update(cx, |u, cx| {
                    u.check(cx);
                    u.download_and_install(cx);
                });
            }
        })
        .detach();
    });
    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    updater.update(cx, super::Updater::download_and_install);
    updater.update(cx, |u, cx| {
        u.check(cx);
        u.download_and_install(cx);
    });
    cx.run_until_parked();
    server.join().unwrap();
    assert_eq!(std::fs::read(&root).unwrap(), b"old");
    cx.executor().advance_clock(Duration::from_millis(119));
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            updater.read(cx).status(),
            &UpdateStatus::Downloading {
                downloaded: 0,
                total: None
            }
        );
    });
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            updater.read(cx).status(),
            &UpdateStatus::Staged(Version::new(3, 0, 0))
        );
        assert!(updater.read(cx).task.is_none());
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    {
        let states = states.borrow();
        assert!(states.contains(&UpdateStatus::Downloading {
            downloaded: length,
            total: Some(length)
        }));
        assert!(states.contains(&UpdateStatus::Installing));
        assert_eq!(
            states.last(),
            Some(&UpdateStatus::Staged(Version::new(3, 0, 0)))
        );
    }
    assert_eq!(std::fs::read(&root).unwrap(), b"new executable fixture");
    // gpui-pre exposes the fake platform's restart request. This never
    // launches the fixture or restarts this test process.
    let restart = cx.expect_restart();
    updater.update(cx, |u, cx| u.restart(cx));
    let (path, arguments) = restart.await.unwrap();
    assert_eq!(path, Some(root.clone()));
    assert!(arguments.is_empty());
    drop(updater);
    cx.update(|_| {});
    // Entity/task drop is not installation rollback.
    assert_eq!(std::fs::read(&root).unwrap(), b"new executable fixture");
}

#[gpui::test]
fn unknown_length_and_verification_error_notify_without_install(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("never-replace");
    std::fs::write(&root, b"old").unwrap();
    let (url, server) = serve(b"abc".to_vec(), false);
    let mut latest = release(2);
    latest.asset.url = url;
    latest.sha256 = Some("deadbeef".into());
    let (updater, _) = updater(
        cx,
        vec![Ok(latest)],
        EngineConfig::new(Version::new(1, 0, 0))
            .verification(Verification::Checksum)
            .download_dir(dir.path().join("downloads"))
            .install_root(&root),
    );
    let states = observe(cx, &updater);
    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    updater.update(cx, super::Updater::download_and_install);
    cx.run_until_parked();
    server.join().unwrap();
    cx.executor().advance_clock(Duration::from_millis(120));
    cx.run_until_parked();
    assert!(states.borrow().contains(&UpdateStatus::Downloading {
        downloaded: 3,
        total: None
    }));
    cx.read(|cx| {
        assert_eq!(updater.read(cx).status(), &UpdateStatus::Errored(
        "checksum mismatch: expected deadbeef, got ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()));
        assert_eq!(states.borrow().last(), Some(updater.read(cx).status()));
        assert!(updater.read(cx).task.is_none());
    });
    assert!(!states.borrow().contains(&UpdateStatus::Installing));
    assert_eq!(std::fs::read(root).unwrap(), b"old");
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn install_error_notifies_and_preserves_target(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("demo");
    std::fs::write(&root, b"old").unwrap();
    let (url, server) = serve(b"not a tarball".to_vec(), true);
    let mut latest = release(2);
    latest.asset.url = url;
    let (updater, _) = updater(
        cx,
        vec![Ok(latest)],
        EngineConfig::new(Version::new(1, 0, 0))
            .download_dir(dir.path().join("downloads"))
            .install_root(&root),
    );
    let states = observe(cx, &updater);
    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    updater.update(cx, super::Updater::download_and_install);
    cx.run_until_parked();
    server.join().unwrap();
    cx.executor().advance_clock(Duration::from_millis(120));
    cx.run_until_parked();
    assert!(states.borrow().contains(&UpdateStatus::Installing));
    cx.read(|cx| {
        assert!(matches!(updater.read(cx).status(), UpdateStatus::Errored(message) if message.starts_with("install failed: tar extract failed:")));
        assert_eq!(states.borrow().last(), Some(updater.read(cx).status()));
        assert!(updater.read(cx).task.is_none());
    });
    assert_eq!(std::fs::read(root).unwrap(), b"old");
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn dropping_during_progress_stops_foreground_but_keeps_download(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("demo");
    std::fs::write(&root, b"old").unwrap();
    let body = tarball(dir.path());
    let (url, server) = serve(body.clone(), true);
    let mut latest = release(2);
    latest.asset.url = url;
    let downloads = dir.path().join("downloads");
    let (updater, _) = updater(
        cx,
        vec![Ok(latest)],
        EngineConfig::new(Version::new(1, 0, 0))
            .download_dir(&downloads)
            .install_root(&root),
    );
    let states = observe(cx, &updater);
    updater.update(cx, super::Updater::check);
    cx.run_until_parked();
    updater.update(cx, super::Updater::download_and_install);
    cx.run_until_parked();
    server.join().unwrap();
    let weak = updater.downgrade();
    cx.read(|cx| assert!(updater.read(cx).task.is_some()));
    let notifications = states.borrow().len();
    drop(updater);
    cx.update(|_| {});
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert_eq!(states.borrow().len(), notifications);
    assert_eq!(std::fs::read(root).unwrap(), b"old");
    assert_eq!(
        std::fs::read(downloads.join("update.tar.gz")).unwrap(),
        body
    );
    // This tests cancellation before install starts. A synchronous install
    // already running cannot be interrupted or rolled back by dropping Task.
}
