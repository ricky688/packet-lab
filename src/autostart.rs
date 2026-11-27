use std::{io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result, ensure};
use gtk::glib;

use crate::config::APP_ID;

pub fn set_enabled(enabled: bool) -> Result<()> {
    let path = glib::user_config_dir().join("autostart").join(format!("{APP_ID}.desktop"));
    if !enabled {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Remove Packet Lab autostart entry"),
        };
    }

    let appimage = std::env::var_os("APPIMAGE");
    let runtime_option = if appimage.is_some() { " --appimage-extract-and-run" } else { "" };
    let launcher = appimage
        .or_else(|| std::env::var_os("PACKET_LAB_LAUNCHER"))
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    ensure!(launcher.is_absolute() && launcher.is_file(), "Packet Lab launcher is unavailable");
    let launcher = launcher.to_str().context("Launcher path is not UTF-8")?;
    ensure!(!launcher.chars().any(char::is_control), "Launcher path contains control characters");
    // A Desktop Entry string is unescaped before its Exec quotes are parsed.
    let launcher = launcher
        .replace('\\', r"\\\\")
        .replace('"', "\\\\\"")
        .replace('`', "\\\\`")
        .replace('$', r"\\$")
        .replace('%', "%%");
    let entry = format!("[Desktop Entry]\nType=Application\nName=Packet Lab\nExec=\"{launcher}\"{runtime_option} --background\nIcon={APP_ID}\nTerminal=false\nStartupNotify=false\n");
    std::fs::create_dir_all(path.parent().unwrap())?;
    if std::fs::read(&path).as_deref().ok() != Some(entry.as_bytes()) {
        glib::file_set_contents(&path, entry.as_bytes())?;
    }
    Ok(())
}
