use super::confirm;
use crate::TermWindow;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::Mux;

pub fn confirm_close_pane(
    pane_id: PaneId,
    mut term: TermWizTerminal,
    mux_window_id: WindowId,
    window: ::window::Window,
) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really kill this pane?", &mut term)? {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            let tab = match mux.get_active_tab_for_window(mux_window_id) {
                Some(tab) => tab,
                None => return,
            };
            if let Some(pane) = mux.get_pane(pane_id) {
                if let Some(domain) = mux.get_domain(pane.domain_id()) {
                    if let Some(tmux) = domain.downcast_ref::<mux::tmux::TmuxDomain>() {
                        if let Err(err) = tmux.kill_pane(pane_id).await {
                            log::error!("failed to close local tmux pane {pane_id}: {err:#}");
                        }
                        return;
                    }
                }
            }
            tab.kill_pane(pane_id);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay_for_pane(window, pane_id);

    Ok(())
}

pub fn confirm_close_tab(
    tab_id: TabId,
    mut term: TermWizTerminal,
    _mux_window_id: WindowId,
    window: ::window::Window,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this tab and all contained panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            if let Some(tab) = mux.get_tab(tab_id) {
                if let Some(pane) = tab.get_active_pane() {
                    if let Some(client_pane) =
                        pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                    {
                        if client_pane.tmux_connection_state().is_some() {
                            client_pane.request_close_remote_tab();
                            return;
                        }
                    }
                    if let Some(domain) = mux.get_domain(pane.domain_id()) {
                        if let Some(tmux) = domain.downcast_ref::<mux::tmux::TmuxDomain>() {
                            if let Err(err) = tmux.close_tab(tab_id).await {
                                log::error!("failed to close local tmux tab {tab_id}: {err:#}");
                            }
                            return;
                        }
                    }
                }
            }
            mux.remove_tab(tab_id);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}

pub fn confirm_close_window(
    mut term: TermWizTerminal,
    mux_window_id: WindowId,
    window: ::window::Window,
    tab_id: TabId,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this window and all contained tabs and panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            mux.kill_window(mux_window_id);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}

pub fn confirm_quit_program(
    mut term: TermWizTerminal,
    window: ::window::Window,
    tab_id: TabId,
) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really Quit WezTerm?", &mut term)? {
        promise::spawn::spawn_into_main_thread(async move {
            use ::window::{Connection, ConnectionOps};
            let con = Connection::get().expect("call on gui thread");
            con.terminate_message_loop();
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}
