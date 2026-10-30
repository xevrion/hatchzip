//! Check Slint's automatic locale selection in fresh processes on Linux.
//! Windows and macOS read the system settings rather than these variables.
use hatchzip_gui::MainWindow;
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};

#[test]
fn system_locale_selects_hindi_or_falls_back_to_english() {
    if std::env::consts::OS != "linux" {
        return;
    }
    if let Ok(expected) = std::env::var("HATCHZIP_LOCALE_CASE") {
        slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
            mock_time: false,
            threading: true,
            renderer_name: Some("software".into()),
        })))
        .unwrap();
        let ui = MainWindow::new().unwrap();
        match expected.as_str() {
            "hi" => assert!(ui.get_formats().contains("समर्थित फ़ॉर्मैट")),
            "fallback" => assert!(ui.get_formats().starts_with("Or drag an archive")),
            other => panic!("unexpected test case: {other}"),
        }
        return;
    }
    for (locale, expected) in [("hi_IN.UTF-8", "hi"), ("fr_FR.UTF-8", "fallback")] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "system_locale_selects_hindi_or_falls_back_to_english",
            ])
            .env("HATCHZIP_LOCALE_CASE", expected)
            .env("LC_ALL", locale)
            .env_remove("LANGUAGE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{locale}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
