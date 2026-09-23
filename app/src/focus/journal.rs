//! Small, asynchronous focus diagnostics. No console titles or contents.
use std::io::Write;
use std::sync::{
    OnceLock,
    mpsc::{SyncSender, sync_channel},
};

static JOURNAL: OnceLock<SyncSender<serde_json::Value>> = OnceLock::new();

pub(super) fn record(target: &super::FocusTarget, report: &super::Report) {
    let sender = JOURNAL.get_or_init(|| {
        let (send, receive) = sync_channel::<serde_json::Value>(64);
        std::thread::spawn(move || {
            let Some(root) = crate::paths::root() else {
                return;
            };
            let path = root.join("focus-events.log");
            for value in receive {
                let Ok(mut bytes) = serde_json::to_vec(&value) else {
                    continue;
                };
                bytes.push(b'\n');
                if bytes.len() > 8192 {
                    continue;
                }
                let _ = std::fs::create_dir_all(&root);
                let truncate = std::fs::metadata(&path)
                    .is_ok_and(|m| m.len() + bytes.len() as u64 > 256 * 1024);
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(!truncate)
                    .truncate(truncate)
                    .open(&path)
                {
                    let _ = file.write_all(&bytes);
                }
            }
        });
        send
    });
    let _ = sender.try_send(serde_json::json!({
        "t":crate::now_ms(), "source":target.source, "session_id":target.session_id,
        "terminal_id":target.terminal_id, "ancestors":target.ancestors.iter().map(|a| a.pid).collect::<Vec<_>>(),
        "raised":report.raised, "target_selected":report.target_selected, "method":report.method,
        "reason":(!report.target_selected).then_some(&report.detail)
    }));
}
