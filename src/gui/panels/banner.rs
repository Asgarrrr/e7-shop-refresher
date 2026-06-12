use egui_phosphor::regular as icon;

use crate::gui::app::{ShopGui, palette};

pub(in crate::gui) fn draw_update_banner(ui: &mut egui::Ui, gui: &mut ShopGui) {
    let Some(tag) = gui.update_status.snapshot() else {
        return;
    };
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(palette::ACCENT, icon::ARROW_FAT_LINES_UP);
        ui.colored_label(palette::ACCENT, format!("{tag} available"));

        match gui.update_progress.as_ref().map(|p| &p.state) {
            None => {
                if ui
                    .button("Download & restart")
                    .on_hover_text(
                        "Downloads the new binary from GitHub, verifies its \
                         SHA256, swaps it in place and restarts. Your config \
                         and templates are untouched.",
                    )
                    .clicked()
                {
                    gui.start_auto_update();
                }
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new("Release notes")
                            .color(palette::TEXT_DIM)
                            .underline(),
                    )
                    .sense(egui::Sense::click()),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    open_url(crate::update_check::RELEASES_PAGE_URL);
                }
            }
            Some(crate::gui::app::UpdateState::Downloading { bytes, total }) => {
                ui.colored_label(palette::TEXT_DIM, format_download_progress(*bytes, *total));
            }
            Some(crate::gui::app::UpdateState::Verifying) => {
                ui.colored_label(palette::TEXT_DIM, "Verifying…");
            }
            Some(crate::gui::app::UpdateState::Installing) => {
                ui.colored_label(palette::TEXT_DIM, "Installing — app will restart…");
            }
            Some(crate::gui::app::UpdateState::Failed(msg)) => {
                ui.colored_label(palette::ERROR, format!("Update failed: {msg}"));
                if ui
                    .small_button("Retry")
                    .on_hover_text("Try the download again")
                    .clicked()
                {
                    gui.update_progress = None;
                    gui.start_auto_update();
                }
            }
        }
    });
    ui.add_space(4.0);
}

fn format_download_progress(bytes: u64, total: Option<u64>) -> String {
    fn mb(n: u64) -> f32 {
        n as f32 / (1024.0 * 1024.0)
    }
    match total {
        Some(t) if t > 0 => format!("Downloading… {:.1} / {:.1} MB", mb(bytes), mb(t)),
        _ => format!("Downloading… {:.1} MB", mb(bytes)),
    }
}

/// Opens a URL in the default browser, **dropping our admin token** on
/// the way out.
///
/// The app embeds an admin manifest, so any child we spawn directly
/// (`cmd /C start`, plain `ShellExecuteW`, the `opener` crate) inherits
/// our elevated token. Chrome refuses to run elevated and Edge / Firefox
/// mishandle profiles in that state, so the click silently appears broken.
///
/// `IShellDispatch2::ShellExecute` resolves the verb inside Explorer
/// (CLSCTX_LOCAL_SERVER, medium integrity), so the browser inherits
/// Explorer's token rather than ours.
///
/// Spawned on a detached thread because COM init/teardown shouldn't run
/// on the GUI thread (eframe owns the apartment there for clipboard/DnD).
fn open_url(url: &str) {
    let url = url.to_owned();
    let spawn = std::thread::Builder::new()
        .name("open-url".into())
        .spawn(move || {
            if let Err(e) = open_url_via_shell(&url) {
                tracing::warn!(error = %e, url, "failed to open URL");
            }
        });
    if let Err(e) = spawn {
        tracing::warn!(error = %e, "failed to spawn open-url thread");
    }
}

fn open_url_via_shell(url: &str) -> windows::core::Result<()> {
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::IShellDispatch2;
    use windows::core::{BSTR, GUID};

    // CLSID_Shell — not exposed as a named constant by the windows crate.
    const CLSID_SHELL: GUID = GUID::from_u128(0x13709620_C279_11CE_A49E_444553540000);

    unsafe {
        // S_FALSE = already initialized on this thread; RPC_E_CHANGED_MODE =
        // different model in use. Both still need a balancing CoUninitialize.
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let inited = hr.is_ok();

        let result: windows::core::Result<()> = (|| {
            let shell: IShellDispatch2 = CoCreateInstance(&CLSID_SHELL, None, CLSCTX_LOCAL_SERVER)?;
            let file = BSTR::from(url);
            let empty = VARIANT::default();
            shell.ShellExecute(&file, &empty, &empty, &empty, &empty)
        })();

        if inited {
            CoUninitialize();
        }
        result
    }
}
