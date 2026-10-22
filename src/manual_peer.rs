use std::net::SocketAddrV4;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::glib;

use crate::{objects, widgets, window::Window};

fn parse_address(value: &str) -> Option<SocketAddrV4> {
    let addr: SocketAddrV4 = value.trim().parse().ok()?;
    (addr.port() != 0 && (addr.ip().is_private() || addr.ip().is_link_local())).then_some(addr)
}

impl Window {
    pub(crate) fn present_manual_peer_dialog(&self) {
        let entry = gtk::Entry::builder()
            .placeholder_text("192.168.1.2:53601")
            .activates_default(true)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label(&gettext(
            "Device IPv4 address and port",
        ))]);
        let error = gtk::Label::builder()
            .label(gettext(
                "Enter a local IPv4 address and a port between 1 and 65535.",
            ))
            .wrap(true)
            .visible(false)
            .css_classes(["error"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.append(&entry);
        content.append(&error);
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Connect by IP"))
            .body(gettext("Use the address and Quick Share port confirmed on the other device. Keep its receive screen open and check the pairing code before accepting."))
            .extra_child(&content)
            .build();
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("connect", &gettext("Connect")),
        ]);
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("connect"));
        dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("connect", false);
        entry.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |entry| {
                let valid = parse_address(&entry.text()).is_some();
                dialog.set_response_enabled("connect", valid);
                error.set_visible(!entry.text().is_empty() && !valid);
            }
        ));
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response != "connect" {
                        return;
                    }
                    let Some(addr) = parse_address(&entry.text()) else {
                        return;
                    };
                    glib::spawn_future_local(glib::clone!(
                        #[weak]
                        window,
                        async move {
                            let imp = window.imp();
                            let id = addr.to_string();
                            let mut cache = imp.send_transfers_id_cache.lock().await;
                            let recipient = if let Some(existing) = cache.get(&id) {
                                existing.clone()
                            } else {
                                let recipient = objects::SendRequestState::new();
                                recipient.set_endpoint_info(objects::EndpointInfo(
                                    rqs_lib::EndpointInfo {
                                        fullname: String::new(),
                                        id: id.clone(),
                                        name: Some(format!(
                                            "{} ({})",
                                            gettext("Manual device"),
                                            addr.ip()
                                        )),
                                        ip: Some(addr.ip().to_string()),
                                        port: Some(addr.port().to_string()),
                                        present: Some(true),
                                        ..Default::default()
                                    },
                                ));
                                imp.recipient_model.insert(0, &recipient);
                                cache.insert(id, recipient.clone());
                                recipient
                            };
                            drop(cache);
                            if recipient.is_active() {
                                return;
                            }
                            // Discovery removal may have cleared a cached endpoint's address.
                            let mut endpoint = recipient.endpoint_info();
                            endpoint.id = addr.to_string();
                            endpoint.ip = Some(addr.ip().to_string());
                            endpoint.port = Some(addr.port().to_string());
                            endpoint.present = Some(true);
                            if endpoint.name.as_ref().is_none_or(|name| name.is_empty()) {
                                endpoint.name =
                                    Some(format!("{} ({})", gettext("Manual device"), addr.ip()));
                            }
                            recipient.set_endpoint_info(endpoint);
                            widgets::emit_send_request(&window, &recipient);
                        }
                    ));
                }
            ),
        );
        dialog.present(Some(self));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_address;

    #[test]
    fn accepts_explicit_local_ipv4_and_port() {
        for value in [
            "192.168.1.2:53601",
            "10.0.0.2:1",
            "172.16.0.2:65535",
            "169.254.1.2:1234",
            " 192.168.1.2:53601 ",
        ] {
            assert!(parse_address(value).is_some(), "{value}");
        }
    }

    #[test]
    fn rejects_missing_ports_dns_and_nonlocal_targets() {
        for value in [
            "",
            "192.168.1.2",
            "192.168.1.2:0",
            "192.168.1.2:65536",
            "phone.local:53601",
            "8.8.8.8:443",
            "127.0.0.1:1234",
            "0.0.0.0:1234",
            "224.0.0.251:5353",
            "[::1]:53601",
        ] {
            assert!(parse_address(value).is_none(), "{value}");
        }
    }
}
