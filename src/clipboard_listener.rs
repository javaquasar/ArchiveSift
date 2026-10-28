use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;

use arboard::Clipboard;

const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub enum ClipboardEvent {
    Text(String),
    Error(String),
}

pub fn spawn_clipboard_watcher<F>(sender: SyncSender<ClipboardEvent>, wakeup: F)
where
    F: Fn() + Send + 'static,
{
    thread::spawn(move || {
        let mut clipboard = match Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(error) => {
                publish_event(
                    &sender,
                    &wakeup,
                    ClipboardEvent::Error(format!(
                        "Clipboard watcher failed to initialize: {}",
                        error
                    )),
                );
                return;
            }
        };

        let mut last_seen: Option<(u64, usize)> = None;
        let mut last_error: Option<String> = None;

        loop {
            match clipboard.get_text() {
                Ok(text) => {
                    last_error = None;
                    let fingerprint = clipboard_fingerprint(&text);
                    if last_seen != Some(fingerprint) {
                        last_seen = Some(fingerprint);
                        let event = bounded_clipboard_event(text);
                        if !publish_event(&sender, &wakeup, event) {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let message = format!("Clipboard read failed: {}", error);
                    if last_error.as_deref() != Some(message.as_str()) {
                        last_error = Some(message.clone());
                        if !publish_event(&sender, &wakeup, ClipboardEvent::Error(message)) {
                            return;
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(750));
        }
    });
}

fn clipboard_fingerprint(text: &str) -> (u64, usize) {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    (hasher.finish(), text.len())
}

fn bounded_clipboard_event(text: String) -> ClipboardEvent {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        ClipboardEvent::Error(format!(
            "Clipboard text ignored: {} bytes exceeds the {} byte limit",
            text.len(),
            MAX_CLIPBOARD_TEXT_BYTES
        ))
    } else {
        ClipboardEvent::Text(text)
    }
}

fn publish_event<F>(sender: &SyncSender<ClipboardEvent>, wakeup: &F, event: ClipboardEvent) -> bool
where
    F: Fn(),
{
    if sender.send(event).is_err() {
        return false;
    }
    wakeup();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn published_clipboard_event_wakes_the_ui_after_delivery() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let wakeups = AtomicUsize::new(0);

        assert!(publish_event(
            &sender,
            &|| {
                wakeups.fetch_add(1, Ordering::Relaxed);
            },
            ClipboardEvent::Text("https://demo.test/".to_string()),
        ));

        assert!(matches!(
            receiver.try_recv(),
            Ok(ClipboardEvent::Text(value)) if value == "https://demo.test/"
        ));
        assert_eq!(wakeups.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clipboard_fingerprint_does_not_retain_or_ignore_length() {
        assert_eq!(clipboard_fingerprint("same"), clipboard_fingerprint("same"));
        assert_ne!(
            clipboard_fingerprint("same"),
            clipboard_fingerprint("same ")
        );
    }

    #[test]
    fn oversized_clipboard_text_is_replaced_by_a_small_error_event() {
        let event = bounded_clipboard_event("x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1));
        match event {
            ClipboardEvent::Error(message) => {
                assert!(message.len() < 256);
                assert!(message.contains("ignored"));
            }
            ClipboardEvent::Text(_) => panic!("oversized text must not reach the UI"),
        }
    }
}
