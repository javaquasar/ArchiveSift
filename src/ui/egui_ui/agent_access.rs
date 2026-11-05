use crate::ui::UiCommand;
use eframe::egui;
use zeroize::Zeroizing;

#[derive(Default)]
pub(super) struct AgentAccessPanel {
    pub enabled: bool,
    pub downloads: bool,
    pub busy: bool,
    pub requests: u64,
    pub message: Option<String>,
    secret: Option<Zeroizing<String>>,
    connection: Option<String>,
}

impl AgentAccessPanel {
    pub fn installed(&mut self, secret: Zeroizing<String>, connection: String) {
        self.enabled = true;
        self.busy = false;
        self.secret = Some(secret);
        self.connection = Some(connection);
        self.message = Some(
            "Connected locally. Copy the session token once into the agent host's secret facility."
                .into(),
        );
    }
    pub fn disabled(&mut self) {
        *self = Self::default();
        self.message = Some("Agent access is off. Accepted downloads can finish normally.".into());
    }
    pub fn failed(&mut self, message: &str) {
        self.disabled();
        self.message = Some(message.into());
    }
    pub fn render(&mut self, ui: &mut egui::Ui) -> Option<UiCommand> {
        let mut command = None;
        ui.group(|ui| {
            ui.heading("Agent access · MCP");
            ui.small("Local access is off by default and ends when the application closes. Only this panel can grant access.");
            if ui.checkbox(&mut self.enabled,"Allow agents to use this application").changed() {
                self.secret=None;self.busy=self.enabled;
                if !self.enabled {self.downloads=false;}
                command=Some(UiCommand::SetAgentAccess{enabled:self.enabled,allow_downloads:self.downloads,rotate:false});
            }
            if self.enabled { ui.add_enabled_ui(!self.busy,|ui| {
                if ui.checkbox(&mut self.downloads,"Allow queue changes and downloads").changed() {
                    command=Some(UiCommand::SetAgentAccess{enabled:true,allow_downloads:self.downloads,rotate:false});
                }
                ui.small("Read access: image search, torrent cards, parser status and queue state. Downloads use the current application destination and settings.");
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.secret.is_some(),egui::Button::new("Copy session token")).clicked() {
                        if let Some(secret)=self.secret.take(){ui.ctx().copy_text(secret.to_string());}
                        self.message=Some("Token copied. Set ARCHIVESIFT_CONTROL_TOKEN in the agent host; it is not saved in the app settings.".into());
                    }
                    if ui.button("Create new token").clicked() {
                        self.secret=None;self.busy=true;
                        command=Some(UiCommand::SetAgentAccess{enabled:true,allow_downloads:self.downloads,rotate:true});
                    }
                    if ui.add_enabled(self.connection.is_some(),egui::Button::new("Copy MCP configuration")).clicked() {
                        if let Some(connection)=&self.connection {ui.ctx().copy_text(connection.clone());}
                    }
                });
                ui.small(format!("Authenticated requests: {}. Activity is recorded in the application log without arguments or credentials.",self.requests));
            }); }
            if self.busy {ui.spinner();ui.label("Starting local agent access…");}
            if let Some(message)=&self.message{ui.label(message);}
        });
        command
    }
}
