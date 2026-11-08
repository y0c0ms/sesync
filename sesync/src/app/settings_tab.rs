//! The Settings tab: where files other machines and the phone send arrive. The folder is `inbox` in
//! `<config>/transfer.json` (`bridge::transfer`); the bridge reads it on every upload, so a change here applies at
//! once, without restarting the bridge.

use super::{App, Mode};
use crate::bridge::transfer;
use std::path::PathBuf;

/// The Settings tab's state.
#[derive(Default)]
pub struct SettingsTab {
    /// The config directory of this run; the tab is inert without one.
    config_dir: Option<PathBuf>,
}

impl App {
    /// The folder files arrive in, as the bridge reads it now. Empty when this run has no config directory.
    pub fn inbox_folder(&self) -> String {
        match &self.settings_tab.config_dir {
            // The bridge's state directory is `<config>/bridge`; `inbox_dir` reads the settings from its parent.
            Some(config) => transfer::inbox_dir(&config.join("bridge")).to_string_lossy().into_owned(),
            None => String::new(),
        }
    }

    /// Whether the folder is the default, not one the user chose.
    pub fn inbox_is_default(&self) -> bool {
        match &self.settings_tab.config_dir {
            Some(config) => !transfer::is_configured(&config.join("bridge")),
            None => true,
        }
    }

    /// Start editing the folder (Enter on the Settings tab): the buffer starts at the current folder.
    pub(super) fn begin_inbox_edit(&mut self) {
        if self.settings_tab.config_dir.is_none() {
            self.fail("this run has no configuration directory to keep the setting in".to_string());
            return;
        }
        self.input = self.inbox_folder();
        self.mode = Mode::Settings;
        self.say("Type the folder files arrive in; Enter saves, Esc cancels.");
    }

    /// Keys while the folder is being typed.
    pub(super) fn handle_settings(&mut self, ev: super::Ev) {
        use super::Ev;
        match ev {
            Ev::Char(c) => self.input.push(c),
            Ev::Backspace => {
                self.input.pop();
            }
            Ev::Esc => {
                self.mode = Mode::Browse;
                self.input.clear();
                self.say("folder unchanged");
            }
            Ev::Enter => {
                let Some(config) = self.settings_tab.config_dir.clone() else {
                    self.mode = Mode::Browse;
                    return;
                };
                match transfer::set_inbox(&config, &self.input) {
                    Ok(dir) => {
                        self.mode = Mode::Browse;
                        self.input.clear();
                        self.say(format!("files now arrive in {}", dir.display()));
                    }
                    // A refused folder keeps the input open so it can be fixed.
                    Err(e) => self.fail(format!("refused: {e}")),
                }
            }
            _ => {}
        }
    }

    /// Whether the Settings tab is the one shown.
    pub fn on_settings(&self) -> bool {
        false
    }

    /// The config directory of this run, for the Settings tab.
    pub(super) fn set_settings_config_dir(&mut self, dir: PathBuf) {
        self.settings_tab.config_dir = Some(dir);
    }
}
