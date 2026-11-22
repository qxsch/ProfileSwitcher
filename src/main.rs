#![windows_subsystem = "windows"]
use switchboard::{app, jump_list, store::Store, temporary};

fn start() -> anyhow::Result<()> {
    use anyhow::Context;
    jump_list::initialize()?;
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
            let job = s.launch(id)?;
            if let Err(error) = jump_list::refresh(&s) {
                rfd::MessageDialog::new()
                    .set_title("Switchboard")
                    .set_level(rfd::MessageLevel::Warning)
                    .set_description(format!(
                        "The profile opened, but taskbar recent profiles could not be updated:\n{error:#}"
                    ))
                    .show();
            }
            if let Some(job) = job {
                let name = s.read()?.profile(id)?.name.clone();
                let mut brand = switchboard::native::WindowBrand::new(id, &name, &s.icon_path(id))?;
                while job.active()? {
                    if brand.icon_changed()
                        && let Ok(name) = s
                            .read()
                            .and_then(|mut c| c.profile(id).map(|p| p.name.clone()))
                    {
                        brand.rename(&name);
                    }
                    // Branding is cosmetic; a failed pass must not end the session watcher.
                    let _ = brand.apply(&job);
                    std::thread::sleep(std::time::Duration::from_millis(200));
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
