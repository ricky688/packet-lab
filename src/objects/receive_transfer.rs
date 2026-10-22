use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self};

use crate::{objects, utils};

#[derive(Debug, Clone, PartialEq, glib::Boxed)]
#[boxed_type(name = "ConsentStateBoxed", nullable)]
pub enum UserAction {
    ConsentAccept,
    ConsentDecline,
    TransferCancel,
}

pub mod imp {
    use std::{cell::RefCell, rc::Rc};

    use gtk::glib::Properties;

    use super::*;

    #[derive(Debug, Default, Properties)]
    #[properties(wrapper_type = super::ReceiveTransferState)]
    pub struct ReceiveTransferState {
        pub eta: Rc<RefCell<utils::DataTransferEta>>,
        #[property(get, set, nullable)]
        user_action: RefCell<Option<UserAction>>,
        #[property(get, set)]
        event: RefCell<Option<objects::ChannelMessage>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ReceiveTransferState {
        const NAME: &'static str = "PacketReceiveTransferState";
        type Type = super::ReceiveTransferState;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ReceiveTransferState {}
}

glib::wrapper! {
    pub struct ReceiveTransferState(ObjectSubclass<imp::ReceiveTransferState>);
}

impl Default for ReceiveTransferState {
    fn default() -> Self {
        glib::Object::builder().build()
    }
}

impl ReceiveTransferState {
    pub fn new(msg: &objects::ChannelMessage) -> Self {
        let obj: Self = Default::default();
        obj.set_event(msg);

        obj
    }
}
