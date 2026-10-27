use gettextrs::gettext;

use crate::config::APP_ID;

#[derive(Debug)]
pub struct Tray {
    pub tx: tokio::sync::mpsc::Sender<TrayMessage>,
}

#[derive(Debug, Clone)]
pub enum TrayMessage {
    OpenWindow,
    Quit,
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = false;

    fn activate(&mut self, _x: i32, _y: i32) {
        _ = self.tx.try_send(TrayMessage::OpenWindow);
    }

    fn icon_theme_path(&self) -> String {
        std::env::current_exe().ok()
            .and_then(|path| Some(path.parent()?.parent()?.join("share/icons/hicolor/scalable/apps")))
            .filter(|path| path.join(format!("{APP_ID}-symbolic.svg")).is_file())
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/icons"))
            .to_string_lossy().into_owned()
    }

    fn id(&self) -> String {
        APP_ID.into()
    }
    fn icon_name(&self) -> String {
        format!("{APP_ID}-symbolic")
    }
    fn title(&self) -> String {
        gettext("Packet Lab")
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: gettext("Open"),
                activate: Box::new(move |this: &mut Self| {
                    _ = this.tx.try_send(TrayMessage::OpenWindow);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: gettext("Exit"),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(move |this: &mut Self| {
                    _ = this.tx.try_send(TrayMessage::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;

    #[test]
    fn left_click_opens_window_and_icon_path_contains_the_named_icon() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut tray = Tray { tx };
        tray.activate(0, 0);
        assert!(matches!(rx.try_recv(), Ok(TrayMessage::OpenWindow)));
        assert!(std::path::Path::new(&tray.icon_theme_path()).join(format!("{}.svg", tray.icon_name())).is_file());
    }
}
