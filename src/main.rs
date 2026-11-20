#![windows_subsystem = "windows"]
use switchboard::{app, store::Store, temporary};

fn start() -> anyhow::Result<()> {
    use anyhow::Context;
    let s = Store::local()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => app::run(s),
        [arg] if arg == "--temporary" => temporary::run(&s),
        [arg, id] if arg == "--open" => {
            let id = uuid::Uuid::parse_str(id).context("Not a valid profile shortcut.")?;
            if s.read()?.profile(id)?.trashed {
                let result = rfd::MessageDialog::new()
                    .set_title("Restore profile?")
                    .set_description("This profile is in Trash. Restore it and open Edge?")
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();
                if result != rfd::MessageDialogResult::Yes {
                    return Ok(());
                }
                s.trash(id, false)?;
            }
            if let Some(job) = s.launch(id)? {
                while job.active()? {
                    std::thread::sleep(std::time::Duration::from_millis(750));
                }
            }
            Ok(())
        }
        _ => anyhow::bail!("Usage: switchboard.exe [--open <profile-id> | --temporary]"),
    }
}
fn main() {
    if let Err(e) = start() {
        rfd::MessageDialog::new()
            .set_title("Switchboard")
            .set_level(rfd::MessageLevel::Error)
            .set_description(format!("{e:#}"))
            .show();
        std::process::exit(1);
    }
}
