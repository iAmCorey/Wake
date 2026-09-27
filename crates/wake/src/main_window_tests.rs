use super::{
    arm_exit_timeout, close, flush, quit, should_close, MainGeometry, MainWindow, SavedWindow,
};
use gpui::{
    div, point, px, size, App, AppContext, Bounds, Context, IntoElement, Render, Window,
    WindowHandle, WindowOptions,
};
use gpui_component::Root;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// The real exit fallback must only run in a child: it must never kill the test runner.
fn child(mode: &str) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "main_window::tests::exit_child", "--nocapture"])
        .env("WAKE_TEST_EXIT_MODE", mode)
        .env("WAKE_TEST_EXIT_DIR", dir.path())
        .env("HOME", dir.path())
        .env("WAKE_HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(15) {
            child.kill().ok();
            let output = child.wait_with_output().unwrap();
            panic!(
                "{mode} did not exit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (dir, output)
}

#[cfg(not(target_os = "linux"))]
#[test]
fn exit_timeout_stops_a_stuck_process() {
    let (dir, output) = child("timeout");
    assert!(dir.path().join("started").is_file());
    assert!(String::from_utf8_lossy(&output.stderr).contains("forcing exit"));
}

// Linux uses its real headless platform and calloop, not GPUI's no-op TestPlatform::quit.
// This covers application lifecycle; compositor/GPU-specific hangs still need desktop testing.
#[cfg(target_os = "linux")]
#[test]
fn linux_window_close_and_quit_run_cleanup_with_a_bounded_fallback() {
    for mode in ["close", "native-close", "quit", "stalled-quit"] {
        let (dir, output) = child(mode);
        for marker in ["secondary-released", "main-survived", "quit-hook"] {
            assert!(
                dir.path().join(marker).is_file(),
                "{mode}: missing {marker}"
            );
        }
        let saved: SavedWindow = serde_json::from_slice(
            &std::fs::read(dir.path().join("config/wake/window.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.bounds.size, size(px(1234.), px(786.)));
        let timed_out = String::from_utf8_lossy(&output.stderr).contains("forcing exit");
        if mode == "stalled-quit" {
            assert!(timed_out);
            assert!(!dir.path().join("returned").exists());
        } else {
            assert!(!timed_out, "{mode} must finish without the fallback");
            for marker in ["main-released", "remaining-released", "returned"] {
                assert!(
                    dir.path().join(marker).is_file(),
                    "{mode}: missing {marker}"
                );
            }
        }
    }
}

#[test]
fn exit_child() {
    let Ok(mode) = std::env::var("WAKE_TEST_EXIT_MODE") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("WAKE_TEST_EXIT_DIR").unwrap());
    if mode == "timeout" {
        std::fs::write(dir.join("started"), b"").unwrap();
        arm_exit_timeout();
        loop {
            std::thread::park();
        }
    }
    native_exit_child(mode, &dir);
}

struct ReleaseMarker(PathBuf);

impl Render for ReleaseMarker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        std::fs::write(&self.0, b"").unwrap();
    }
}

fn test_window(marker: PathBuf, cx: &mut App) -> WindowHandle<Root> {
    cx.open_window(WindowOptions::default(), |window, cx| {
        window.on_window_should_close(cx, should_close);
        let view = cx.new(|_| ReleaseMarker(marker));
        cx.new(|cx| Root::new(view, window, cx))
    })
    .unwrap()
}

// Compiled on every platform so local builds also type-check the Linux-only smoke test.
fn native_exit_child(mode: String, dir: &Path) {
    if !cfg!(target_os = "linux") {
        panic!("native lifecycle checks require Linux");
    }
    let app_dir = dir.to_path_buf();
    gpui_platform::headless()
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            let main = test_window(app_dir.join("main-released"), cx);
            cx.set_global(MainWindow(main));
            let secondary = test_window(app_dir.join("secondary-released"), cx);
            let quit_dir = app_dir.clone();
            let stalled = mode == "stalled-quit";
            cx.on_app_quit(move |cx| {
                flush(cx);
                std::fs::write(quit_dir.join("quit-hook"), b"").unwrap();
                if stalled {
                    loop {
                        std::thread::park();
                    }
                }
                async {}
            })
            .detach();

            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    secondary
                        .update(cx, |_, window, cx| close(window, cx))
                        .unwrap()
                });
                // Return to calloop: an accidental application quit from closing Settings
                // must prevent the next stage and fail the parent's marker assertions.
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                cx.update(|cx| {
                    assert_eq!(cx.windows().len(), 1);
                    main.update(cx, |_, _, _| {}).unwrap();
                    std::fs::write(app_dir.join("main-survived"), b"").unwrap();
                    test_window(app_dir.join("remaining-released"), cx);
                    // Model a pending debounced geometry update, before it has hit disk.
                    let save_task = cx.spawn(async |_| {});
                    cx.set_global(MainGeometry {
                        saved: SavedWindow::default(),
                        live: Some(Bounds {
                            origin: point(px(20.), px(30.)),
                            size: size(px(1234.), px(786.)),
                        }),
                        save_task: Some(save_task),
                    });
                    match mode.as_str() {
                        "close" => main.update(cx, |_, window, cx| close(window, cx)).unwrap(),
                        "native-close" => main
                            .update(cx, |_, window, cx| assert!(!should_close(window, cx)))
                            .unwrap(),
                        "quit" | "stalled-quit" => quit(cx),
                        _ => panic!("unknown exit mode: {mode}"),
                    }
                });
            })
            .detach();
        });
    std::fs::write(dir.join("returned"), b"").unwrap();
}
